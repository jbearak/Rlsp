//
// cross_file/mod.rs
//
// Cross-file awareness for Rlsp
//

pub(crate) mod binding;
pub mod cache;
pub mod config;
pub mod dependency;
pub mod directive;
pub mod file_cache;
pub mod parent_resolve;
pub mod path_resolve;
pub mod revalidation;
pub mod scope;
pub(crate) mod shiny;
pub mod source_detect;
pub mod standalone_cache;
pub(crate) mod static_path;
pub mod tar_source;
pub(crate) mod targets;
pub mod types;

#[cfg(test)]
mod property_tests;

#[cfg(test)]
pub mod integration_tests;

pub use cache::*;
pub use config::*;
pub use dependency::*;
pub use directive::*;
pub use file_cache::*;
pub use parent_resolve::*;
pub use path_resolve::*;
pub use revalidation::*;
pub use scope::*;
pub use source_detect::*;
pub use tar_source::*;
pub use types::*;

/// Extract cross-file metadata from R source by combining directive parsing with AST-detected sources and package facts.
///
/// Directive-derived `source` entries take precedence over AST-detected `source()` calls when they occur on the same line. When a thread-local parser is available the function also detects lexical package loads (`library()`, `require()`, and `loadNamespace()`) and file-level targets worker-package declarations; if parsing fails those AST-derived detections are skipped.
///
/// # Returns
///
/// A `CrossFileMetadata` containing collected sources, backward declarations, lexical package loads, and targets pipeline packages. Positional collections are sorted by document order (line, column).
///
/// # Examples
///
/// ```no_run
/// use raven::cross_file;
///
/// let content = r#"
/// #> sourceline: helper.R
/// source('other.R')
/// library(pkg)
/// "#;
/// let meta = cross_file::extract_metadata(content);
/// assert!(meta.sources.len() >= 1);
/// assert!(meta.library_calls.iter().any(|lc| lc.package == "pkg"));
/// ```
pub fn extract_metadata(content: &str) -> CrossFileMetadata {
    let tree = crate::parser_pool::with_parser(|parser| parser.parse(content, None));
    extract_metadata_with_tree(content, tree.as_ref())
}

/// The R-analysis view of `content` for the file identified by `path_or_uri`:
/// the geometry-preserving [`crate::chunks::mask_to_r`] mask for R Markdown /
/// Quarto documents (`.Rmd` / `.Rmarkdown` / `.qmd`), and the raw `content`
/// borrowed unchanged for everything else.
///
/// This is the single place that pairs path-based classification with masking
/// for closed-file / on-demand-indexing call sites that only have a path and a
/// byte string (no constructed `Document`). Open documents should prefer the
/// already-masked [`crate::state::Document::analysis_text`] instead of
/// re-masking here.
///
/// Returns a `Cow` so the plain-R case (the overwhelming majority) borrows
/// without allocating.
pub fn analysis_text_for_path<'a>(
    path_or_uri: &str,
    content: &'a str,
) -> std::borrow::Cow<'a, str> {
    analysis_text_for_kind(crate::chunks::classify_chunk_document(path_or_uri), content)
}

/// The R-analysis view of `content` for an already-classified document: the
/// geometry-preserving [`crate::chunks::mask_to_r`] mask for
/// [`ChunkKind::Rmd`](crate::chunks::ChunkKind::Rmd), the raw `content`
/// borrowed unchanged for [`ChunkKind::R`](crate::chunks::ChunkKind::R).
///
/// Use this when the caller already knows the kind from the editor's
/// `languageId`-then-URI classification (e.g. `did_open`, where path-based
/// classification would mis-handle untitled `.Rmd`/`.qmd` buffers, #343).
/// [`analysis_text_for_path`] is the path-classified convenience wrapper.
pub fn analysis_text_for_kind(
    chunk_kind: crate::chunks::ChunkKind,
    content: &str,
) -> std::borrow::Cow<'_, str> {
    match chunk_kind {
        crate::chunks::ChunkKind::Rmd => std::borrow::Cow::Owned(crate::chunks::mask_to_r(content)),
        crate::chunks::ChunkKind::R => std::borrow::Cow::Borrowed(content),
    }
}

/// The `Option`-returning sibling of [`analysis_text_for_kind`] for callers that
/// store `masked_text: Option<String>` (an open document's analysis text is the
/// masked string for Rmd/Quarto, or `None` to mean "use the raw text as-is").
///
/// Returns `Some(masked)` for [`ChunkKind::Rmd`](crate::chunks::ChunkKind::Rmd)
/// (the geometry-preserving [`crate::chunks::mask_to_r`] mask) and `None` for
/// [`ChunkKind::R`](crate::chunks::ChunkKind::R), where analysis text equals raw
/// text. This is the single masking chokepoint for `masked_text` fields:
/// [`crate::state::Document`] and
/// [`crate::open_document_store::OpenDocumentStore`] both
/// route through it so their analysis views can never diverge.
pub(crate) fn masked_analysis_text(
    chunk_kind: crate::chunks::ChunkKind,
    text: &str,
) -> Option<String> {
    match analysis_text_for_kind(chunk_kind, text) {
        std::borrow::Cow::Owned(masked) => Some(masked),
        std::borrow::Cow::Borrowed(_) => None,
    }
}

/// Extract cross-file metadata from an already-derived R analysis view.
///
/// `analysis` must be raw R for [`ChunkKind::R`](crate::chunks::ChunkKind::R)
/// and the geometry-preserving report mask for
/// [`ChunkKind::Rmd`](crate::chunks::ChunkKind::Rmd). Report chunks may read
/// pipeline targets, but target constructors and nested report factories do not
/// register pipeline declarations or links while the report renders. Keeping
/// that post-extraction rule here gives open and closed documents identical
/// target-authority semantics without re-masking an open document's analysis
/// text.
pub(crate) fn extract_metadata_from_analysis_for_kind(
    chunk_kind: crate::chunks::ChunkKind,
    analysis: &str,
) -> CrossFileMetadata {
    let tree = crate::parser_pool::with_parser(|parser| parser.parse(analysis, None));
    extract_metadata_with_tree_from_analysis_for_kind(chunk_kind, analysis, tree.as_ref())
}

/// Tree-reusing sibling of [`extract_metadata_from_analysis_for_kind`].
///
/// The supplied tree must have been parsed from `analysis`. Keeping report
/// authority filtering after the shared tree-aware extractor lets resync and
/// indexing paths avoid a second parse without bypassing the Rmd/Qmd rule.
pub(crate) fn extract_metadata_with_tree_from_analysis_for_kind(
    chunk_kind: crate::chunks::ChunkKind,
    analysis: &str,
    tree: Option<&tree_sitter::Tree>,
) -> CrossFileMetadata {
    let mut metadata = extract_metadata_with_tree(analysis, tree);
    if chunk_kind == crate::chunks::ChunkKind::Rmd {
        metadata.target_declarations.clear();
        metadata.tarchetypes_document_links.clear();
    }
    metadata
}

/// Extract cross-file metadata from `content` using an already-resolved chunk
/// kind, masking R Markdown / Quarto prose first so directives, `source()`
/// calls, and `library()` calls are taken from R chunk bodies only (never from
/// prose or YAML front matter).
///
/// For non-Rmd files this is identical to [`extract_metadata`]. Use this at any
/// site that extracts metadata from a file's *raw* content when the caller has
/// a live or persisted editor-language classification. That classification must
/// outrank path classification for extension-mismatched Rmd/Quarto files
/// (issue #563).
pub fn extract_metadata_for_kind(
    chunk_kind: crate::chunks::ChunkKind,
    content: &str,
) -> CrossFileMetadata {
    let analysis = analysis_text_for_kind(chunk_kind, content);
    extract_metadata_from_analysis_for_kind(chunk_kind, &analysis)
}

/// Extract cross-file metadata from `content`, masking R Markdown / Quarto
/// prose first so directives, `source()` calls, and `library()` calls are
/// taken from R chunk bodies only (never from prose or YAML front matter).
///
/// For non-Rmd files this is identical to [`extract_metadata`]. Use this at any
/// site that extracts metadata from a path-identified file's *raw* content
/// (file-cache fallbacks and on-demand indexing) so that
/// `.Rmd` / `.Rmarkdown` / `.qmd` files contribute outgoing edges from their
/// chunks rather than spurious prose-derived ones (issue #343). State-aware
/// closed-file callers should prefer
/// [`WorldState::extract_metadata_for_uri`](crate::state::WorldState::extract_metadata_for_uri)
/// so persisted editor-language chunk classification can outrank the path
/// (issue #563).
pub fn extract_metadata_for_path(path_or_uri: &str, content: &str) -> CrossFileMetadata {
    extract_metadata_for_kind(crate::chunks::classify_chunk_document(path_or_uri), content)
}

/// Extract cross-file metadata using a pre-parsed tree when available.
///
/// This avoids redundant parsing when the caller already has a tree-sitter `Tree`.
pub fn extract_metadata_with_tree(
    content: &str,
    tree: Option<&tree_sitter::Tree>,
) -> CrossFileMetadata {
    log::trace!(
        "Extracting cross-file metadata from content ({} bytes)",
        content.len()
    );

    // Parse directives first
    let mut meta = directive::parse_directives(content);

    // Parse AST for source() calls and library() calls using provided tree
    if let Some(tree) = tree {
        let detected = source_detect::detect_source_calls(tree, content);

        // Merge detected source() calls with directive sources
        // Directive sources take precedence (Requirement 6.8)
        for source in detected {
            // Check if there's already a directive at the same line
            let has_directive = meta
                .sources
                .iter()
                .any(|s| s.is_directive && s.line == source.line);
            if !has_directive {
                meta.sources.push(source);
            }
        }

        // Sort by line number for consistent ordering
        meta.sources.sort_by_key(|s| (s.line, s.column));

        // Detect lexical package loads, targets worker-package declarations,
        // and static source-batch requests with one shared lazy binding table.
        let (
            mut library_calls,
            targets_pipeline_packages,
            tar_source_requests,
            list_files_source_requests,
            targets_metadata,
        ) = source_detect::detect_library_and_tar_source_requests(tree, content);
        // Sort by line/column for document order (Requirement 1.8)
        library_calls.sort_by_key(|lc| (lc.line, lc.column));
        meta.library_calls = library_calls;
        meta.targets_pipeline_packages = targets_pipeline_packages;
        meta.tar_source_requests = tar_source_requests;
        meta.list_files_source_requests = list_files_source_requests;
        meta.target_declarations = targets_metadata.declarations;
        meta.target_references = targets_metadata.references;
        meta.tarchetypes_document_links = targets_metadata.document_links;
        meta.namespace_references = source_detect::detect_namespace_references(tree, content);

        // box::use() imports and box::export() / #' @export interface (#662).
        // Surface parse only; path resolution and scope consumption live in
        // `crate::box_use` and are applied by downstream cross-file phases.
        meta.box_imports = crate::box_use::detect_box_imports(tree, content);
        meta.box_exports = crate::box_use::parse_box_exports(tree, content);
        meta.import_calls = crate::import_pkg::detect::detect_import_calls(tree, content);
    } else {
        log::warn!("Failed to parse R code with tree-sitter during metadata extraction");
    }

    log::trace!(
        "Metadata extraction complete: {} total sources ({} from directives, {} from AST), {} backward directives, {} library calls, {} targets pipeline packages",
        meta.sources.len(),
        meta.sources.iter().filter(|s| s.is_directive).count(),
        meta.sources.iter().filter(|s| !s.is_directive).count(),
        meta.sourced_by.len(),
        meta.library_calls.len(),
        meta.targets_pipeline_packages.len()
    );

    meta
}

/// Persist local `{box}` module resolution outcomes into metadata in tests and
/// box-only helpers that have no workspace context. Production mixed-dialect
/// analysis must call [`enrich_selective_import_resolutions`] so `{import}` keeps
/// its workspace-root fallback.
pub(crate) fn enrich_box_import_resolutions(
    meta: &mut CrossFileMetadata,
    importing_uri: &tower_lsp::lsp_types::Url,
) {
    enrich_selective_import_resolutions(meta, importing_uri, None);
}

/// Enrich both selective-import dialects, supplying the workspace root needed
/// by `{import}`'s normal forward fallback tiers.
pub(crate) fn enrich_selective_import_resolutions(
    meta: &mut CrossFileMetadata,
    importing_uri: &tower_lsp::lsp_types::Url,
    workspace_root: Option<&tower_lsp::lsp_types::Url>,
) {
    crate::box_use::path::enrich_local_imports(importing_uri, &mut meta.box_imports);
    if let Some(exports) = &mut meta.box_exports {
        crate::box_use::path::enrich_local_imports(importing_uri, &mut exports.reexports);
    }
    let metadata_snapshot = meta.clone();
    crate::import_pkg::path::enrich_local_imports(
        importing_uri,
        &metadata_snapshot,
        &mut meta.import_calls,
        workspace_root,
    );
}
