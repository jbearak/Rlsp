//
// state.rs
//
// Copyright (C) 2022-2026 Posit Software, PBC. All rights reserved.
// Modifications copyright (C) 2026 Jonathan Marc Bearak
//

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::content_provider::ContentProvider;
use crate::indentation::IndentationStyle;
use ropey::Rope;
use tower_lsp::lsp_types::TextDocumentContentChangeEvent;

/// Symbol provider configuration
///
/// Controls behavior of document symbol and workspace symbol providers.
///
/// # Configuration Options
///
/// - `workspace_max_results`: Maximum number of symbols returned by workspace symbol queries.
///   Limits results to prevent overwhelming the client with large result sets.
///   Valid range: 100-10000 (values outside this range are clamped).
///
/// # Requirements
///
/// - **11.1**: Default value of 1000 for workspace_max_results
/// - **11.2**: Configurable via `symbols.workspaceMaxResults` initialization option
/// - **11.3**: Valid range 100-10000 with clamping
#[derive(Debug, Clone)]
pub struct SymbolConfig {
    /// Maximum workspace symbol results (default: 1000)
    ///
    /// When a workspace symbol query returns more results than this limit,
    /// the results are truncated. Valid range: 100-10000.
    pub workspace_max_results: usize,

    /// Whether the client supports hierarchical document symbols.
    ///
    /// When true, the document symbol provider returns `DocumentSymbol[]` (nested structure).
    /// When false, it returns `SymbolInformation[]` (flat structure) as fallback.
    ///
    /// This capability is detected from the client's `InitializeParams` at:
    /// `params.capabilities.text_document.document_symbol.hierarchical_document_symbol_support`
    ///
    /// Requirements 1.1, 1.2: Response type selection based on client capability.
    pub hierarchical_document_symbol_support: bool,
}

impl Default for SymbolConfig {
    fn default() -> Self {
        Self {
            workspace_max_results: 1000,
            // Default to false (flat response) until client capability is detected
            hierarchical_document_symbol_support: false,
        }
    }
}

impl SymbolConfig {
    /// Minimum allowed value for workspace_max_results
    pub const MIN_WORKSPACE_MAX_RESULTS: usize = 100;

    /// Maximum allowed value for workspace_max_results
    pub const MAX_WORKSPACE_MAX_RESULTS: usize = 10000;

    /// Default value for workspace_max_results (used in tests)
    #[cfg(test)]
    pub const DEFAULT_WORKSPACE_MAX_RESULTS: usize = 1000;

    /// Create a new SymbolConfig with the given workspace_max_results value.
    ///
    /// The value is clamped to the valid range [100, 10000].
    /// The hierarchical_document_symbol_support field defaults to false.
    ///
    /// # Examples
    ///
    /// ```text
    /// let config = SymbolConfig::with_max_results(500);
    /// assert_eq!(config.workspace_max_results, 500);
    ///
    /// // Values below minimum are clamped
    /// let config = SymbolConfig::with_max_results(50);
    /// assert_eq!(config.workspace_max_results, 100);
    ///
    /// // Values above maximum are clamped
    /// let config = SymbolConfig::with_max_results(20000);
    /// assert_eq!(config.workspace_max_results, 10000);
    /// ```
    pub fn with_max_results(value: usize) -> Self {
        Self {
            workspace_max_results: value.clamp(
                Self::MIN_WORKSPACE_MAX_RESULTS,
                Self::MAX_WORKSPACE_MAX_RESULTS,
            ),
            hierarchical_document_symbol_support: false,
        }
    }
}

/// Completion provider configuration
///
/// Controls behavior of the completion trigger characters and related UI settings.
#[derive(Debug, Clone)]
pub struct CompletionConfig {
    /// Whether typing `(` triggers parameter completions.
    /// When true, `(` is registered as a completion trigger character so that
    /// parameter suggestions appear immediately when opening a function call.
    pub trigger_on_open_paren: bool,
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            trigger_on_open_paren: true,
        }
    }
}

/// Indentation configuration settings.
#[derive(Debug, Clone, Default)]
pub struct IndentationSettings {
    /// Indentation style for R code formatting.
    /// _Requirements: 7.1, 7.2, 7.3, 7.4_
    pub style: IndentationStyle,
}

use tower_lsp::lsp_types::Url;
use tree_sitter::Parser;
use tree_sitter::Tree;

use crate::chunks::{ChunkKind, classify_chunk_document, classify_chunk_document_for};
use crate::content_provider::DefaultContentProvider;
use crate::cross_file::revalidation::{CrossFileDiagnosticsGate, DiagnosticsEpoch};
use crate::cross_file::{
    CrossFileActivityState, CrossFileConfig, CrossFileFileCache, CrossFileRevalidationState,
    CrossFileWorkspaceIndex, DependencyGraph, MetadataCache,
};
use crate::document_store::DocumentStore;
use crate::file_type::{FileType, file_type_from_language_id_or_uri, file_type_from_uri};
use crate::package_library::PackageLibrary;
use crate::parameter_resolver::SignatureCache;
use crate::workspace_index::WorkspaceIndex;

/// A parsed document.
///
/// # Raw text vs. analysis text (Rmd/Quarto invariant)
///
/// A `Document` carries two views of its content that are deliberately kept
/// distinct for R Markdown / Quarto (`ChunkKind::Rmd`) documents:
///
/// * **Raw view** — [`contents`](Document::contents) / [`text()`](Document::text)
///   is *always* the verbatim document as the editor sees it. Anything that
///   operates on the literal source uses this: LSP incremental sync, chunk/fence
///   detection, the Markdown outline, raw snippet retrieval, knit/run-chunk, and
///   semantic-token re-detection of chunks.
///
/// * **Analysis view** — [`tree`](Document::tree) together with
///   [`analysis_text()`](Document::analysis_text). For an Rmd document these are
///   derived from [`crate::chunks::mask_to_r`]: every non-R-chunk-body line is blanked
///   so the R tree-sitter parser sees only real R code. All AST work — parsing,
///   scope/symbol extraction, diagnostics, completion, hover — must pair
///   `tree` with `analysis_text()`, **never** with `text()`. Byte offsets in
///   `tree` index into `analysis_text()`; slicing `text()` with them mis-slices
///   (and can panic on a non-UTF-8 boundary) because the masked text is a
///   different byte string of a different length.
///
/// For plain R / JAGS / Stan documents `analysis_text()` *is* `text()` and the
/// distinction collapses, so behavior-neutral call sites may use either.
///
/// `mask_to_r` is **geometry-preserving**: line count and the line/column of
/// every kept R-body character are identical between the two views. Therefore
/// `Position`/`Range` values (line + UTF-16 column) are interchangeable across
/// the two; only *byte* offsets are view-specific.
#[derive(Clone)]
pub struct Document {
    pub contents: Rope,
    pub tree: Option<Tree>,
    pub loaded_packages: Vec<String>,
    /// Packages named in `data(..., package = "pkg")` calls (issue #429). These
    /// are NOT attached like `library()` packages, but their `data/` enumeration
    /// must be warmed so `data()` alias expansion can resolve the dataset object
    /// names at diagnostics time. Extracted from the same `(tree, text)` pair as
    /// `loaded_packages`. Distinct field because the attachment semantics differ.
    pub data_packages: Vec<String>,
    pub file_type: FileType,
    /// Chunk-detection kind for the outline: `Rmd` for `.Rmd`,
    /// `.Rmarkdown`, and `.qmd` documents and for untitled buffers whose
    /// `languageId` is `rmd`/`quarto`; `R` (i.e. `# %%` cells) otherwise.
    /// Mirrors the client-side classifier in
    /// `editors/vscode/src/chunks/chunk-detector.ts`.
    pub chunk_kind: ChunkKind,
    /// Masked analysis text for Rmd/Quarto documents (`chunks::mask_to_r` of the
    /// raw contents), or `None` for plain R / JAGS / Stan. The `tree` is parsed
    /// from this when present. Kept in sync with `contents` by `apply_change`.
    /// Exposed read-only via [`analysis_text()`](Document::analysis_text).
    masked_text: Option<String>,
    pub version: Option<i32>,
    pub revision: u64,
}

impl Document {
    #[cfg(test)]
    pub fn new(text: &str, version: Option<i32>) -> Self {
        Self::new_with_file_type(text, version, FileType::R)
    }

    pub fn new_with_uri(text: &str, version: Option<i32>, uri: &Url) -> Self {
        // Determine the chunk kind BEFORE parsing: for Rmd/Quarto documents the
        // tree must be parsed from the masked analysis text, not the raw text.
        let chunk_kind = classify_chunk_document(uri.path());
        Self::new_with_kind(text, version, file_type_from_uri(uri), chunk_kind)
    }

    pub fn new_with_language_id(
        text: &str,
        version: Option<i32>,
        uri: &Url,
        language_id: Option<&str>,
    ) -> Self {
        // Determine the chunk kind BEFORE parsing (see `new_with_uri`).
        let chunk_kind = classify_chunk_document_for(language_id, uri.path());
        Self::new_with_kind(
            text,
            version,
            file_type_from_language_id_or_uri(language_id, uri),
            chunk_kind,
        )
    }

    pub fn new_with_file_type(text: &str, version: Option<i32>, file_type: FileType) -> Self {
        // No URI/languageId signal, so default to `# %%` cell detection.
        Self::new_with_kind(text, version, file_type, ChunkKind::R)
    }

    /// Shared constructor: builds the analysis representation up front so the
    /// `tree` is parsed from the right text (masked for Rmd, raw otherwise) and
    /// `loaded_packages` is extracted from the same `(tree, text)` pair.
    fn new_with_kind(
        text: &str,
        version: Option<i32>,
        file_type: FileType,
        chunk_kind: ChunkKind,
    ) -> Self {
        let contents = Rope::from_str(text);
        // Mask Rmd/Quarto bodies so the R parser only sees real R code; plain R
        // (and JAGS/Stan) keep their raw text. Routed through the shared
        // `masked_analysis_text` chokepoint so this and the DocumentStore can
        // never derive divergent analysis views.
        let masked_text = crate::cross_file::masked_analysis_text(chunk_kind, text);
        let analysis_text = masked_text.as_deref().unwrap_or(text);
        let tree = parse_document_text(analysis_text, file_type);
        // Extract from the SAME text the tree was parsed from, so `library()`
        // calls inside chunks are found and prose mentions are not.
        let loaded_packages = extract_loaded_packages(&tree, analysis_text);
        let data_packages = extract_data_packages(&tree, analysis_text);
        Self {
            contents,
            tree,
            loaded_packages,
            data_packages,
            file_type,
            chunk_kind,
            masked_text,
            version,
            revision: 0,
        }
    }

    pub fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
        // Always apply the edit to the RAW contents exactly as before — LSP
        // incremental sync, chunk detection, and the outline all rely on the
        // verbatim source.
        if let Some(range) = change.range {
            let start_line = range.start.line as usize;
            let start_utf16_char = range.start.character as usize;
            let end_line = range.end.line as usize;
            let end_utf16_char = range.end.character as usize;

            let start_line_text = self.contents.line(start_line).to_string();
            let end_line_text = self.contents.line(end_line).to_string();

            let start_char = utf16_offset_to_char_offset(&start_line_text, start_utf16_char);
            let end_char = utf16_offset_to_char_offset(&end_line_text, end_utf16_char);

            let start_idx = self.contents.line_to_char(start_line) + start_char;
            let end_idx = self.contents.line_to_char(end_line) + end_char;

            self.contents.remove(start_idx..end_idx);
            self.contents.insert(start_idx, &change.text);
        } else {
            // Full document sync
            self.contents = Rope::from_str(&change.text);
        }

        self.revision += 1;

        let raw_text = self.contents.to_string();
        // Re-derive the analysis text and parse the tree from it.
        //
        // For Rmd documents we re-mask the FULL new raw text and parse from
        // scratch (no incremental tree-sitter edit). The previous tree's byte
        // offsets reference the OLD masked text, whereas the LSP edit ranges
        // are computed against the raw text — feeding the latter into an
        // incremental `Tree::edit` would corrupt the former. A full reparse per
        // change is acceptable: tree-sitter is fast, and incremental masking is
        // a future optimization. (Plain R already reparses from scratch here,
        // so this is not a regression for the common case.)
        self.masked_text = crate::cross_file::masked_analysis_text(self.chunk_kind, &raw_text);
        // `Some(masked)` for Rmd, `None` for plain R (analysis text == raw text).
        let analysis_text = self.masked_text.as_deref().unwrap_or(&raw_text);

        self.tree = parse_document_text(analysis_text, self.file_type);
        self.loaded_packages = extract_loaded_packages(&self.tree, analysis_text);
        self.data_packages = extract_data_packages(&self.tree, analysis_text);
    }

    pub fn text(&self) -> String {
        self.contents.to_string()
    }

    /// The text the [`tree`](Document::tree) was parsed from: the masked
    /// analysis text for Rmd/Quarto documents, the raw text otherwise.
    ///
    /// Use this — never [`text()`](Document::text) — whenever you slice the
    /// document by byte offsets taken from `tree` (e.g. `node.byte_range()` /
    /// `node.utf8_text(...)`). For plain R / JAGS / Stan this equals `text()`,
    /// so the choice is behavior-neutral there. See the [`Document`] type docs
    /// for the full raw-vs-analysis invariant.
    pub fn analysis_text(&self) -> String {
        match &self.masked_text {
            Some(masked) => masked.clone(),
            None => self.contents.to_string(),
        }
    }

    /// True when the document is an R Markdown / Quarto document.
    ///
    /// For Rmd documents the analysis view (`tree` + `analysis_text()`) is the
    /// geometry-preserving [`crate::chunks::mask_to_r`] mask, so R-language features
    /// (diagnostics, completion, hover, signature help, go-to-definition,
    /// references, folding, selection, on-type formatting, semantic tokens) are
    /// first-class **inside R chunk bodies** and operate on document
    /// coordinates directly.
    ///
    /// Callers still use this flag for two reasons: (1) a few handlers must add
    /// a prose guard — at a prose/YAML position the masked line is blank, which
    /// would otherwise let completion / signature help / on-type formatting
    /// behave like top-level R; guard such positions with
    /// [`crate::chunks::position_in_r_chunk_body`] on the *raw* text. (2) Whole-text
    /// paths that can't consume the R AST (e.g. chunk-aware semantic tokens,
    /// the text-based document outline) branch on it.
    pub fn is_rmd_document(&self) -> bool {
        self.chunk_kind == ChunkKind::Rmd
    }
}

/// Alias map for open file documents whose client URI is not the spelling used
/// by the source graph or closed-file indexes.
///
/// The authoritative document stores stay keyed by the exact URI the client
/// opened so diagnostics can continue to publish to that spelling. This layer
/// only answers boundary questions for equivalent on-disk paths: "is the
/// canonical graph URI open?", "which client URI has the live buffer?", and
/// "which graph URI roots should an edit to this open buffer revalidate?".
///
/// A single open URI can map to more than one canonical URI: the case-corrected
/// spelling of the opened path and, on platforms with symlinks, the symlink
/// target rebased into a registered workspace root when possible. When multiple
/// alias buffers map to the same canonical URI, the oldest remaining alias is
/// used as the live-buffer source; if the canonical URI itself is open, exact
/// document-store lookup wins before this map is consulted.
///
/// Alias graph mirroring follows the same authority rule: an open buffer may
/// update its own graph URI plus only the canonical roots for which
/// [`WorldState::open_document_uri_for_authoritative_uri`] currently returns
/// that buffer's URI. A newer non-authoritative alias can keep its own graph
/// node fresh, but it must not overwrite the canonical node whose live content
/// comes from an older alias or from the exact canonical open URI.
///
/// Package-mode and `.Rprofile` authority reuse this map too. A symlink/case
/// alias may keep diagnostics published to the client URI while package
/// membership checks, package-internal scope injection, self-package NSE policy,
/// parameter/signature package scope, package sibling fanout, and workspace-root
/// `.Rprofile` prelude ownership resolve through the authoritative canonical
/// URI.
#[derive(Debug, Default, Clone)]
pub struct OpenDocumentAliases {
    canonical_to_open: HashMap<Url, Vec<Url>>,
    open_to_canonical: HashMap<Url, Vec<Url>>,
}

impl OpenDocumentAliases {
    pub fn is_empty(&self) -> bool {
        self.canonical_to_open.is_empty()
    }

    pub fn open(&mut self, open_uri: Url, canonical_uris: Vec<Url>) {
        self.close(&open_uri);

        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for canonical_uri in canonical_uris {
            if canonical_uri == open_uri || !seen.insert(canonical_uri.clone()) {
                continue;
            }
            self.canonical_to_open
                .entry(canonical_uri.clone())
                .or_default()
                .push(open_uri.clone());
            unique.push(canonical_uri);
        }

        if !unique.is_empty() {
            self.open_to_canonical.insert(open_uri, unique);
        }
    }

    pub fn close(&mut self, open_uri: &Url) -> Vec<Url> {
        let Some(canonical_uris) = self.open_to_canonical.remove(open_uri) else {
            return Vec::new();
        };

        for canonical_uri in &canonical_uris {
            if let Some(open_uris) = self.canonical_to_open.get_mut(canonical_uri) {
                open_uris.retain(|candidate| candidate != open_uri);
                if open_uris.is_empty() {
                    self.canonical_to_open.remove(canonical_uri);
                }
            }
        }

        canonical_uris
    }

    pub fn canonical_uris_for_open(&self, open_uri: &Url) -> Option<&[Url]> {
        self.open_to_canonical
            .get(open_uri)
            .map(std::vec::Vec::as_slice)
    }

    pub fn open_uris_for_canonical(&self, canonical_uri: &Url) -> Option<&[Url]> {
        self.canonical_to_open
            .get(canonical_uri)
            .map(std::vec::Vec::as_slice)
    }
}

fn filesystem_root_base(path: &Path) -> Option<PathBuf> {
    let mut base = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => base.push(prefix.as_os_str()),
            std::path::Component::RootDir => {
                base.push(component.as_os_str());
                return Some(base);
            }
            std::path::Component::CurDir => return Some(PathBuf::from(".")),
            std::path::Component::ParentDir | std::path::Component::Normal(_) => return Some(base),
        }
    }
    None
}

fn utf16_offset_to_char_offset(line_text: &str, utf16_offset: usize) -> usize {
    let mut utf16_count = 0;
    let mut char_count = 0;

    for ch in line_text.chars() {
        if utf16_count >= utf16_offset {
            return char_count;
        }
        utf16_count += ch.len_utf16();
        char_count += 1;
    }
    char_count
}

fn parse_r_text(text: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_r::LANGUAGE.into()).ok()?;
    parser.parse(text, None)
}

/// Parse `text` (the analysis text — masked for Rmd, raw otherwise) into a
/// tree-sitter tree appropriate for `file_type`. All current file types parse
/// with the R grammar.
fn parse_document_text(text: &str, file_type: FileType) -> Option<Tree> {
    match file_type {
        FileType::R | FileType::Jags | FileType::Stan => parse_r_text(text),
    }
}

fn extract_loaded_packages(tree: &Option<Tree>, text: &str) -> Vec<String> {
    let Some(tree) = tree else {
        return Vec::new();
    };

    let mut packages = Vec::new();
    let mut stack = Vec::new();
    stack.push(tree.root_node());

    while let Some(node) = stack.pop() {
        if node.kind() == "call" {
            // Check if this is a library/require/loadNamespace call
            if let Some(func_node) = node.child_by_field_name("function") {
                let func_text = &text[func_node.byte_range()];

                if func_text == "library" || func_text == "require" || func_text == "loadNamespace"
                {
                    // Extract the first argument
                    if let Some(args_node) = node.child_by_field_name("arguments") {
                        for i in 0..args_node.child_count() {
                            if let Some(child) = args_node.child(i as u32)
                                && child.kind() == "argument"
                                && let Some(value_node) = child.child_by_field_name("value")
                            {
                                let value_text = &text[value_node.byte_range()];
                                let pkg_name =
                                    value_text.trim_matches(|c: char| c == '"' || c == '\'');
                                packages.push(pkg_name.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }

        let child_count = node.child_count();
        for i in (0..child_count).rev() {
            if let Some(child) = node.child(i as u32) {
                stack.push(child);
            }
        }
    }
    packages
}

/// Extract package names from `data(..., package = "pkg")` / `utils::data(...)`
/// calls (issue #429). Mirrors [`extract_loaded_packages`] but targets the
/// `package =` string-literal named argument of `data()` calls so the CLI can
/// warm those packages' `data/` enumeration for alias expansion. Only
/// string-literal `package =` values are collected (a variable package arg
/// can't be resolved statically).
fn extract_data_packages(tree: &Option<Tree>, text: &str) -> Vec<String> {
    let Some(tree) = tree else {
        return Vec::new();
    };

    let mut packages = Vec::new();
    let mut stack = vec![tree.root_node()];

    while let Some(node) = stack.pop() {
        if node.kind() == "call"
            && let Some(func_node) = node.child_by_field_name("function")
        {
            let is_data = match func_node.kind() {
                "identifier" => &text[func_node.byte_range()] == "data",
                "namespace_operator" => func_node
                    .child_by_field_name("rhs")
                    .is_some_and(|rhs| &text[rhs.byte_range()] == "data"),
                _ => false,
            };
            if is_data && let Some(args_node) = node.child_by_field_name("arguments") {
                for i in 0..args_node.child_count() {
                    if let Some(child) = args_node.child(i as u32)
                        && child.kind() == "argument"
                        && let Some(name_node) = child.child_by_field_name("name")
                        && &text[name_node.byte_range()] == "package"
                        && let Some(value_node) = child.child_by_field_name("value")
                        && value_node.kind() == "string"
                    {
                        let pkg = text[value_node.byte_range()]
                            .trim_matches(|c: char| c == '"' || c == '\'');
                        if !pkg.is_empty() {
                            packages.push(pkg.to_string());
                        }
                    }
                }
            }
        }

        let child_count = node.child_count();
        for i in (0..child_count).rev() {
            if let Some(child) = node.child(i as u32) {
                stack.push(child);
            }
        }
    }
    packages
}

/// Global LSP state.
///
/// # Raw URI identity
///
/// Raven's LSP document identity is the raw file `Url` supplied by the client
/// or path-resolution caller. `WorldState` keeps that convention consistently:
/// open documents, legacy document maps, workspace indexes, the cross-file file
/// cache, the dependency graph, and diagnostic publication gates are all keyed
/// by the uncanonicalized `Url`.
///
/// Symlink aliases and alternate case spellings are therefore distinct document
/// identities by design. An open buffer is authoritative for the exact raw URI
/// that was opened; it is not an alias layer for another URI that happens to
/// name the same underlying file. Raven deliberately avoids
/// `std::fs::canonicalize` for this identity model because canonicalization
/// follows symlinks and can produce path prefixes that diverge from the
/// uncanonicalized workspace-index keys. The path resolver's case correction
/// (`canonicalize_case_below`) is intentionally narrower: it rewrites only the
/// resolved suffix below a trusted prefix so source edges match index keys, and
/// does not make symlink or case aliases equivalent LSP identities.
pub struct WorldState {
    // Document management (new architecture)
    pub document_store: DocumentStore,
    pub workspace_index_new: WorkspaceIndex,

    // Legacy fields (kept for migration compatibility)
    /// Open documents keyed by the exact URI spelling supplied by the client.
    ///
    /// # Raw URI Identity
    ///
    /// Raven keeps editor-facing document identity raw: diagnostics, versions,
    /// revisions, and document text are stored under the client's `Url` so
    /// publishes go back to the URI the editor owns. Cross-file graph and index
    /// keys likewise stay uncanonicalized. [`Self::open_document_aliases`]
    /// bridges only open-buffer authority for equivalent file paths (case
    /// aliases and symlink targets discovered at open time): a graph URI that
    /// aliases an open buffer is treated as open for revalidation, live content,
    /// and watched-file vetoes, but publish work still targets the client URI.
    pub documents: HashMap<Url, Document>,
    pub open_document_aliases: OpenDocumentAliases,
    pub workspace_index: HashMap<Url, Document>,

    // Workspace configuration
    pub workspace_folders: Vec<Url>,

    // Package function awareness
    // Manages installed packages, their exports, and caching for package-aware scope resolution
    // Requirement 13.4: THE Package_Cache SHALL support concurrent read access from multiple LSP handlers
    // Arc allows sharing across async tasks without holding WorldState lock
    pub package_library: Arc<PackageLibrary>,

    // Caches
    pub help_cache: crate::help::HelpCache,
    pub html_help_cache: crate::help::HtmlHelpCache,
    pub signature_cache: Arc<SignatureCache>,
    pub cross_file_file_cache: CrossFileFileCache,
    pub diagnostics_gate: CrossFileDiagnosticsGate,
    /// Serializes each final diagnostic eligibility check + client publish
    /// with editor-resource removal and document-close clears.
    ///
    /// The lock is separate from `WorldState` so no state guard is held across
    /// the client send. Without it, a computation could pass its final tab
    /// check, lose a race to an empty clear, and then republish stale Problems.
    pub diagnostics_publish_lock: Arc<tokio::sync::Mutex<()>>,
    /// Test-only pause points for the diagnostics publish pipelines; see
    /// `DiagnosticsPublishPause`. Compiled out of production builds.
    #[cfg(any(test, feature = "test-support"))]
    pub diagnostics_test_pause: crate::cross_file::revalidation::DiagnosticsPublishPause,

    // Cross-file state
    pub cross_file_config: CrossFileConfig,
    /// Symbol provider configuration
    /// Controls document symbol and workspace symbol behavior
    pub symbol_config: SymbolConfig,
    /// Completion provider configuration
    pub completion_config: CompletionConfig,
    /// Indentation configuration
    pub indentation_config: IndentationSettings,
    /// Style/lint configuration.
    /// Master switch is tri-state (`"auto" | true | false`); default `"auto"`
    /// resolves to on when a `.lintr` is discovered (see #281 and
    /// `docs/linting.md` for the full matrix).
    pub lint_config: crate::linting::LintConfig,

    /// Last-seen client-supplied settings: LSP `initializationOptions` at
    /// startup, then the latest `did_change_configuration` payload. Stored
    /// raw so we can re-merge with the project file on either side changing.
    pub raw_client_settings: serde_json::Value,

    /// Last-loaded `raven.toml` (or `.lintr`-derived JSON), or `None` if no
    /// project config file is present. Stored raw for the same reason.
    pub raw_project_settings: Option<serde_json::Value>,

    /// Resolved path of the project config currently in effect, if any.
    /// Reported via `raven/projectConfigLoaded` to the client.
    pub project_config_path: Option<PathBuf>,

    /// Compiled `[[linting.overrides]]` entries. Empty when no overrides
    /// are configured. Per-document resolution scans this list.
    pub lint_overrides: Vec<crate::config_file::CompiledLintOverride>,

    /// The merged (client + raw project) `linting` settings section, cached
    /// by `recompute_parsed_configs` alongside `lint_overrides` so
    /// per-document override resolution never re-merges and re-clones the
    /// raw settings trees on the typing hot path. `{}` when the merged
    /// settings carry no `linting` section. Only meaningful together with
    /// `lint_overrides` — both are written by the same single writer.
    pub merged_linting_section: serde_json::Value,

    /// Compiled `[workspace].exclude` entries. Empty when no project-level
    /// exclusions are configured. These apply to workspace/default discovery,
    /// indexing, watcher resync, on-demand indexing, and LSP diagnostics.
    pub workspace_exclusions: crate::config_file::CompiledWorkspaceExclusions,

    /// Per-document `indentation_unit` overrides sent by the client via
    /// `raven/documentIndentUnitsChanged` when the user sets
    /// `raven.linting.indentationUnit` to `"auto"`. Keyed by URI string.
    /// Empty when the setting is a fixed integer; absent URIs fall back to
    /// `lint_config.indentation_unit`.
    pub per_document_indent_unit: std::collections::HashMap<String, u32>,

    pub cross_file_meta: MetadataCache,
    pub cross_file_graph: DependencyGraph,
    /// Persistent (cross-snapshot) cache of `# raven: standalone` callees'
    /// isolated EOF scopes (issue #483 / WI2b). Behind an `Arc` so the
    /// diagnostics snapshot can clone the handle out from under the read lock
    /// and consult the cache with no `WorldState` guard held (CLAUDE.md locking
    /// discipline). Survives the per-snapshot `DependencyGraph` clone, which
    /// resets its own caches.
    pub standalone_scope_cache: Arc<crate::cross_file::standalone_cache::StandaloneScopeCache>,
    /// Coarse monotonic counter bumped on package-library re-init (`#483`). A
    /// component of the `StandaloneScopeCache` key: the depth-≥1 isolated scope
    /// is independent of `base_exports`/package content (those are depth-0 /
    /// downstream), so this is defense-in-depth against any package-state input
    /// the analysis missed; a missed bump cannot cause a stale-content hit.
    pub package_config_generation: u64,
    pub cross_file_revalidation: CrossFileRevalidationState,
    pub cross_file_activity: CrossFileActivityState,
    /// Editor resources eligible to own push diagnostics, when the client
    /// supplies an explicit UI-resource set.
    ///
    /// `None` preserves normal LSP behavior for clients that do not implement
    /// Raven's VS Code-specific tab notification: every `didOpen` document may
    /// receive diagnostics. `Some` keeps hidden client-created text models as
    /// analysis inputs while preventing them from acquiring Problems entries.
    pub editor_diagnostic_uris: Option<HashSet<Url>>,
    pub cross_file_workspace_index: CrossFileWorkspaceIndex,
    /// Last-known editor-derived chunk classification for file-backed
    /// documents whose editor language made the file behave differently from
    /// path classification.
    ///
    /// Live documents use their stored [`Document::chunk_kind`]. This tiny
    /// closed-file override map preserves the editor `languageId: rmd/quarto`
    /// signal after close so watched disk resyncs, on-demand indexing, and raw
    /// file-cache metadata fallbacks keep masking extension-mismatched Rmd /
    /// Quarto files. Entries exist only when the editor-derived kind differs
    /// from [`classify_chunk_document`] for the URI path, and cross-file state
    /// removal prunes them with the rest of the URI's closed-file state.
    pub editor_chunk_kind_overrides: HashMap<Url, ChunkKind>,
    /// State-wide source for watched-file resync generations.
    ///
    /// Values are copied into `watched_file_resync_generations` per URI. Keeping
    /// the source outside that map lets removal prune URI entries without
    /// reusing a generation if a later CREATE/CHANGE or close re-adds the same
    /// URI; old delayed retries still see either a missing entry or a
    /// different value.
    pub watched_file_resync_generation_counter: u64,
    /// Per-URI latest generation for ownership of closed-file convergence.
    ///
    /// Watched CREATE/CHANGE/DELETE events bump this before queuing or applying
    /// disk state. `did_close` also bumps it while switching a URI from
    /// open-buffer truth back to disk/package truth: watcher events skipped
    /// while the document was open must not let an older delayed retry apply
    /// after close. `did_open` does not bump; open-document guards veto watched
    /// resync commits and package-input applies while the buffer is open, and
    /// the eventual close bump owns the next closed-state transition.
    ///
    /// Removal through `remove_file_from_cross_file_state` prunes the URI entry
    /// with the rest of the closed-file state. Delayed undecodable retries
    /// capture a generation and re-check it at commit/apply time: `Updated`
    /// requires an exact table match, while `Removed` requires the entry to
    /// still be absent after its own removal commit pruned it. Later watched
    /// events or closes recreate the entry from the state-wide counter, so old
    /// generations are never reused.
    pub watched_file_resync_generations: HashMap<Url, u64>,
    /// Handle to the running libpath watcher, if any. Dropping it stops watching.
    pub libpath_watcher_handle:
        Option<std::sync::Arc<super::libpath_watcher::LibpathWatcherHandle>>,
    pub package_library_ready: bool,
    /// Whether the background workspace scan has completed and the dependency
    /// graph has been populated from workspace entries. In `Auto` backward
    /// dependency mode, undefined variable diagnostics are deferred for files
    /// without explicit backward directives until this flag is true.
    pub workspace_scan_complete: bool,
    /// Container for all derived R package mode state. See package_state/mod.rs.
    pub package_state: crate::package_state::PackageState,
    /// Inputs to the package-mode `derive` function. Updated by event handlers
    /// before calling `apply_package_event`. See package_state::PackageInputs.
    pub package_inputs: crate::package_state::PackageInputs,
}

/// A snapshot of the lifecycle state a diagnostics run was triggered
/// against: the URI's `(version, revision, epoch)` captured under a
/// `WorldState` lock when the work was spawned, carried through the
/// debounce → compute → publish pipeline, and re-compared at every
/// freshness checkpoint via [`Self::is_stale`].
///
/// Bundling the three fields keeps every checkpoint epoch-aware by
/// construction. The loose `(version, revision)` pair this replaces cannot
/// identify a lifecycle (issue #603): a client may reopen at the same
/// version, and `Document::revision` restarts at 0 on every open, so a
/// worker retired by a close or tab removal could pass both comparisons
/// against the URI's next lifecycle. The epoch is globally unique per
/// lifecycle and never reused.
///
/// `version`/`revision` are `None` when the document is not open;
/// `epoch` is `None` when the URI is not diagnostic-eligible (never began,
/// or retired). Workers must decline to schedule when `epoch` is `None` —
/// an all-`None` trigger compares equal to a still-absent document, but
/// such a worker can never legally publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiagnosticsTrigger {
    pub(crate) version: Option<i32>,
    pub(crate) revision: Option<u64>,
    pub(crate) epoch: Option<DiagnosticsEpoch>,
}

impl DiagnosticsTrigger {
    /// Capture `uri`'s current trigger snapshot. The caller must hold a
    /// `WorldState` lock for the duration so the three reads are coherent.
    pub(crate) fn capture(state: &WorldState, uri: &Url) -> Self {
        let doc = state.documents.get(uri);
        Self {
            version: doc.and_then(|d| d.version),
            revision: doc.map(|d| d.revision),
            epoch: state.diagnostics_gate.current_epoch(uri),
        }
    }

    /// Whether the URI's current state no longer matches this trigger: the
    /// worker is obsolete (a newer edit's worker owns the current state) or
    /// belongs to a retired lifecycle, and must not schedule, compute, or
    /// publish past this point.
    pub(crate) fn is_stale(&self, state: &WorldState, uri: &Url) -> bool {
        *self != Self::capture(state, uri)
    }

    /// Atomically commit a publish for this trigger through `gate`,
    /// returning whether the caller may proceed to send. Encapsulates the
    /// commit contract shared by the debounced and direct pipelines so the
    /// two cannot drift:
    /// - no live epoch (`epoch: None`): never publish — a retired lifecycle
    ///   fails closed even though callers' earlier checks make this
    ///   unreachable in practice;
    /// - versioned: the gate's monotonic + force-marker predicate decides,
    ///   validating the epoch under the gate's own locks;
    /// - versionless (`version: None` with a live epoch): publish — there is
    ///   no version to gate monotonically, so the live-epoch requirement
    ///   (the caller's `is_stale` re-check under the publish lock) is the
    ///   gate.
    pub(crate) fn commit_publish(&self, gate: &CrossFileDiagnosticsGate, uri: &Url) -> bool {
        match (self.version, self.epoch) {
            (_, None) => false,
            (Some(ver), Some(epoch)) => gate.try_consume_publish(uri, ver, epoch),
            (None, Some(_)) => true,
        }
    }
}

impl WorldState {
    /// Passthrough for legacy `state.package_workspace` reads.
    pub fn package_workspace(&self) -> Option<&crate::package_namespace::PackageWorkspace> {
        self.package_state.workspace()
    }

    /// The effective per-document `LintConfig`: the workspace-wide base,
    /// patched with the auto-detected per-document indentation unit, then any
    /// matching `[[linting.overrides]]` entries layered on top. Passing the
    /// patched config as the *base* to `resolve_lint_for_document` keeps an
    /// override's `indentationUnit` winning over the per-document value, not
    /// the other way around.
    ///
    /// This is the single implementation shared by diagnostics
    /// (`DiagnosticsSnapshot::build`) and on-type formatting (the judge tier),
    /// so the two can never drift and disagree about a document's accepted
    /// columns. With no overrides configured (the common case) this is a
    /// plain config clone; with overrides it resolves against the
    /// `merged_linting_section` cached by `recompute_parsed_configs`, so no
    /// caller ever re-merges the raw settings trees on the typing hot path.
    pub fn effective_lint_config_for_document(
        &self,
        uri: &tower_lsp::lsp_types::Url,
    ) -> crate::linting::LintConfig {
        let mut base = self.lint_config.clone();
        if let Some(&unit) = self.per_document_indent_unit.get(uri.as_str()) {
            base.indentation_unit = unit;
        }
        if self.lint_overrides.is_empty() {
            return base;
        }
        crate::config_file::resolve_lint_for_document(
            &base,
            &self.merged_linting_section,
            &self.lint_overrides,
            uri,
        )
    }

    /// Bump the package/config generation (issue #483) so the persistent
    /// `StandaloneScopeCache` treats entries computed before a package-library
    /// re-init as belonging to a different key. Defensive: the depth-≥1 isolated
    /// scope is independent of package-library content, so a missed bump cannot
    /// produce a stale-content hit — this only adds isolation if some
    /// package-state input feeds the scope that the analysis did not foresee.
    pub fn bump_package_config_generation(&mut self) {
        self.package_config_generation = self.package_config_generation.wrapping_add(1);
    }

    /// Apply a `PackageInputDelta` produced by an event handler.
    /// Caller has already mutated `self.package_inputs` to reflect the event.
    /// Recomputes `package_state` as a pure function of inputs.
    pub fn apply_package_event(&mut self, delta: &crate::package_state::PackageInputDelta) {
        let new_package_state = crate::package_state::derive_package_state(
            &self.package_state,
            &self.package_inputs,
            delta,
        );
        self.package_state.set_from(new_package_state);

        // Refresh the package library's local-dev overlay from the freshly-set
        // contribution. `apply_package_event` is the single writer that recomputes
        // the contribution, so this is the one correct refresh point.
        self.refresh_local_dev_overlay();
    }

    /// (Re)build the package library's local-dev overlay from the current
    /// package-state contribution and install it on the *current*
    /// `package_library`. The overlay collects the workspace-local internal
    /// symbol set that a `devtools::load_all()` call attaches via the sentinel.
    /// It is built whenever a package workspace exists (not only when load_all
    /// is in play); that is safe because the resolution chokepoints
    /// short-circuit on the sentinel not being attached, leaving non-load_all
    /// resolution unchanged.
    ///
    /// Because a fresh `PackageLibrary` starts with a `None` overlay, every code
    /// path that *replaces* `self.package_library` (libpath rebuild, init) must
    /// call this afterward, otherwise sentinel resolution silently reverts until
    /// the next package event. `apply_package_event` also calls it.
    pub fn refresh_local_dev_overlay(&self) {
        let contrib = self.package_state.scope_contribution();
        let overlay = if contrib.workspace_root.is_some() {
            // Build the overlay from the single shared enumeration of internal
            // sources (`local_dev_internal_symbols`), the same one
            // `PackageScopeContribution::is_local_dev_internal` (the goto gate in
            // handlers.rs) tests against — so the overlay's contents and that
            // predicate cannot drift on which sources count.
            let symbols: std::collections::HashSet<String> = contrib
                .local_dev_internal_symbols()
                .map(str::to_string)
                .collect();
            Some(std::sync::Arc::new(
                crate::package_library::LocalDevPackage { symbols },
            ))
        } else {
            None
        };
        self.package_library.set_local_dev_overlay(overlay);
    }

    /// Whether `uri` is covered by the project-level `[workspace].exclude`
    /// matcher. Empty exclusions are a fast false path.
    pub(crate) fn is_project_excluded_uri(&self, uri: &tower_lsp::lsp_types::Url) -> bool {
        self.workspace_exclusions.is_excluded_uri(uri)
    }

    /// Return metadata suitable for dependency-graph edge construction under
    /// the current project exclusions.
    ///
    /// Open project-excluded documents remain in the document stores so their
    /// buffers can stay authoritative and publish live diagnostics, but they
    /// must not lend symbols to non-excluded files. The graph is intentionally
    /// exclusion-agnostic, so callers pass this filtered view when updating
    /// graph edges. The original metadata is preserved for diagnostics such as
    /// missing-file checks on the active document's literal `source()` path.
    pub(crate) fn metadata_for_dependency_graph<'a>(
        &self,
        uri: &Url,
        meta: &'a crate::cross_file::CrossFileMetadata,
        workspace_root: Option<&Url>,
    ) -> Cow<'a, crate::cross_file::CrossFileMetadata> {
        Self::metadata_for_dependency_graph_with_exclusions(
            &self.workspace_exclusions,
            uri,
            meta,
            workspace_root,
        )
    }

    pub(crate) fn metadata_for_dependency_graph_with_exclusions<'a>(
        exclusions: &crate::config_file::CompiledWorkspaceExclusions,
        uri: &Url,
        meta: &'a crate::cross_file::CrossFileMetadata,
        workspace_root: Option<&Url>,
    ) -> Cow<'a, crate::cross_file::CrossFileMetadata> {
        if exclusions.is_empty() {
            return Cow::Borrowed(meta);
        }

        let forward_ctx =
            crate::cross_file::path_resolve::PathContext::from_metadata(uri, meta, workspace_root);
        let backward_ctx = crate::cross_file::path_resolve::PathContext::new(uri, workspace_root);

        let forward_target_excluded = |source: &crate::cross_file::ForwardSource| {
            let target_uri = source.resolved_uri.clone().or_else(|| {
                let ctx = forward_ctx.as_ref()?;
                let resolved =
                    crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                        &source.path,
                        ctx,
                    )?;
                Url::from_file_path(resolved).ok()
            });
            target_uri
                .as_ref()
                .is_some_and(|target| exclusions.is_excluded_uri(target))
        };

        let backward_parent_excluded = |directive: &crate::cross_file::types::BackwardDirective| {
            let Some(ctx) = backward_ctx.as_ref() else {
                return false;
            };
            let Some(resolved) =
                crate::cross_file::path_resolve::resolve_path(&directive.path, ctx)
            else {
                return false;
            };
            Url::from_file_path(resolved)
                .ok()
                .is_some_and(|parent| exclusions.is_excluded_uri(&parent))
        };

        let sources_changed = meta.sources.iter().any(forward_target_excluded);
        let sourced_by_changed = meta.sourced_by.iter().any(backward_parent_excluded);
        if !sources_changed && !sourced_by_changed {
            return Cow::Borrowed(meta);
        }

        let mut filtered = meta.clone();
        if sources_changed {
            filtered
                .sources
                .retain(|source| !forward_target_excluded(source));
        }
        if sourced_by_changed {
            filtered
                .sourced_by
                .retain(|directive| !backward_parent_excluded(directive));
        }
        Cow::Owned(filtered)
    }

    /// Snapshot the owned inputs `resolve_system_file_sources` needs (workspace
    /// name + root, and the library search paths) so a caller can drop the state
    /// lock before resolving system.file() source edges (AGENTS.md locking
    /// discipline: never hold the WorldState lock across cross-file resolution).
    pub(crate) fn snapshot_system_file_inputs(
        &self,
    ) -> (Option<String>, Option<PathBuf>, Vec<PathBuf>) {
        let ws = self.package_state.workspace();
        let ws_name = ws.map(|w| w.name.as_str().to_owned());
        let ws_root = ws.map(|w| w.root.clone());
        let lib_paths = self.package_library.lib_paths().to_vec();
        (ws_name, ws_root, lib_paths)
    }
}

impl Default for WorldState {
    fn default() -> Self {
        Self::new()
    }
}

impl WorldState {
    /// Absolute ceiling on the multi-seed neighborhood-traversal budget,
    /// independent of the configured `max_transitive_dependents_visited`. Both
    /// multi-seed walks (`build_package_scope_snapshot` and
    /// `recompute_open_neighborhood_pins`) scale the budget by open-doc count
    /// and cap it here, so the raised #473 default (50_000) cannot drive a walk
    /// to millions of nodes when many files are open. Far above any real
    /// workspace's file count, so it never trims coverage in practice.
    const MULTI_SEED_VISITED_CEILING: usize = 200_000;

    /// Creates a new WorldState initialized with default cross-file configuration and empty caches.
    ///
    /// The returned state is populated with:
    /// - default CrossFileConfig (logged at initialization),
    /// - empty document and workspace indexes (legacy and new),
    /// - an empty, concurrently accessible PackageLibrary,
    /// - all cross-file caches and auxiliary structures in their default state.
    ///
    /// # Examples
    ///
    /// ```
    /// use raven::state::WorldState;
    ///
    /// let ws = WorldState::new();
    /// // newly created state has no opened documents or workspace folders by default
    /// assert!(ws.documents.is_empty());
    /// assert!(ws.workspace_folders.is_empty());
    /// ```
    pub fn new() -> Self {
        let config = CrossFileConfig::default();

        // Log default cross-file configuration at startup
        log::info!("Initializing cross-file configuration with defaults:");
        log::info!("  max_backward_depth: {}", config.max_backward_depth);
        log::info!("  max_forward_depth: {}", config.max_forward_depth);
        log::info!("  max_chain_depth: {}", config.max_chain_depth);
        log::info!("  assume_call_site: {:?}", config.assume_call_site);
        log::info!("  index_workspace: {}", config.index_workspace);
        log::info!(
            "  max_revalidations_per_trigger: {}",
            config.max_revalidations_per_trigger
        );
        log::info!(
            "  revalidation_debounce_ms: {}",
            config.revalidation_debounce_ms
        );
        log::info!("  Diagnostic severities:");
        log::info!(
            "    undefined_variable: {:?}",
            config.undefined_variable_severity
        );
        log::info!("    missing_file: {:?}", config.missing_file_severity);
        log::info!(
            "    circular_dependency: {:?}",
            config.circular_dependency_severity
        );
        log::info!("    out_of_scope: {:?}", config.out_of_scope_severity);
        log::info!("    max_chain_depth: {:?}", config.max_chain_depth_severity);

        Self {
            // New architecture components
            document_store: DocumentStore::new(Default::default()),
            workspace_index_new: WorkspaceIndex::new(Default::default()),

            // Legacy fields (kept for migration compatibility)
            documents: HashMap::new(),
            open_document_aliases: OpenDocumentAliases::default(),
            workspace_index: HashMap::new(),

            // Workspace configuration
            workspace_folders: Vec::new(),

            // Package function awareness
            // Initialize with empty state - will be populated via initialize() or async initialization
            // Requirement 13.4: THE Package_Cache SHALL support concurrent read access
            package_library: Arc::new(PackageLibrary::new_empty()),

            // Caches
            help_cache: crate::help::HelpCache::new(),
            html_help_cache: crate::help::HtmlHelpCache::new(),
            signature_cache: Arc::new(SignatureCache::new(500)),
            cross_file_file_cache: CrossFileFileCache::new(),
            diagnostics_gate: CrossFileDiagnosticsGate::new(),
            diagnostics_publish_lock: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(any(test, feature = "test-support"))]
            diagnostics_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),

            // Cross-file state
            cross_file_config: config,
            symbol_config: SymbolConfig::default(),
            completion_config: CompletionConfig::default(),
            indentation_config: IndentationSettings::default(),
            lint_config: crate::linting::LintConfig::default(),
            raw_client_settings: serde_json::Value::Object(serde_json::Map::new()),
            raw_project_settings: None,
            project_config_path: None,
            lint_overrides: Vec::new(),
            merged_linting_section: serde_json::json!({}),
            workspace_exclusions: crate::config_file::CompiledWorkspaceExclusions::default(),
            per_document_indent_unit: std::collections::HashMap::new(),
            cross_file_meta: MetadataCache::new(),
            cross_file_graph: DependencyGraph::new(),
            standalone_scope_cache: Arc::new(
                crate::cross_file::standalone_cache::StandaloneScopeCache::new(),
            ),
            package_config_generation: 0,
            cross_file_revalidation: CrossFileRevalidationState::new(),
            cross_file_activity: CrossFileActivityState::new(),
            editor_diagnostic_uris: None,
            cross_file_workspace_index: CrossFileWorkspaceIndex::new(),
            editor_chunk_kind_overrides: HashMap::new(),
            watched_file_resync_generation_counter: 0,
            watched_file_resync_generations: HashMap::new(),
            libpath_watcher_handle: None,
            package_library_ready: false,
            workspace_scan_complete: false,
            package_state: crate::package_state::PackageState::new(),
            package_inputs: crate::package_state::PackageInputs::default(),
        }
    }

    /// Drain the text and HTML help caches.
    ///
    /// Call this whenever the package set may have shifted underneath cached
    /// help content (libpath watcher events, `raven.refreshPackages`, and the
    /// package-settings branch of `did_change_configuration`). Keeping all the
    /// callers funnelled through this helper makes it impossible to flush one
    /// cache and forget the other.
    pub fn clear_help_caches(&self) {
        self.help_cache.drain();
        self.html_help_cache.drain();
    }

    /// Whether `uri` may own editor diagnostics under the client's policy.
    ///
    /// This predicate must be checked again at the atomic publish commit, not
    /// only before computation: a tab can close while diagnostics are running.
    pub(crate) fn diagnostics_publish_allowed(&self, uri: &Url) -> bool {
        self.editor_diagnostic_uris
            .as_ref()
            .is_none_or(|uris| uris.contains(uri))
    }

    /// Begin a fresh diagnostics lifecycle for `uri` if it is currently
    /// eligible to own push diagnostics ([`Self::diagnostics_publish_allowed`]);
    /// returns `None` without minting an epoch otherwise.
    ///
    /// Call on every "URI becomes diagnostic-eligible" transition: `did_open`
    /// (both the project-excluded and normal branches) and a tab re-addition
    /// via `raven/activeDocumentsChanged` — for the latter, only AFTER
    /// `editor_diagnostic_uris` has been replaced, since eligibility is read
    /// from it (calling before the replacement silently mints nothing and
    /// the re-added tab's publishes all fail closed).
    pub(crate) fn begin_diagnostic_lifecycle(&self, uri: &Url) -> Option<DiagnosticsEpoch> {
        self.diagnostics_publish_allowed(uri)
            .then(|| self.diagnostics_gate.begin_epoch(uri))
    }

    /// Retire `uri`'s diagnostics lifecycle: cancel pending debounced work
    /// and clear all gate state including the lifecycle epoch. Call on every
    /// "URI stops being diagnostic-eligible" transition: `did_close` and tab
    /// removal. In-flight workers holding the retired epoch fail their next
    /// [`DiagnosticsTrigger::is_stale`] check or the atomic gate commit, so
    /// they cannot publish into the URI's next lifecycle.
    pub(crate) fn retire_diagnostic_lifecycle(&self, uri: &Url) {
        self.cross_file_revalidation.cancel(uri);
        self.diagnostics_gate.clear(uri);
    }

    /// Retire every diagnostics lifecycle and cancel all pending debounced
    /// work. Called on server shutdown so no in-flight diagnostic work can
    /// publish after the shutdown response.
    pub(crate) fn retire_all_diagnostic_lifecycles(&self) {
        self.cross_file_revalidation.cancel_all();
        self.diagnostics_gate.clear_all();
    }

    fn open_alias_candidates_for_uri(&self, uri: &Url) -> Vec<Url> {
        let Ok(path) = uri.to_file_path() else {
            return Vec::new();
        };

        let mut candidates = Vec::new();
        if let Some(case_path) = self.case_correct_open_path(&path)
            && let Ok(case_uri) = Url::from_file_path(case_path)
            && case_uri != *uri
        {
            candidates.push(case_uri);
        }
        if let Some(target_path) = self.symlink_target_open_path(&path)
            && let Ok(target_uri) = Url::from_file_path(target_path)
            && target_uri != *uri
        {
            candidates.push(target_uri);
        }

        let mut seen = HashSet::new();
        candidates.retain(|candidate| seen.insert(candidate.clone()));
        candidates
    }

    fn case_correct_open_path(&self, path: &Path) -> Option<PathBuf> {
        let base = self
            .workspace_base_for_path(path)
            .or_else(|| filesystem_root_base(path))?;
        crate::cross_file::path_resolve::canonicalize_case_below_unique(&base, path)
    }

    fn symlink_target_open_path(&self, path: &Path) -> Option<PathBuf> {
        let canonical_path = fs::canonicalize(path).ok()?;
        let rebased = self
            .rebase_canonical_path_to_workspace(&canonical_path)
            .unwrap_or(canonical_path);
        self.case_correct_open_path(&rebased).or(Some(rebased))
    }

    fn workspace_base_for_path(&self, path: &Path) -> Option<PathBuf> {
        self.workspace_folders
            .iter()
            .filter_map(|root| root.to_file_path().ok())
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
    }

    fn rebase_canonical_path_to_workspace(&self, canonical_path: &Path) -> Option<PathBuf> {
        let mut best: Option<(usize, PathBuf)> = None;
        for root_url in &self.workspace_folders {
            let Ok(root) = root_url.to_file_path() else {
                continue;
            };
            let Ok(canonical_root) = fs::canonicalize(&root) else {
                continue;
            };
            let Ok(suffix) = canonical_path.strip_prefix(&canonical_root) else {
                continue;
            };
            let depth = canonical_root.components().count();
            let rebased = root.join(suffix);
            if best
                .as_ref()
                .is_none_or(|(best_depth, _)| depth > *best_depth)
            {
                best = Some((depth, rebased));
            }
        }
        best.map(|(_, path)| path)
    }

    fn register_open_document_aliases(&mut self, uri: &Url) -> Vec<Url> {
        let aliases = self.open_alias_candidates_for_uri(uri);
        self.open_document_aliases
            .open(uri.clone(), aliases.clone());
        aliases
    }

    pub fn is_document_open_or_alias(&self, uri: &Url) -> bool {
        self.documents.contains_key(uri)
            || self
                .open_document_aliases
                .open_uris_for_canonical(uri)
                .is_some_and(|open_uris| {
                    open_uris
                        .iter()
                        .any(|open_uri| self.documents.contains_key(open_uri))
                })
    }

    pub fn open_document_uri_for_authoritative_uri(&self, uri: &Url) -> Option<Url> {
        if self.documents.contains_key(uri) {
            return Some(uri.clone());
        }
        self.open_document_aliases
            .open_uris_for_canonical(uri)?
            .iter()
            .find(|open_uri| self.documents.contains_key(*open_uri))
            .cloned()
    }

    pub fn canonical_uris_for_open_document(&self, uri: &Url) -> Vec<Url> {
        self.open_document_aliases
            .canonical_uris_for_open(uri)
            .map(<[Url]>::to_vec)
            .unwrap_or_default()
    }

    /// Return the canonical workspace URI that should be used for depth-0
    /// scope-contribution path checks for `open_uri`, when `open_uri` is an
    /// authoritative alias of a workspace file.
    ///
    /// This is broader than package-source membership: the same query URI gates
    /// package symbols, testthat packages, self-package NSE policy, parameter
    /// scope, and `.Rprofile` prelude applicability. Diagnostics and document
    /// storage still use the raw client URI; callers use this only for
    /// membership/scope resolution and fall back to `open_uri` on `None`.
    ///
    /// `None` is the no-alias fast path: callers should keep using `open_uri`.
    pub(crate) fn authoritative_workspace_query_uri_for_open_document(
        &self,
        open_uri: &Url,
        workspace_root: &std::path::Path,
    ) -> Option<Url> {
        if self.open_document_aliases.is_empty() {
            return None;
        }
        let canonical_uris = self
            .open_document_aliases
            .canonical_uris_for_open(open_uri)?;
        for canonical_uri in canonical_uris.iter().rev() {
            let Ok(path) = canonical_uri.to_file_path() else {
                continue;
            };
            if path.strip_prefix(workspace_root).is_err() {
                continue;
            }
            if self
                .open_document_uri_for_authoritative_uri(canonical_uri)
                .as_ref()
                == Some(open_uri)
            {
                return Some(canonical_uri.clone());
            }
        }
        None
    }

    /// Return the authoritative URI root for `target_path` when `open_uri`
    /// owns that path either directly or through an open-document alias.
    pub(crate) fn authoritative_open_uri_for_path(
        &self,
        open_uri: &Url,
        target_path: &std::path::Path,
    ) -> Option<Url> {
        if open_uri.to_file_path().ok().as_deref() == Some(target_path) {
            return Some(open_uri.clone());
        }
        if self.open_document_aliases.is_empty() {
            return None;
        }
        self.authoritative_revalidation_roots_for_uri(open_uri)
            .into_iter()
            .find(|root| root.to_file_path().ok().as_deref() == Some(target_path))
    }

    pub fn revalidation_roots_for_uri(&self, uri: &Url) -> Vec<Url> {
        let mut roots = Vec::with_capacity(
            self.open_document_aliases
                .canonical_uris_for_open(uri)
                .map_or(1, |aliases| aliases.len() + 1),
        );
        roots.push(uri.clone());
        if let Some(aliases) = self.open_document_aliases.canonical_uris_for_open(uri) {
            roots.extend(aliases.iter().cloned());
        }
        roots
    }

    pub fn authoritative_revalidation_roots_for_uri(&self, uri: &Url) -> Vec<Url> {
        let roots = self.revalidation_roots_for_uri(uri);
        if !self.documents.contains_key(uri) || self.open_document_aliases.is_empty() {
            return roots;
        }

        roots
            .into_iter()
            .filter(|root| self.open_document_uri_for_authoritative_uri(root).as_ref() == Some(uri))
            .collect()
    }

    pub fn affected_open_dependents_after_edit(
        &self,
        edited_uri: &Url,
        interface_changed: bool,
        edges_changed: bool,
    ) -> Vec<Url> {
        let mut affected = Vec::new();
        let mut seen = HashSet::new();
        for root in self.authoritative_revalidation_roots_for_uri(edited_uri) {
            let dependents =
                crate::cross_file::revalidation::compute_affected_dependents_after_edit(
                    &root,
                    interface_changed,
                    edges_changed,
                    &self.cross_file_graph,
                    |u| self.is_document_open_or_alias(u),
                    self.cross_file_config.max_chain_depth,
                    self.cross_file_config.max_transitive_dependents_visited,
                );
            for dependent in dependents {
                let Some(open_uri) = self.open_document_uri_for_authoritative_uri(&dependent)
                else {
                    continue;
                };
                if seen.insert(open_uri.clone()) {
                    affected.push(open_uri);
                }
            }
        }
        affected
    }

    /// Create a content provider for this state
    ///
    /// The content provider provides a unified interface for accessing file content,
    /// metadata, and artifacts. It respects the open-docs-authoritative rule:
    /// open documents always take precedence over indexed data.
    ///
    /// This method creates a content provider with legacy field support for
    /// migration compatibility. During the migration period, both old and new
    /// fields are checked.
    ///
    /// **Validates: Requirements 4.1, 13.1, 13.2**
    pub fn content_provider(&self) -> DefaultContentProvider<'_> {
        self.content_provider_with_documents(&self.documents)
    }

    /// Like [`Self::content_provider`] but with an explicit open-documents map
    /// instead of `self.documents`. Used by `raven check`'s parallel per-file
    /// loop (issue #479 WI3): each rayon worker supplies a one-entry map holding
    /// just its target, so exactly one document is "open" per task without
    /// mutating the shared `self.documents` (open docs outrank index content, so
    /// sharing one `documents` map across workers would make each worker treat
    /// the others' targets as open and pull the wrong artifacts). Every other
    /// field is shared by reference (immutable after the workspace scan).
    pub fn content_provider_with_documents<'a>(
        &'a self,
        documents: &'a HashMap<Url, Document>,
    ) -> DefaultContentProvider<'a> {
        DefaultContentProvider::with_legacy(
            &self.document_store,
            &self.workspace_index_new,
            &self.cross_file_file_cache,
            documents,
            &self.workspace_index,
            &self.cross_file_workspace_index,
            &self.open_document_aliases,
        )
    }

    /// Build a snapshot of the dependency neighborhood for package scope
    /// resolution. The snapshot includes artifacts/metadata for all files
    /// reachable from `docs` via the cross-file dependency graph (not just
    /// open documents), so inherited packages from closed parent files are
    /// discovered.
    ///
    /// Call this under the read lock, then drop the lock before running
    /// `scope_at_position_with_graph` against the returned snapshot.
    pub(crate) fn build_package_scope_snapshot(
        &self,
        docs: &[(Url, u32)],
    ) -> crate::backend::ScopeProbeSnapshot {
        let max_depth = self.cross_file_config.max_chain_depth;
        let max_visited = self.cross_file_config.max_transitive_dependents_visited;
        // Scale the shared visited budget with seed count so workspaces with many
        // open files retain coverage equivalent to the old per-seed loop, capped to
        // bound lock-hold time when the user has hundreds of files open.
        //
        // Two ceilings apply, whichever is smaller. The relative one
        // (`max_visited * 50`) caps the per-seed-count scaling. The absolute one
        // (`MULTI_SEED_VISITED_CEILING`) bounds total nodes regardless of the
        // configured budget, so the raised default of
        // `max_transitive_dependents_visited` (issue #473 lifted it from 2_000 to
        // 50_000) cannot push the multi-seed walk to 2.5M nodes — an unnecessary
        // latency/memory cliff. At the new default the absolute ceiling binds once
        // `docs.len() >= 4`; 200_000 still far exceeds any real workspace's file
        // count (the neighborhood is naturally bounded by it), so it never trims
        // coverage in practice. The same ceiling guards
        // `recompute_open_neighborhood_pins`, the other multi-seed walk.
        let effective_max_visited = max_visited
            .saturating_mul(docs.len().max(1))
            .min(max_visited.saturating_mul(50))
            .min(Self::MULTI_SEED_VISITED_CEILING);

        let neighborhood = self.cross_file_graph.collect_neighborhood_multi(
            docs.iter().map(|(uri, _)| uri.clone()),
            max_depth,
            effective_max_visited,
        );

        let content_provider = self.content_provider();
        let mut artifacts_map = HashMap::with_capacity(neighborhood.len());
        let mut metadata_map = HashMap::with_capacity(neighborhood.len());
        for u in &neighborhood {
            if let Some(a) = content_provider.get_artifacts(u) {
                artifacts_map.insert(u.clone(), a);
            }
            if let Some(m) = content_provider.get_metadata(u) {
                metadata_map.insert(u.clone(), m);
            }
        }

        crate::backend::ScopeProbeSnapshot {
            docs: docs.to_vec(),
            package_query_uris: self
                .package_inputs
                .workspace_root
                .as_ref()
                .map(|root| {
                    docs.iter()
                        .filter_map(|(uri, _)| {
                            self.authoritative_workspace_query_uri_for_open_document(uri, root)
                                .map(|query_uri| (uri.clone(), query_uri))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            artifacts_map,
            metadata_map,
            doc_loaded_packages: self
                .documents
                .iter()
                .map(|(uri, doc)| (uri.clone(), doc.loaded_packages.clone()))
                .collect(),
            graph: self.cross_file_graph.extract_subgraph(&neighborhood),
            workspace_folder: self.workspace_folders.first().cloned(),
            max_chain_depth: self.cross_file_config.max_chain_depth,
            backward_dependencies: self.cross_file_config.backward_dependencies,
            scope_contribution: self.package_state.scope_contribution().clone(),
        }
    }

    /// Recompute the pinned URI set across all caches that hold open-document
    /// neighborhood entries.
    ///
    /// The pinned set is the transitive dependency neighborhood of every open
    /// document — closed-but-reachable files included. Pinned entries are
    /// protected from LRU eviction in `DocumentStore`, `WorkspaceIndex`, and
    /// `CrossFileWorkspaceIndex`, so closed-but-reachable documents survive
    /// across edits to other files and avoid the `compute_artifacts_with_metadata`
    /// recomputation fallback. `DocumentStore` only physically holds open
    /// documents; closed neighbors live in the two workspace indexes, so all
    /// three caches must share the same pin set to fully cover the contract.
    ///
    /// Call after the open set changes (`did_open` / `did_close`) or after a
    /// dependency-graph edge change touches an open file.
    pub fn recompute_open_neighborhood_pins(&mut self) {
        let mut open_uris: Vec<Url> = self.document_store.uris();
        let mut seen_open_roots: HashSet<Url> = open_uris.iter().cloned().collect();
        for open_uri in self.document_store.uris() {
            for canonical_uri in self.canonical_uris_for_open_document(&open_uri) {
                if seen_open_roots.insert(canonical_uri.clone()) {
                    open_uris.push(canonical_uri);
                }
            }
        }
        if open_uris.is_empty() {
            self.document_store.set_pinned_uris(HashSet::new());
            self.workspace_index_new.set_pinned_uris(HashSet::new());
            self.cross_file_workspace_index
                .set_pinned_uris(HashSet::new());
            return;
        }

        let max_depth = self.cross_file_config.max_chain_depth;
        let max_visited = self.cross_file_config.max_transitive_dependents_visited;
        // Same scaling AND absolute ceiling as build_package_scope_snapshot:
        // bound lock-hold time while preserving per-seed coverage. The absolute
        // ceiling matters with the raised default (issue #473): 50 open files at
        // the 50_000 default would otherwise allow a 2.5M-node walk.
        let effective_max_visited = max_visited
            .saturating_mul(open_uris.len().max(1))
            .min(max_visited.saturating_mul(50))
            .min(Self::MULTI_SEED_VISITED_CEILING);

        let neighborhood = self.cross_file_graph.collect_neighborhood_multi(
            open_uris.iter().cloned(),
            max_depth,
            effective_max_visited,
        );

        // Mirror the same neighborhood across all three caches. Cloning is
        // cheap relative to the neighborhood traversal and avoids needing
        // `Arc<HashSet<Url>>` plumbing through the existing setter signature.
        self.workspace_index_new
            .set_pinned_uris(neighborhood.clone());
        self.cross_file_workspace_index
            .set_pinned_uris(neighborhood.clone());
        self.document_store.set_pinned_uris(neighborhood);
    }

    /// Resize all LRU caches based on configuration.
    /// Called after parsing initialization options.
    pub fn resize_caches(&self, config: &crate::cross_file::config::CrossFileConfig) {
        self.cross_file_meta
            .resize(config.cache_metadata_max_entries);
        self.cross_file_file_cache.resize(
            config.cache_file_content_max_entries,
            config.cache_existence_max_entries,
        );
        self.cross_file_workspace_index
            .resize(config.cache_workspace_index_max_entries);
    }

    #[cfg(test)]
    pub fn open_document(&mut self, uri: Url, text: &str, version: Option<i32>) {
        let aliases = self.register_open_document_aliases(&uri);
        self.cross_file_file_cache.invalidate(&uri);
        for alias in aliases {
            self.cross_file_file_cache.invalidate(&alias);
        }
        self.documents
            .insert(uri.clone(), Document::new_with_uri(text, version, &uri));
    }

    pub fn open_document_with_language_id(
        &mut self,
        uri: Url,
        text: &str,
        version: Option<i32>,
        language_id: Option<&str>,
    ) {
        // The disk-content cache is a CLOSED-file tier. A snapshot surviving
        // into the open lifetime can wrongly win the disk-resync staleness
        // veto at the next close — e.g. a `git checkout` restoring older
        // mtimes while the buffer is open, when watcher events (and their
        // cache invalidation) are skipped because open docs are
        // authoritative. No reader consults this cache for open documents.
        let aliases = self.register_open_document_aliases(&uri);
        self.cross_file_file_cache.invalidate(&uri);
        let document = Document::new_with_language_id(text, version, &uri, language_id);
        self.record_editor_chunk_kind_override(&uri, document.chunk_kind);
        for alias in aliases {
            self.cross_file_file_cache.invalidate(&alias);
        }
        self.documents.insert(uri.clone(), document);
    }

    pub fn close_document(&mut self, uri: &Url) -> Vec<Url> {
        let aliases = self.open_document_aliases.close(uri);
        self.documents.remove(uri);
        aliases
    }

    /// Persist or clear the editor-derived chunk-kind override for a
    /// file-backed document.
    ///
    /// Only extension mismatches are stored: when `chunk_kind` equals
    /// [`classify_chunk_document`] for `uri.path()`, any previous override is
    /// removed. This keeps the map small and means ordinary `.Rmd`,
    /// `.Rmarkdown`, and `.qmd` files continue to be path-classified after
    /// close. Untitled and other non-file URIs never have closed disk state, so
    /// they also clear any stale entry.
    pub fn record_editor_chunk_kind_override(&mut self, uri: &Url, chunk_kind: ChunkKind) {
        if uri.to_file_path().is_err() || chunk_kind == classify_chunk_document(uri.path()) {
            self.editor_chunk_kind_overrides.remove(uri);
        } else {
            self.editor_chunk_kind_overrides
                .insert(uri.clone(), chunk_kind);
        }
    }

    /// Drop the persisted editor-derived chunk-kind override for `uri`.
    ///
    /// Call this from every path that removes a URI's closed cross-file state
    /// (deletion, final undecodable convergence, or workspace-index cleanup) so
    /// a later recreated file starts from path classification until the editor
    /// supplies a fresh language signal.
    pub fn prune_editor_chunk_kind_override(&mut self, uri: &Url) {
        self.editor_chunk_kind_overrides.remove(uri);
    }

    /// Return the chunk kind to use for closed-file disk content.
    ///
    /// The last-known editor-derived override wins over path classification.
    /// This mirrors open-document precedence without requiring raw disk caches
    /// or workspace-index entries to persist a full `Document`.
    pub fn chunk_kind_for_closed_file(&self, uri: &Url) -> ChunkKind {
        self.editor_chunk_kind_overrides
            .get(uri)
            .copied()
            .unwrap_or_else(|| classify_chunk_document(uri.path()))
    }

    /// Return the R-analysis view of raw closed-file `content` for `uri`.
    ///
    /// This is the state-aware sibling of
    /// [`crate::cross_file::analysis_text_for_path`]: persisted editor-derived
    /// chunk classification wins before falling back to path classification, so
    /// extension-mismatched Rmd/Quarto files continue to mask prose after close.
    pub fn analysis_text_for_uri<'a>(
        &self,
        uri: &Url,
        content: &'a str,
    ) -> std::borrow::Cow<'a, str> {
        crate::cross_file::analysis_text_for_kind(self.chunk_kind_for_closed_file(uri), content)
    }

    /// Extract metadata from raw closed-file `content` for `uri`.
    ///
    /// File-cache and content-provider fallbacks should use this instead of
    /// [`crate::cross_file::extract_metadata_for_path`] whenever a
    /// [`WorldState`] is available, because the persisted editor-derived chunk
    /// kind must outrank path classification for extension-mismatched Rmd /
    /// Quarto files.
    pub fn extract_metadata_for_uri(
        &self,
        uri: &Url,
        content: &str,
    ) -> crate::cross_file::CrossFileMetadata {
        crate::cross_file::extract_metadata_for_kind(self.chunk_kind_for_closed_file(uri), content)
    }

    pub fn apply_change(&mut self, uri: &Url, change: TextDocumentContentChangeEvent) {
        let aliases = self.canonical_uris_for_open_document(uri);
        self.cross_file_file_cache.invalidate(uri);
        for alias in aliases {
            self.cross_file_file_cache.invalidate(&alias);
        }
        if let Some(doc) = self.documents.get_mut(uri) {
            doc.apply_change(change);
        }
    }

    pub fn get_document(&self, uri: &Url) -> Option<&Document> {
        self.documents.get(uri)
    }

    /// Find or parse `CrossFileMetadata` for `uri` for the working-directory
    /// inheritance closures used by snapshot builds and several diagnostic
    /// helpers. Walks the chain: open document → cross-file workspace index
    /// → file-cache contents. Returns an `Arc` so callers (closures bound to
    /// `compute_inherited_working_directory`) avoid deep clones.
    pub fn get_or_parse_metadata(
        &self,
        uri: &Url,
    ) -> Option<Arc<crate::cross_file::CrossFileMetadata>> {
        if let Some(open_uri) = self.open_document_uri_for_authoritative_uri(uri)
            && let Some(doc) = self.documents.get(&open_uri)
        {
            // Parse from `analysis_text()`: masked for Rmd/Quarto (so a
            // `# raven: cd` in prose is ignored while one inside a chunk is a
            // real directive), raw for everything else (behavior-neutral).
            return Some(Arc::new(crate::cross_file::directive::parse_directives(
                &doc.analysis_text(),
            )));
        }
        if let Some(meta) = self.cross_file_workspace_index.get_metadata(uri) {
            return Some(meta);
        }
        let content_provider = self.content_provider();
        if let Some(content) = content_provider.get_content(uri) {
            // Cached content is RAW; mask Rmd/Quarto before extracting so
            // directives come from chunk bodies, not prose (#343). Use the
            // persisted editor-derived chunk kind when present so an
            // extension-mismatched Rmd/Quarto file keeps masking after close
            // (#563).
            return Some(Arc::new(self.extract_metadata_for_uri(uri, &content)));
        }
        None
    }

    /// Get enriched metadata for a URI, preferring already-enriched sources.
    ///
    /// Priority order:
    /// 1. DocumentStore (open documents with enriched metadata)
    /// 2. WorkspaceIndex (new unified index)
    /// 3. Legacy cross_file_workspace_index
    /// 4. Legacy documents HashMap (re-extract metadata)
    /// 5. File cache (re-extract metadata)
    ///
    /// Rmd/Quarto note (issue #343): every tier here is masked-correct for
    /// open R Markdown / Quarto documents. The DocumentStore arm (tier 1)
    /// stores metadata extracted from the masked analysis text at
    /// `did_open`/`did_change` time (`backend.rs` passes
    /// `extract_metadata_for_path`, and `DocumentStore::compute_derived`
    /// re-derives artifacts from the masked text); the legacy-documents arm
    /// (tier 4) and the file-cache arm (tier 5) likewise mask via
    /// `analysis_text()` / `extract_metadata_for_uri`. The state-aware file
    /// cache fallback consults persisted editor-language chunk classification
    /// before path classification, so extension-mismatched Rmd/Quarto files do
    /// not start treating prose as R after close (issue #563). So directives,
    /// `source()`, and `library()` always reflect chunk bodies, never prose.
    pub fn get_enriched_metadata(
        &self,
        uri: &Url,
    ) -> Option<Arc<crate::cross_file::CrossFileMetadata>> {
        self.open_document_uri_for_authoritative_uri(uri)
            .and_then(|open_uri| {
                self.document_store
                    .get_without_touch(&open_uri)
                    .map(|doc| doc.metadata.clone())
                    .or_else(|| {
                        self.documents.get(&open_uri).map(|doc| {
                            Arc::new(crate::cross_file::extract_metadata(&doc.analysis_text()))
                        })
                    })
            })
            .or_else(|| self.workspace_index_new.get_metadata(uri))
            .or_else(|| self.cross_file_workspace_index.get_metadata(uri))
            .or_else(|| {
                // `analysis_text()`: masked for Rmd/Quarto (directives/source()/
                // library() come from chunk bodies, not prose), raw otherwise
                // (behavior-neutral for plain R / JAGS / Stan).
                self.documents
                    .get(uri)
                    .map(|doc| Arc::new(crate::cross_file::extract_metadata(&doc.analysis_text())))
            })
            .or_else(|| {
                // Cached content is RAW; mask Rmd/Quarto before extracting so
                // directives/source()/library() come from chunk bodies, not
                // prose (#343), preserving any editor-language override from
                // the closed document (#563).
                self.cross_file_file_cache
                    .get(uri)
                    .map(|content| Arc::new(self.extract_metadata_for_uri(uri, &content)))
            })
    }

    fn metadata_for_open_graph_root<'a>(
        &self,
        root: &Url,
        open_uri: &Url,
        meta: &'a crate::cross_file::CrossFileMetadata,
        workspace_root: Option<&Url>,
    ) -> std::borrow::Cow<'a, crate::cross_file::CrossFileMetadata> {
        if root == open_uri {
            return self.metadata_for_dependency_graph(open_uri, meta, workspace_root);
        }

        let mut root_meta = meta.clone();
        root_meta.inherited_working_directory = None;
        crate::cross_file::enrich_metadata_with_inherited_wd(
            &mut root_meta,
            root,
            workspace_root,
            |parent_uri| self.get_enriched_metadata(parent_uri),
            self.cross_file_config.max_chain_depth,
        );
        let graph_meta = self.metadata_for_dependency_graph(root, &root_meta, workspace_root);
        std::borrow::Cow::Owned(graph_meta.into_owned())
    }

    /// Apply pre-scanned workspace index results (for non-blocking initialization).
    ///
    /// Package-mode state is *not* set from parameters: this function
    /// resets `self.package_state` to its default (neutral) value, and
    /// the caller is expected to follow with
    /// `apply_package_event(PackageInputDelta::Initial)` — which in turn
    /// derives `workspace` / `namespace_model` / `r_file_facts` /
    /// `scope_contribution` from `self.package_inputs` via
    /// `derive_package_state`. This keeps package derivation single-sourced.
    ///
    /// Tests and benchmarks that only exercise cross-file / workspace
    /// scanning behavior (and don't care about package state) can rely on
    /// the post-reset `PackageState::default()` — they don't need to call
    /// `apply_package_event` themselves.
    ///
    /// **Validates: Requirements 11.1, 13.1**
    pub fn apply_workspace_index(
        &mut self,
        index: HashMap<Url, Document>,
        cross_file_entries: HashMap<Url, crate::cross_file::workspace_index::IndexEntry>,
        new_index_entries: HashMap<Url, crate::workspace_index::IndexEntry>,
    ) {
        self.workspace_index = index;

        // Atomic reset of package state. Per-mode transitions (e.g. toggling
        // packageMode) must never leave stale `r_file_facts` or
        // `scope_contribution` from a prior mode, so we always start from a
        // neutral default here. The scan-completion caller follows this
        // with `apply_package_event`, which repopulates every field from
        // `package_inputs` via `derive_package_state`.
        self.package_state = crate::package_state::PackageState::default();

        // Populate cross-file workspace index (legacy)
        for (uri, entry) in cross_file_entries {
            log::info!(
                "Indexing cross-file entry: {} (exported symbols: {})",
                uri,
                entry.artifacts.exported_interface.len()
            );
            self.cross_file_workspace_index.insert(uri, entry);
        }

        // Populate new unified WorkspaceIndex
        for (uri, entry) in new_index_entries {
            log::trace!(
                "Indexing new workspace entry: {} (exported symbols: {})",
                uri,
                entry.artifacts.exported_interface.len()
            );
            self.workspace_index_new.insert(uri, entry);
        }

        log::info!(
            "Applied {} workspace files, {} cross-file entries, {} new index entries",
            self.workspace_index.len(),
            self.cross_file_workspace_index.uris().len(),
            self.workspace_index_new.len()
        );

        // Build the dependency graph from all workspace entries so that
        // forward-created backward edges are available for auto-detect mode.
        self.build_dependency_graph_from_workspace();
        self.workspace_scan_complete = true;
        log::info!(
            "[Background] Dependency graph built from workspace entries, workspace_scan_complete = true"
        );

        // Now that the graph reflects the workspace, refresh the document_store
        // pin set so any file opened before the scan completes picks up its
        // neighborhood.
        self.recompute_open_neighborhood_pins();
    }

    /// Build the dependency graph from all entries in the workspace index.
    ///
    /// For each file, calls `update_file` on the dependency graph using its
    /// metadata. This creates forward edges (parent→child) and their
    /// corresponding backward entries (child→parent) for all workspace files,
    /// enabling auto-detection of backward dependencies.
    fn build_dependency_graph_from_workspace(&mut self) {
        let workspace_root = self.workspace_folders.first().cloned();

        // Resolve system.file() entries before building the graph so that
        // dependency edges reflect the concrete paths.
        let ws = self.package_state.workspace();
        let ws_name = ws.map(|w| w.name.as_str()).map(|s| s.to_owned());
        let ws_root = ws.map(|w| w.root.clone());
        let lib_paths = self.package_library.lib_paths().to_vec();

        // Collect URIs and metadata to avoid borrow conflicts with self.
        // `entry.metadata` is `Arc<CrossFileMetadata>`, so the clone is a
        // refcount bump rather than a deep clone of Vec/HashSet/String fields.
        let mut entries: Vec<(Url, Arc<crate::cross_file::CrossFileMetadata>)> = Vec::new();
        for (uri, entry) in self.workspace_index_new.iter() {
            entries.push((uri.clone(), entry.metadata.clone()));
        }
        // Determinism (issue #476): `update_file` appends each file's incoming
        // edges to `backward[child]` in call order, and scope resolution's
        // parent-prefix walk follows that `Vec` order. The iteration order above
        // ultimately derives from the rayon parallel workspace scan
        // (HashMap -> LruCache insertion order) and is NOT stable run-to-run, so
        // feeding `update_file` in it made `raven check` drop a *different*
        // subset of symbols each run (709/711/680 on worldwide). Sort by URI so
        // the graph — and therefore every downstream diagnostic — is byte-stable.
        // URIs are unique, so a stable sort buys nothing; use the faster unstable.
        entries.sort_unstable_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));

        let workspace_exclusions = self.workspace_exclusions.clone();

        // Destructure self to split borrows: cross_file_graph (mutable) and
        // workspace_index_new (shared) can coexist without pre-cloning all contents.
        let Self {
            cross_file_graph,
            workspace_index_new,
            ..
        } = self;

        for (uri, meta) in &mut entries {
            // Resolve system.file() sources if any are present
            if meta.sources.iter().any(|s| s.system_file.is_some()) {
                let m = Arc::make_mut(meta);
                crate::cross_file::resolve_system_file_sources(
                    m,
                    ws_name.as_deref(),
                    ws_root.as_deref(),
                    &lib_paths,
                );
            }
            let get_content = |parent_uri: &Url| -> Option<String> {
                workspace_index_new
                    .get(parent_uri)
                    .map(|e| e.contents.to_string())
            };
            let graph_meta = Self::metadata_for_dependency_graph_with_exclusions(
                &workspace_exclusions,
                uri,
                meta.as_ref(),
                workspace_root.as_ref(),
            );
            let _result = cross_file_graph.update_file(
                uri,
                graph_meta.as_ref(),
                workspace_root.as_ref(),
                get_content,
            );
        }

        log::info!(
            "Built dependency graph from {} workspace files",
            entries.len()
        );
    }

    /// Resolve `system.file()` sources in workspace index metadata and rebuild
    /// dependency graph edges for files whose resolution changed. Must be called
    /// after `apply_package_event` so that `package_state.workspace()` is
    /// populated.
    ///
    /// Every entry with `system_file.is_some()` is revisited — resolved entries
    /// keep their `SystemFileCall` (see `resolve_system_file_sources`), so this
    /// is the single recovery point for package lifecycle events: a package
    /// install/removal in a watched libpath (`LibpathEvent::Changed`) and a
    /// workspace `Package:` rename (the DESCRIPTION manifest branch) both call
    /// this to form, drop, or re-target edges without the user editing the
    /// sourcing file.
    ///
    /// Returns the URIs whose source resolution actually changed, so callers
    /// can republish diagnostics for exactly those files (expand with
    /// [`Self::system_file_republish_set`] to include open dependents).
    pub fn resolve_system_file_in_workspace(&mut self) -> Vec<Url> {
        self.resolve_system_file_in_workspace_for_packages(None)
    }

    /// Package-filtered variant of [`Self::resolve_system_file_in_workspace`]:
    /// with `Some(packages)`, only entries containing a `system.file()` source
    /// referencing one of those packages are re-resolved; everything else is
    /// neither cloned nor disk-probed. The libpath-event consumer passes the
    /// changed-package set so a package install/removal does not re-probe
    /// every resolved entry in the workspace. Callers reacting to events that
    /// can shift resolution for arbitrary packages (startup, library swaps,
    /// a workspace `Package:` rename) pass `None`.
    ///
    /// Covers BOTH metadata stores: the workspace index (closed files) and
    /// the document store (open buffers, which are authoritative and whose
    /// metadata is read in preference to the index — see
    /// `get_enriched_metadata`). Without the open-document pass, an open
    /// buffer with a `system.file()` source would stay stale across package
    /// lifecycle events until the user edited it, and would never recover at
    /// all when the file is absent from the index (unsaved buffer,
    /// `index_workspace = false`).
    pub fn resolve_system_file_in_workspace_for_packages(
        &mut self,
        only_packages: Option<&std::collections::HashSet<String>>,
    ) -> Vec<Url> {
        let ws = self.package_state.workspace();
        let ws_name = ws.map(|w| w.name.as_str()).map(|s| s.to_owned());
        let ws_root = ws.map(|w| w.root.clone());
        let lib_paths = self.package_library.lib_paths().to_vec();

        let source_selected = |s: &crate::cross_file::ForwardSource| {
            s.system_file
                .as_ref()
                .is_some_and(|sf| only_packages.is_none_or(|pkgs| pkgs.contains(&sf.package)))
        };

        // Snapshot only the index entries with a selected system.file source —
        // `entries_matching` clones just that subset, so workspaces without
        // system.file sources (the common case) pay one predicate pass.
        let affected = self
            .workspace_index_new
            .entries_matching(|entry| entry.metadata.sources.iter().any(source_selected));

        // Open buffers with a selected system.file source (authoritative
        // metadata lives in the document store, not the index).
        let open_affected: Vec<Url> = self
            .document_store
            .uris()
            .into_iter()
            .filter(|uri| {
                self.document_store
                    .get_without_touch(uri)
                    .is_some_and(|doc| doc.metadata.sources.iter().any(source_selected))
            })
            .collect();

        if affected.is_empty() && open_affected.is_empty() {
            return Vec::new();
        }

        // Resolve into a cloned sources Vec; only a real change pays for the
        // full-metadata clone (`Arc::make_mut`), re-insertion, and the edge
        // rebuild below (resolution is idempotent, so unchanged entries need
        // none of them). Previous targets of changed resolutions are
        // collected so the cleanup below can drop external entries nothing
        // references anymore.
        let mut changed_uris: Vec<Url> = Vec::new();
        let mut old_targets: std::collections::HashSet<Url> = std::collections::HashSet::new();
        for (uri, mut entry) in affected {
            let mut new_sources = entry.metadata.sources.clone();
            crate::cross_file::resolve_system_file_source_entries(
                &mut new_sources,
                ws_name.as_deref(),
                ws_root.as_deref(),
                &lib_paths,
            );
            if new_sources != entry.metadata.sources {
                old_targets.extend(
                    entry
                        .metadata
                        .sources
                        .iter()
                        .filter_map(|s| s.resolved_uri.clone()),
                );
                Arc::make_mut(&mut entry.metadata).sources = new_sources;
                changed_uris.push(uri.clone());
                self.workspace_index_new.insert(uri, entry);
            }
        }

        // Rebuild graph edges for changed index entries
        let workspace_root = self.workspace_folders.first().cloned();
        for uri in &changed_uris {
            if let Some(entry) = self.workspace_index_new.get(uri) {
                let meta = entry.metadata.clone();
                let get_content = |parent_uri: &Url| -> Option<String> {
                    self.workspace_index_new
                        .get(parent_uri)
                        .map(|e| e.contents.to_string())
                };
                let graph_meta =
                    self.metadata_for_dependency_graph(uri, meta.as_ref(), workspace_root.as_ref());
                self.cross_file_graph.update_file(
                    uri,
                    graph_meta.as_ref(),
                    workspace_root.as_ref(),
                    get_content,
                );
            }
        }

        // Open-document pass. Runs AFTER the index pass so for a file present
        // in both stores the graph edges rebuilt here — from the buffer's
        // (authoritative) metadata — win over the index-derived ones. The
        // index pass above rebuilt edges for every URI in `changed_uris`, so
        // an open buffer whose own resolution is UNCHANGED still needs its
        // edges re-asserted when the index pass touched the same file (e.g.
        // the buffer resolved at did_open while the scanned index entry was
        // still unresolved) — otherwise the graph would keep the stale
        // index-derived edges until the user edits the buffer.
        let index_rebuilt: std::collections::HashSet<Url> = changed_uris.iter().cloned().collect();
        for uri in open_affected {
            let Some(doc_meta) = self
                .document_store
                .get_without_touch(&uri)
                .map(|doc| doc.metadata.clone())
            else {
                continue;
            };
            let mut new_sources = doc_meta.sources.clone();
            crate::cross_file::resolve_system_file_source_entries(
                &mut new_sources,
                ws_name.as_deref(),
                ws_root.as_deref(),
                &lib_paths,
            );
            let resolution_changed = new_sources != doc_meta.sources;
            let meta = if resolution_changed {
                old_targets.extend(
                    doc_meta
                        .sources
                        .iter()
                        .filter_map(|s| s.resolved_uri.clone()),
                );
                let mut new_meta = (*doc_meta).clone();
                new_meta.sources = new_sources;
                let new_meta = Arc::new(new_meta);
                self.document_store.replace_metadata(&uri, new_meta.clone());
                new_meta
            } else if index_rebuilt.contains(&uri) {
                // Unchanged buffer, but the index pass overwrote this file's
                // edges from index metadata — re-assert the buffer's.
                doc_meta.clone()
            } else {
                continue;
            };
            let graph_roots = self.authoritative_revalidation_roots_for_uri(&uri);
            for root in &graph_roots {
                let root_meta = self.metadata_for_open_graph_root(
                    root,
                    &uri,
                    meta.as_ref(),
                    workspace_root.as_ref(),
                );
                let get_content = |parent_uri: &Url| -> Option<String> {
                    self.workspace_index_new
                        .get(parent_uri)
                        .map(|e| e.contents.to_string())
                };
                self.cross_file_graph.update_file(
                    root,
                    root_meta.as_ref(),
                    workspace_root.as_ref(),
                    get_content,
                );
                if self.is_project_excluded_uri(&uri) || self.is_project_excluded_uri(root) {
                    self.cross_file_graph.make_forward_edges_non_lending(root);
                }
            }
            if resolution_changed && !changed_uris.contains(&uri) {
                changed_uris.push(uri);
            }
        }

        // Index outside-workspace files resolved via cross-package system.file
        // so their artifacts are available to scope resolution.
        self.index_cross_package_resolved_files();

        // Drop external entries the changed resolutions no longer point at.
        self.drop_orphaned_external_entries(old_targets);

        changed_uris
    }

    /// Drop outside-workspace index entries that were indexed as
    /// cross-package `system.file()` targets (see
    /// [`Self::index_cross_package_resolved_files`]) but lost their last
    /// referencing resolution — without this, a cleared or re-targeted
    /// `resolved_uri` leaves the previously indexed external file occupying
    /// an LRU slot until natural eviction.
    ///
    /// `candidates` are the previous targets of resolutions that just
    /// changed. A candidate is dropped only when it is (a) outside every
    /// workspace folder — workspace files are owned by the workspace scan,
    /// e.g. an renv library inside the project — (b) not an open document,
    /// and (c) no longer referenced by any `resolved_uri` in the index or an
    /// open buffer. The reference check is a full scan of sources, which is
    /// acceptable because resolutions only change on rare package lifecycle
    /// events.
    fn drop_orphaned_external_entries(&mut self, candidates: std::collections::HashSet<Url>) {
        if candidates.is_empty() {
            return;
        }
        let workspace_dirs: Vec<std::path::PathBuf> = self
            .workspace_folders
            .iter()
            .filter_map(|f| f.to_file_path().ok())
            .collect();
        for uri in candidates {
            if !self.workspace_index_new.contains(&uri) {
                continue;
            }
            if self.document_store.get_without_touch(&uri).is_some()
                || self.is_document_open_or_alias(&uri)
            {
                continue;
            }
            if let Ok(path) = uri.to_file_path()
                && workspace_dirs.iter().any(|dir| path.starts_with(dir))
            {
                continue;
            }
            let referenced_from_index = self.workspace_index_new.any_entry(|entry| {
                entry
                    .metadata
                    .sources
                    .iter()
                    .any(|s| s.resolved_uri.as_ref() == Some(&uri))
            });
            let referenced_from_open_doc = || {
                self.document_store.uris().into_iter().any(|doc_uri| {
                    self.document_store
                        .get_without_touch(&doc_uri)
                        .is_some_and(|doc| {
                            doc.metadata
                                .sources
                                .iter()
                                .any(|s| s.resolved_uri.as_ref() == Some(&uri))
                        })
                })
            };
            if referenced_from_index || referenced_from_open_doc() {
                continue;
            }
            self.workspace_index_new.invalidate(&uri);
            self.cross_file_graph.remove_file(&uri);
            self.prune_editor_chunk_kind_override(&uri);
        }
    }

    /// Expand the changed-URI list from
    /// [`Self::resolve_system_file_in_workspace`] into the open documents
    /// whose diagnostics may be affected: the changed files themselves plus
    /// their open transitive dependents and sibling subtrees. A parent's
    /// cross-file scope traverses forward source edges transitively, so an
    /// edge formed or dropped on a child changes the parent's diagnostics
    /// even though the parent's own text and edges are untouched — the same
    /// fan-out `did_change` performs via
    /// `compute_affected_dependents_after_edit`.
    pub fn system_file_republish_set(&self, changed: &[Url]) -> Vec<Url> {
        let mut seen: std::collections::HashSet<Url> = std::collections::HashSet::new();
        let mut out: Vec<Url> = Vec::new();
        for uri in changed {
            if let Some(open_uri) = self.open_document_uri_for_authoritative_uri(uri)
                && seen.insert(open_uri.clone())
            {
                out.push(open_uri);
            }
            let dependents = self.affected_open_dependents_after_edit(
                uri, false, // the file's text (interface) did not change
                true,  // its dependency edges did
            );
            for dep in dependents {
                if seen.insert(dep.clone()) {
                    out.push(dep);
                }
            }
        }
        out
    }

    /// Read, parse, and index outside-workspace files that were resolved via
    /// cross-package `system.file()`. Called after `resolve_system_file_sources`
    /// populates `resolved_uri` fields and graph edges are rebuilt.
    fn index_cross_package_resolved_files(&mut self) {
        // Collect resolved_uris from all workspace entries AND open buffers
        // (open-document metadata is authoritative and may carry resolutions
        // the index does not — unsaved buffers, index_workspace = false).
        let mut external_uris: Vec<Url> = Vec::new();
        for (_, entry) in self.workspace_index_new.iter() {
            for source in &entry.metadata.sources {
                if let Some(ref uri) = source.resolved_uri
                    && !self.workspace_index_new.contains(uri)
                {
                    external_uris.push(uri.clone());
                }
            }
        }
        for doc_uri in self.document_store.uris() {
            if let Some(doc) = self.document_store.get_without_touch(&doc_uri) {
                for source in &doc.metadata.sources {
                    if let Some(ref uri) = source.resolved_uri
                        && !self.workspace_index_new.contains(uri)
                    {
                        external_uris.push(uri.clone());
                    }
                }
            }
        }
        external_uris.sort();
        external_uris.dedup();

        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).ok();

        for uri in external_uris {
            if self.workspace_index_new.contains(&uri) {
                continue;
            }
            let Some(path) = uri.to_file_path().ok() else {
                continue;
            };
            let Ok(content) = read_source(&path) else {
                continue;
            };
            let Ok(fs_meta) = std::fs::metadata(&path) else {
                continue;
            };

            let tree = parser.parse(&content, None);
            let metadata = Arc::new(crate::cross_file::extract_metadata(&content));
            let artifacts = tree.as_ref().map_or_else(
                || Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
                |t| {
                    Arc::new(crate::cross_file::scope::compute_artifacts_with_metadata(
                        &uri,
                        t,
                        &content,
                        Some(&metadata),
                    ))
                },
            );
            let snapshot =
                crate::cross_file::file_cache::FileSnapshot::with_content_hash(&fs_meta, &content);

            let entry = crate::workspace_index::IndexEntry {
                contents: Rope::from_str(&content),
                tree,
                loaded_packages: Vec::new(),
                snapshot,
                metadata,
                artifacts,
                indexed_at_version: 0,
            };
            self.workspace_index_new.insert(uri, entry);
        }
    }
}

/// Scan workspace folders for R files without holding any locks (Requirement 13a)
///
/// Returns:
/// - `HashMap<Url, Document>` - Legacy index for backward compatibility
/// - `HashMap<Url, crate::cross_file::workspace_index::IndexEntry>` - Cross-file entries (legacy)
/// - `HashMap<Url, crate::workspace_index::IndexEntry>` - New unified WorkspaceIndex entries
///
/// Package-mode state (workspace/namespace model, roxygen cache, NAMESPACE
/// imports) is intentionally **not** produced here. The canonical derivation
/// is `derive_package_state`, invoked through `apply_package_event` after
/// the caller populates `WorldState::package_inputs`.
///
/// **Validates: Requirements 11.1, 11.2, 11.3, 11.4, 11.5**
pub type WorkspaceScanResult = (
    HashMap<Url, Document>,
    HashMap<Url, crate::cross_file::workspace_index::IndexEntry>,
    HashMap<Url, crate::workspace_index::IndexEntry>,
);

/// Result of processing a single workspace file (used by parallel scan).
struct ProcessedFile {
    uri: Url,
    document: Document,
    cross_file_entry: crate::cross_file::workspace_index::IndexEntry,
    new_index_entry: crate::workspace_index::IndexEntry,
}

/// Recursively collect file paths under `dir` whose leaf matches `accept`
/// (serial walk, fast). Symlinked directories ARE followed, with canonical-path
/// cycle detection to terminate on loops and avoid double-counting; the
/// non-source directories in [`should_skip_directory`] (`.git`, `node_modules`,
/// `renv`, `target`, …) are pruned. Results are unsorted; callers that need
/// deterministic order sort afterwards.
///
/// This is the single directory walk shared by the workspace indexer (which
/// passes [`is_stat_model_extension`] to collect `.r`/`.jags`/`.bugs`/`.stan`)
/// and the CLI's [`crate::cli::shared::collect_r_file_paths`] (R-only). Sharing
/// one walk is what keeps `raven check`'s *reported* file set equal to its
/// *indexed* set: a `.R` file reachable only through a symlinked directory
/// (e.g. a monorepo `src -> ../shared` layout) is both indexed for cross-file
/// resolution and reported, instead of one walk following the symlink while the
/// other skips it. Only the leaf predicate differs between callers; the
/// symlink/cycle/skip logic — the part that would otherwise drift — lives here
/// once.
pub(crate) fn collect_files_matching(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    accept: fn(&Path) -> bool,
) {
    collect_files_matching_impl(dir, out, accept, None);
}

pub(crate) fn collect_files_matching_with_exclusions(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    accept: fn(&Path) -> bool,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) {
    collect_files_matching_impl(
        dir,
        out,
        accept,
        (!exclusions.is_empty()).then_some(exclusions),
    );
}

fn collect_files_matching_impl(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    accept: fn(&Path) -> bool,
    exclusions: Option<&crate::config_file::CompiledWorkspaceExclusions>,
) {
    let mut visited = HashSet::new();
    // Seed with the canonical root so a symlink pointing back at the root (or
    // any already-visited directory) is detected as a cycle and skipped.
    if let Ok(canonical) = fs::canonicalize(dir) {
        visited.insert(canonical);
    }
    collect_files_matching_inner(dir, out, &mut visited, accept, exclusions);
}

fn collect_files_matching_inner(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    accept: fn(&Path) -> bool,
    exclusions: Option<&crate::config_file::CompiledWorkspaceExclusions>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // `is_dir()` follows symlinks, so a symlink to a directory is walked
        // (after the cycle check) and a symlink to a file falls through to the
        // `accept` branch.
        if path.is_dir() {
            // Cheap first pass: prune by the entry name (a real `node_modules`,
            // `.git`, … — no canonicalize needed for the common case).
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(should_skip_directory)
            {
                log::trace!("Skipping directory: {}", path.display());
                continue;
            }
            if exclusions.is_some_and(|exclusions| exclusions.can_prune_directory(&path)) {
                log::trace!("Skipping excluded directory: {}", path.display());
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(c) => c,
                Err(e) => {
                    log::trace!("Skipping unresolvable dir {}: {}", path.display(), e);
                    continue;
                }
            };
            // A symlink whose own name isn't skip-listed but whose TARGET is
            // (e.g. `deps -> node_modules`) must be pruned too, or it pulls the
            // whole vendored tree back into the scan.
            if canonical
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(should_skip_directory)
            {
                log::trace!(
                    "Skipping symlinked directory {} -> {}",
                    path.display(),
                    canonical.display()
                );
                continue;
            }
            if !visited.insert(canonical) {
                log::trace!("Skipping symlink cycle: {}", path.display());
                continue;
            }
            collect_files_matching_inner(&path, out, visited, accept, exclusions);
        } else if accept(&path)
            && !exclusions.is_some_and(|exclusions| exclusions.is_excluded_path(&path))
        {
            out.push(path);
        }
    }
}

/// Why a source file could not be read as text by [`read_source`].
#[derive(Debug)]
pub(crate) enum SourceReadError {
    /// The file could not be read from disk at all (missing, permissions, …).
    Io(std::io::Error),
    /// The bytes are not valid UTF-8 and carry no UTF-16 byte-order mark —
    /// almost always a legacy single-byte encoding (Latin-1 / Windows-1252).
    /// `offset` is the byte index of the first undecodable byte and `byte` its
    /// value, for an actionable diagnostic. `byte` is `0` only in the rare
    /// malformed-UTF-16 case, where no single offending byte is meaningful.
    InvalidEncoding { offset: usize, byte: u8 },
}

impl std::fmt::Display for SourceReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceReadError::Io(e) => write!(f, "{e}"),
            // `byte == 0` is the malformed/odd-length UTF-16 case (the file
            // carried a BOM): no single offending byte is meaningful to name.
            SourceReadError::InvalidEncoding { byte: 0, .. } => {
                f.write_str("could not be decoded as UTF-8 or UTF-16")
            }
            SourceReadError::InvalidEncoding { offset, byte } => write!(
                f,
                "not valid UTF-8: first invalid byte {byte:#04x} at offset {offset}"
            ),
        }
    }
}

impl std::error::Error for SourceReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SourceReadError::Io(e) => Some(e),
            SourceReadError::InvalidEncoding { .. } => None,
        }
    }
}

/// Read a source file as UTF-8 text, transparently handling byte-order marks:
/// a UTF-8 BOM is stripped, and BOM-marked UTF-16 LE/BE is decoded to UTF-8.
/// Any other input must already be valid UTF-8.
///
/// This is the single disk-read seam shared by the workspace scan
/// ([`process_workspace_file`]) and `raven check`'s report loop, so the two
/// decode files identically. It deliberately does NOT guess legacy encodings: a
/// non-UTF-8 file with no UTF-16 BOM is reported as
/// [`SourceReadError::InvalidEncoding`] rather than silently mis-decoded —
/// guessing would hide bugs (e.g. a non-breaking space sitting inside a string
/// comparison reads as a normal space). The scan discards the error (an
/// undecodable file is simply left unindexed); `raven check` turns it into a
/// reported finding. This governs only files raven reads from disk — open
/// documents arrive already-decoded from the editor over LSP.
pub(crate) fn read_source(path: &Path) -> Result<String, SourceReadError> {
    decode_source(fs::read(path).map_err(SourceReadError::Io)?)
}

/// Async counterpart to [`read_source`]: read the file's bytes off the Tokio
/// runtime, then decode them through the shared [`decode_source`] rules. Used by
/// the LSP's async cross-file readers (watched-file reindex, on-demand indexing)
/// so they handle a UTF-8 BOM and UTF-16 identically to the synchronous scan.
/// Like `read_source`, error *policy* is the caller's: those index paths discard
/// the error and skip the file — they never publish encoding diagnostics.
pub(crate) async fn read_source_async(path: &Path) -> Result<String, SourceReadError> {
    decode_source(tokio::fs::read(path).await.map_err(SourceReadError::Io)?)
}

/// Decode raw file bytes per the [`read_source`] rules. Split out so the
/// BOM/UTF-8 logic is unit-testable without touching the filesystem, and so
/// both [`read_source`] (sync, via `fs::read`) and [`read_source_async`]
/// (async, via `tokio::fs::read`) share the exact same decode regardless of how
/// they read the bytes. Takes an owned `Vec` so the common no-BOM UTF-8 path
/// moves the buffer straight into the `String` without copying.
fn decode_source(bytes: Vec<u8>) -> Result<String, SourceReadError> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        // UTF-8 BOM: strip it; an error's file offset is then `3 + valid_up_to`.
        return decode_utf8_slice(&bytes[3..], 3);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16(&bytes[2..], false);
    }
    String::from_utf8(bytes).map_err(|e| {
        let offset = e.utf8_error().valid_up_to();
        SourceReadError::InvalidEncoding {
            offset,
            byte: e.as_bytes().get(offset).copied().unwrap_or(0),
        }
    })
}

fn decode_utf8_slice(bytes: &[u8], base_offset: usize) -> Result<String, SourceReadError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|e| SourceReadError::InvalidEncoding {
            offset: base_offset + e.valid_up_to(),
            byte: bytes.get(e.valid_up_to()).copied().unwrap_or(0),
        })
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String, SourceReadError> {
    // An odd byte count means the final code unit is truncated; surface that
    // rather than letting `chunks_exact` silently drop the dangling byte and
    // accept corrupted input. UTF-16 source is vanishingly rare, so we don't
    // pinpoint a byte offset here (`byte == 0` selects the encoding-agnostic
    // diagnostic message in `encoding_diagnostic`).
    if !bytes.len().is_multiple_of(2) {
        return Err(SourceReadError::InvalidEncoding { offset: 0, byte: 0 });
    }
    let units = bytes.chunks_exact(2).map(|c| {
        let pair = [c[0], c[1]];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .map_err(|_| SourceReadError::InvalidEncoding { offset: 0, byte: 0 })
}

/// Process a single file: read, parse, compute metadata and artifacts.
/// Returns `None` if the file can't be read or converted to a URI.
fn process_workspace_file(path: &Path) -> Option<ProcessedFile> {
    let text = read_source(path).ok()?;
    let uri = Url::from_file_path(path).ok()?;
    let metadata_result = fs::metadata(path).ok()?;

    log::trace!("Scanning file: {}", uri);
    let doc = Document::new_with_uri(&text, None, &uri);

    // Pair `doc.tree` with the analysis text it was parsed from (masked for
    // Rmd/Quarto, raw otherwise) for both metadata extraction and artifact
    // computation, so byte offsets align (#343). The scan currently never sees
    // chunk files (`is_stat_model_extension` excludes Rmd/Quarto extensions),
    // so this is behavior-neutral today; the pairing must stay
    // analysis-consistent in case that ever changes — feeding raw `&text`
    // against a masked tree would mis-slice (and panic on a non-UTF-8 boundary
    // in multibyte prose).
    let analysis_text = doc.analysis_text();

    let cross_file_meta = crate::cross_file::extract_metadata(&analysis_text);

    let artifacts = std::sync::Arc::new(if let Some(tree) = doc.tree.as_ref() {
        crate::cross_file::scope::compute_artifacts_with_metadata(
            &uri,
            tree,
            &analysis_text,
            Some(&cross_file_meta),
        )
    } else {
        crate::cross_file::scope::ScopeArtifacts::default()
    });

    let snapshot =
        crate::cross_file::file_cache::FileSnapshot::with_content_hash(&metadata_result, &text);
    let cross_file_meta = Arc::new(cross_file_meta);

    let cross_file_entry = crate::cross_file::workspace_index::IndexEntry {
        snapshot: snapshot.clone(),
        metadata: cross_file_meta.clone(),
        artifacts: artifacts.clone(),
        indexed_at_version: 0,
    };

    let new_index_entry = crate::workspace_index::IndexEntry {
        contents: doc.contents.clone(),
        tree: doc.tree.clone(),
        loaded_packages: doc.loaded_packages.clone(),
        snapshot,
        metadata: cross_file_meta,
        artifacts,
        indexed_at_version: 0,
    };

    Some(ProcessedFile {
        uri,
        document: doc,
        cross_file_entry,
        new_index_entry,
    })
}

pub fn scan_workspace(folders: &[Url], max_chain_depth: usize) -> WorkspaceScanResult {
    let exclusions = crate::config_file::CompiledWorkspaceExclusions::default();
    scan_workspace_with_exclusions(folders, max_chain_depth, &exclusions)
}

pub fn scan_workspace_with_exclusions(
    folders: &[Url],
    max_chain_depth: usize,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> WorkspaceScanResult {
    use rayon::prelude::*;

    // Get workspace root for path resolution
    let workspace_root = folders.first().cloned();

    // Phase 1: Collect file paths (serial directory walk — fast, I/O-bound)
    let mut file_paths: Vec<PathBuf> = Vec::new();
    for folder in folders {
        log::info!("Scanning folder: {}", folder);
        if let Ok(path) = folder.to_file_path() {
            collect_files_matching_with_exclusions(
                &path,
                &mut file_paths,
                is_stat_model_extension,
                exclusions,
            );
        }
    }

    log::info!(
        "Collected {} file paths for parallel processing",
        file_paths.len()
    );

    // Type aliases for the thread-local accumulators used in fold/reduce.
    type IndexMap = HashMap<Url, Document>;
    type CrossFileMap = HashMap<Url, crate::cross_file::workspace_index::IndexEntry>;
    type NewIndexMap = HashMap<Url, crate::workspace_index::IndexEntry>;

    // Phase 2+3: Process files in parallel and accumulate directly into
    // thread-local HashMaps via fold, then merge with reduce. This avoids
    // an intermediate Vec<ProcessedFile> that would transiently hold all
    // file contents + ASTs and require two extra Url clones per file for
    // the serial insert loop.
    let (index, mut cross_file_entries, mut new_index_entries): (
        IndexMap,
        CrossFileMap,
        NewIndexMap,
    ) = file_paths
        .par_iter()
        .fold(
            || (IndexMap::new(), CrossFileMap::new(), NewIndexMap::new()),
            |(mut idx, mut cfe, mut nie), path| {
                if let Some(item) = process_workspace_file(path) {
                    cfe.insert(item.uri.clone(), item.cross_file_entry);
                    nie.insert(item.uri.clone(), item.new_index_entry);
                    idx.insert(item.uri, item.document);
                }
                (idx, cfe, nie)
            },
        )
        .reduce(
            || (IndexMap::new(), CrossFileMap::new(), NewIndexMap::new()),
            |(mut idx_a, mut cfe_a, mut nie_a), (idx_b, cfe_b, nie_b)| {
                idx_a.extend(idx_b);
                cfe_a.extend(cfe_b);
                nie_a.extend(nie_b);
                (idx_a, cfe_a, nie_a)
            },
        );

    // Second pass: iteratively enrich metadata with inherited_working_directory
    // Track only files that need enrichment to avoid O(n²) behavior
    let mut files_needing_enrichment: HashSet<Url> = new_index_entries
        .iter()
        .filter(|(_, entry)| {
            !entry.metadata.sourced_by.is_empty()
                && entry.metadata.working_directory.is_none()
                && entry.metadata.inherited_working_directory.is_none()
        })
        .map(|(uri, _)| uri.clone())
        .collect();

    for iteration in 0..max_chain_depth {
        if files_needing_enrichment.is_empty() {
            log::trace!(
                "Workspace scan enrichment converged after {} iteration(s)",
                iteration + 1
            );
            break;
        }

        // Build metadata map from current state
        let metadata_map: HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>> =
            new_index_entries
                .iter()
                .map(|(uri, entry)| (uri.clone(), entry.metadata.clone()))
                .collect();

        let mut newly_enriched = Vec::new();

        // Only process files that need enrichment
        for uri in &files_needing_enrichment {
            if let Some(entry) = new_index_entries.get_mut(uri) {
                let old_inherited = entry.metadata.inherited_working_directory.clone();
                let meta = Arc::make_mut(&mut entry.metadata);
                crate::cross_file::enrich_metadata_with_inherited_wd(
                    meta,
                    uri,
                    workspace_root.as_ref(),
                    |parent_uri| metadata_map.get(parent_uri).cloned(),
                    max_chain_depth,
                );
                if entry.metadata.inherited_working_directory != old_inherited {
                    newly_enriched.push(uri.clone());
                }
            }
            // Also update legacy cross_file_entries
            if let Some(entry) = cross_file_entries.get_mut(uri) {
                let meta = Arc::make_mut(&mut entry.metadata);
                crate::cross_file::enrich_metadata_with_inherited_wd(
                    meta,
                    uri,
                    workspace_root.as_ref(),
                    |parent_uri| metadata_map.get(parent_uri).cloned(),
                    max_chain_depth,
                );
            }
        }

        // Remove enriched files from the set
        for uri in &newly_enriched {
            files_needing_enrichment.remove(uri);
        }

        if newly_enriched.is_empty() {
            log::trace!(
                "Workspace scan enrichment converged after {} iteration(s)",
                iteration + 1
            );
            break;
        }
    }

    log::info!(
        "Scanned {} workspace files ({} with cross-file metadata, {} new index entries)",
        index.len(),
        cross_file_entries.len(),
        new_index_entries.len()
    );

    // Package-mode detection is *not* done here. `scan_workspace` used to
    // construct `PackageWorkspace` and a `PackageNamespaceModel` inline —
    // detecting roxygen, parsing NAMESPACE, aggregating roxygen tags per
    // file, and caching per-file roxygen tags — but
    // that logic duplicated `derive_package_state` and the result was
    // unconditionally overwritten by the `apply_package_event(Initial)`
    // call that follows `apply_workspace_index` in `backend.rs`.
    // The canonical derivation is now single-sourced through the event path
    // (`PackageInputs` → `derive_package_state`).

    (index, cross_file_entries, new_index_entries)
}

/// Directories to skip during workspace scanning.
///
/// This is a conservative list of directories that are extremely unlikely to
/// contain user R source files. The workspace scan runs in the background,
/// so the primary goal is to avoid wasting time on directories that would
/// never contain R files.
///
/// Comparison is case-insensitive. This list is also used by the
/// `analysis-stats` CLI (via [`should_skip_directory`]).
const SKIP_DIRECTORIES: &[&str] = &[
    ".git",         // Git internal files
    ".github",      // GitHub Actions workflows (not package code)
    ".svn",         // Subversion internal files
    ".hg",          // Mercurial internal files
    "node_modules", // JavaScript dependencies (can have 100k+ files)
    ".Rproj.user",  // RStudio user-local project state
    "renv",         // renv package library cache
    "packrat",      // packrat package library cache
    ".vscode",      // VS Code settings
    ".idea",        // JetBrains IDE settings
    "target",       // Rust build artifacts
];

/// Check if a directory should be skipped during scanning.
pub(crate) fn should_skip_directory(dir_name: &str) -> bool {
    SKIP_DIRECTORIES
        .iter()
        .any(|skip| dir_name.eq_ignore_ascii_case(skip))
}

fn is_stat_model_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| {
            ext.eq_ignore_ascii_case("r")
                || ext.eq_ignore_ascii_case("jags")
                || ext.eq_ignore_ascii_case("bugs")
                || ext.eq_ignore_ascii_case("stan")
        })
}

// `scan_directory` was replaced by `collect_files_matching` + `process_workspace_file`
// for parallel scanning via rayon. See `scan_workspace`.

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};

    #[test]
    fn decode_source_plain_utf8() {
        assert_eq!(decode_source(b"x <- 1\n".to_vec()).unwrap(), "x <- 1\n");
    }

    #[test]
    fn decode_source_strips_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"x <- 1\n");
        // The BOM must not survive into the parsed content (a leading U+FEFF
        // would otherwise corrupt the first token).
        assert_eq!(decode_source(bytes).unwrap(), "x <- 1\n");
    }

    #[test]
    fn decode_source_decodes_utf16le_bom() {
        // "ab\n" as UTF-16 little-endian, BOM-prefixed.
        let bytes = vec![0xFF, 0xFE, b'a', 0x00, b'b', 0x00, b'\n', 0x00];
        assert_eq!(decode_source(bytes).unwrap(), "ab\n");
    }

    #[test]
    fn decode_source_decodes_utf16be_bom() {
        let bytes = vec![0xFE, 0xFF, 0x00, b'a', 0x00, b'b', 0x00, b'\n'];
        assert_eq!(decode_source(bytes).unwrap(), "ab\n");
    }

    #[test]
    fn decode_source_rejects_truncated_utf16() {
        // UTF-16 LE BOM followed by an odd number of bytes: the final code unit
        // is truncated. We must surface this rather than silently dropping the
        // dangling byte and accepting corrupted input.
        let bytes = vec![0xFF, 0xFE, b'a', 0x00, b'b']; // 'a', then a lone 0x62
        match decode_source(bytes) {
            Err(SourceReadError::InvalidEncoding { byte, .. }) => {
                // byte == 0 selects the encoding-agnostic message (it had a BOM).
                assert_eq!(byte, 0);
            }
            other => panic!("expected InvalidEncoding for truncated UTF-16, got {other:?}"),
        }
    }

    #[test]
    fn decode_source_reports_first_bad_byte_for_latin1() {
        // The real-world case: a non-breaking space (0xA0) after valid ASCII,
        // no BOM. We must point at the offending byte, not silently mangle it.
        let mut bytes = b"x <- 1".to_vec(); // 6 valid bytes
        bytes.push(0xA0); // offset 6: invalid UTF-8 start byte
        bytes.extend_from_slice(b"\n");
        match decode_source(bytes) {
            Err(SourceReadError::InvalidEncoding { offset, byte }) => {
                assert_eq!(offset, 6);
                assert_eq!(byte, 0xA0);
            }
            other => panic!("expected InvalidEncoding, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_source_async_matches_read_source() {
        let tmp = tempfile::TempDir::new().unwrap();

        // UTF-8 BOM is stripped, exactly like the synchronous read_source.
        let bom = tmp.path().join("bom.R");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"x <- 1\n");
        std::fs::write(&bom, bytes).unwrap();
        assert_eq!(read_source_async(&bom).await.unwrap(), "x <- 1\n");

        // UTF-16 LE BOM is decoded.
        let u16_path = tmp.path().join("u16.R");
        std::fs::write(
            &u16_path,
            vec![0xFF, 0xFE, b'a', 0x00, b'b', 0x00, b'\n', 0x00],
        )
        .unwrap();
        assert_eq!(read_source_async(&u16_path).await.unwrap(), "ab\n");

        // A missing file is an Io error, not InvalidEncoding.
        match read_source_async(&tmp.path().join("missing.R")).await {
            Err(SourceReadError::Io(_)) => {}
            other => panic!("expected Io error for a missing file, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn collect_files_matching_skips_symlink_to_skiplisted_dir() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("real.R"), "1\n").unwrap();
        // A skip-listed directory with a source file inside (pruned by name).
        fs::create_dir(tmp.path().join("node_modules")).unwrap();
        fs::write(tmp.path().join("node_modules").join("inner.R"), "1\n").unwrap();
        // A symlink whose own name is NOT skip-listed but whose target IS
        // (`deps -> node_modules`) must also be pruned, or it pulls the whole
        // vendored tree back into the scan via the symlink alias.
        std::os::unix::fs::symlink(tmp.path().join("node_modules"), tmp.path().join("deps"))
            .unwrap();

        let mut out = Vec::new();
        collect_files_matching(tmp.path(), &mut out, is_stat_model_extension);

        // Only real.R: inner.R is unreachable both directly (node_modules entry
        // name) and via the symlink (deps' canonical target name).
        assert_eq!(out.len(), 1, "got {out:?}");
        assert!(out[0].ends_with("real.R"), "got {out:?}");
    }

    #[test]
    fn workspace_scan_skips_project_excluded_directory() {
        use serde_json::json;
        use std::fs;

        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(tmp.path().join("main.R"), "main <- 1\n").unwrap();
        fs::create_dir(tmp.path().join("generated")).unwrap();
        fs::write(tmp.path().join("generated/ignored.R"), "ignored <- 1\n").unwrap();
        let exclusions = crate::config_file::compile_workspace_exclusions(
            &json!({ "workspace": { "exclude": ["generated/**"] } }),
            vec![tmp.path().to_path_buf()],
        );
        let root = tower_lsp::lsp_types::Url::from_file_path(tmp.path()).unwrap();

        let (index, _, _) = scan_workspace_with_exclusions(&[root], 20, &exclusions);
        let indexed_paths: Vec<_> = index
            .keys()
            .filter_map(|uri| uri.to_file_path().ok())
            .collect();

        assert!(
            indexed_paths.iter().any(|path| path.ends_with("main.R")),
            "included file should be indexed; got {indexed_paths:?}"
        );
        assert!(
            !indexed_paths
                .iter()
                .any(|path| path.ends_with("generated/ignored.R")),
            "excluded generated directory must be skipped; got {indexed_paths:?}"
        );
    }

    #[test]
    fn collect_files_matching_negated_exclusion_does_not_prune_reincluded_file() {
        use serde_json::json;
        use std::fs;

        let tmp = tempfile::TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("generated")).unwrap();
        fs::write(tmp.path().join("generated/drop.R"), "drop <- 1\n").unwrap();
        fs::write(tmp.path().join("generated/keep.R"), "keep <- 1\n").unwrap();
        let exclusions = crate::config_file::compile_workspace_exclusions(
            &json!({ "workspace": { "exclude": ["generated/**", "!generated/keep.R"] } }),
            vec![tmp.path().to_path_buf()],
        );

        let mut out = Vec::new();
        collect_files_matching_with_exclusions(
            tmp.path(),
            &mut out,
            is_stat_model_extension,
            &exclusions,
        );

        assert_eq!(out.len(), 1, "got {out:?}");
        assert!(out[0].ends_with("generated/keep.R"), "got {out:?}");
    }

    // Include workspace scanning tests
    include!("state_tests.rs");

    #[test]
    fn test_should_skip_directory() {
        assert!(should_skip_directory(".git"));
        assert!(should_skip_directory("node_modules"));
        assert!(should_skip_directory("renv"));
        assert!(should_skip_directory("target"));
        assert!(!should_skip_directory("R"));
        assert!(!should_skip_directory("src"));
        assert!(!should_skip_directory("data"));
    }

    #[test]
    fn test_document_apply_change_ascii() {
        let mut doc = Document::new("hello world", None);

        // Replace "world" with "rust"
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 0,
                    character: 6,
                },
                end: Position {
                    line: 0,
                    character: 11,
                },
            }),
            range_length: None,
            text: "rust".to_string(),
        });

        assert_eq!(doc.text(), "hello rust");
    }

    #[test]
    fn test_document_apply_change_utf16_emoji() {
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let mut doc = Document::new("a🎉b", None);

        // Insert "x" after the emoji (UTF-16 position 3 = after 'a' + 2 for emoji)
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 0,
                    character: 3,
                },
                end: Position {
                    line: 0,
                    character: 3,
                },
            }),
            range_length: None,
            text: "x".to_string(),
        });

        assert_eq!(doc.text(), "a🎉xb");
    }

    #[test]
    fn test_document_apply_change_utf16_cjk() {
        // CJK characters are 3 bytes in UTF-8, 1 UTF-16 code unit each
        let mut doc = Document::new("a中b", None);

        // Insert "x" after '中' (UTF-16 position 2)
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 0,
                    character: 2,
                },
                end: Position {
                    line: 0,
                    character: 2,
                },
            }),
            range_length: None,
            text: "x".to_string(),
        });

        assert_eq!(doc.text(), "a中xb");
    }

    #[test]
    fn test_document_apply_change_utf16_delete_emoji() {
        let mut doc = Document::new("a🎉b", None);

        // Delete the emoji (UTF-16 positions 1-3)
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 0,
                    character: 1,
                },
                end: Position {
                    line: 0,
                    character: 3,
                },
            }),
            range_length: None,
            text: "".to_string(),
        });

        assert_eq!(doc.text(), "ab");
    }

    #[test]
    fn test_document_apply_change_multiline_utf16() {
        let mut doc = Document::new("line1\n🎉line2", None);

        // Replace "line2" on second line (after emoji)
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 1,
                    character: 2,
                }, // After emoji (2 UTF-16 units)
                end: Position {
                    line: 1,
                    character: 7,
                }, // End of "line2"
            }),
            range_length: None,
            text: "test".to_string(),
        });

        assert_eq!(doc.text(), "line1\n🎉test");
    }

    // ========================================================================
    // Masked analysis representation for Rmd/Quarto documents (Task 2)
    // ========================================================================

    /// True iff the tree contains an `identifier` node whose text equals `name`.
    /// Slices against `text`, which MUST be the text the tree was parsed from.
    fn tree_has_identifier(tree: &Tree, text: &str, name: &str) -> bool {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "identifier" && &text[node.byte_range()] == name {
                return true;
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        false
    }

    fn rmd_uri() -> Url {
        Url::parse("file:///tmp/report.Rmd").unwrap()
    }

    fn r_uri() -> Url {
        Url::parse("file:///tmp/script.R").unwrap()
    }

    #[test]
    fn rmd_document_tree_is_parsed_from_masked_text() {
        // Prose + YAML + a valid R chunk. A raw parse would treat the prose and
        // YAML as garbage and produce ERROR nodes; the masked parse must not.
        let src = "---\ntitle: Demo\n---\n\nSome prose here.\n\n```{r}\nx <- 1\nf <- function(a) a + 1\n```\n\nMore prose.\n";
        let doc = Document::new_with_uri(src, None, &rmd_uri());
        assert_eq!(doc.chunk_kind, ChunkKind::Rmd);
        let tree = doc.tree.as_ref().expect("Rmd doc should have a parse tree");
        assert!(
            !tree.root_node().has_error(),
            "masked-derived tree for an Rmd doc with valid R chunks must have no ERROR nodes"
        );
        // The chunk symbol must be visible in the masked tree, sliced against
        // the analysis text (which is what the tree was parsed from).
        let analysis = doc.analysis_text();
        assert!(tree_has_identifier(tree, &analysis, "f"));
        assert!(tree_has_identifier(tree, &analysis, "x"));
    }

    #[test]
    fn analysis_text_is_masked_for_rmd_and_raw_for_plain_r() {
        let rmd_src = "prose\n```{r}\nx <- 1\n```\n";
        let rmd_doc = Document::new_with_uri(rmd_src, None, &rmd_uri());
        assert_eq!(rmd_doc.analysis_text(), crate::chunks::mask_to_r(rmd_src));
        // The raw contents are untouched.
        assert_eq!(rmd_doc.text(), rmd_src);

        let r_src = "x <- 1\nf <- function() 2\n";
        let r_doc = Document::new_with_uri(r_src, None, &r_uri());
        assert_eq!(r_doc.analysis_text(), r_doc.text());
        assert_eq!(r_doc.analysis_text(), r_src);
    }

    #[test]
    fn rmd_loaded_packages_come_from_chunk_bodies_only() {
        // `library(dplyr)` lives inside an R chunk; a prose line mentions
        // `library(ignored)` and a Python chunk loads nothing R-relevant.
        let src = "Intro mentions library(ignored) inline.\n\n```{r}\nlibrary(dplyr)\nx <- 1\n```\n\n```{python}\nimport os\n```\n";
        let doc = Document::new_with_uri(src, None, &rmd_uri());
        assert_eq!(doc.loaded_packages, vec!["dplyr".to_string()]);
    }

    #[test]
    fn rmd_apply_change_inside_chunk_reparses_from_masked_text() {
        let src = "prose\n```{r}\nx <- 1\n```\n";
        let mut doc = Document::new_with_uri(src, Some(1), &rmd_uri());
        let v0 = doc.revision;

        // Insert a new statement on the body line: replace "x <- 1" with
        // "x <- 1\nnewsym <- 2" (line 2, full-line range).
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 2,
                    character: 0,
                },
                end: Position {
                    line: 2,
                    character: 6,
                },
            }),
            range_length: None,
            text: "x <- 1\nnewsym <- 2".to_string(),
        });

        // Raw contents updated.
        assert!(doc.text().contains("newsym <- 2"));
        // masked_text re-derived and consistent with the raw contents.
        let analysis = doc.analysis_text();
        assert_eq!(analysis, crate::chunks::mask_to_r(&doc.text()));
        // Tree reparsed from the masked text: no ERROR nodes, new symbol present.
        let tree = doc.tree.as_ref().expect("tree after change");
        assert!(
            !tree.root_node().has_error(),
            "no ERROR nodes after in-chunk edit"
        );
        assert!(tree_has_identifier(tree, &analysis, "newsym"));
        // Revision bumped.
        assert!(doc.revision > v0);
    }

    #[test]
    fn rmd_apply_change_to_prose_keeps_tree_clean() {
        let src = "prose line\n```{r}\nx <- 1\n```\n";
        let mut doc = Document::new_with_uri(src, Some(1), &rmd_uri());

        // Edit the prose on line 0 only.
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 10,
                },
            }),
            range_length: None,
            text: "different prose entirely".to_string(),
        });

        assert!(doc.text().contains("different prose entirely"));
        // Prose is still blanked in the analysis text.
        let analysis = doc.analysis_text();
        assert_eq!(analysis, crate::chunks::mask_to_r(&doc.text()));
        let tree = doc.tree.as_ref().expect("tree after prose change");
        assert!(
            !tree.root_node().has_error(),
            "prose edits must not introduce ERROR nodes"
        );
        // The R chunk body is still on line 2 (geometry preserved) and the
        // symbol is still visible.
        assert!(tree_has_identifier(tree, &analysis, "x"));
    }

    #[test]
    fn plain_r_apply_change_uses_raw_text_for_analysis() {
        // Regression: a plain .R doc's analysis_text tracks the raw contents and
        // the tree continues to reflect edits.
        let mut doc = Document::new_with_uri("x <- 1\n", Some(1), &r_uri());
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 1,
                    character: 0,
                },
                end: Position {
                    line: 1,
                    character: 0,
                },
            }),
            range_length: None,
            text: "yvar <- 2\n".to_string(),
        });
        assert_eq!(doc.text(), "x <- 1\nyvar <- 2\n");
        assert_eq!(doc.analysis_text(), doc.text());
        let tree = doc.tree.as_ref().unwrap();
        assert!(tree_has_identifier(tree, &doc.analysis_text(), "yvar"));
    }

    #[test]
    fn test_utf16_offset_to_char_offset_ascii() {
        let line = "hello";
        assert_eq!(utf16_offset_to_char_offset(line, 0), 0);
        assert_eq!(utf16_offset_to_char_offset(line, 3), 3);
        assert_eq!(utf16_offset_to_char_offset(line, 5), 5);
    }

    #[test]
    fn test_utf16_offset_to_char_offset_emoji() {
        // 🎉 is 2 UTF-16 code units, 1 char
        let line = "a🎉b";
        assert_eq!(utf16_offset_to_char_offset(line, 0), 0); // before 'a'
        assert_eq!(utf16_offset_to_char_offset(line, 1), 1); // after 'a', before emoji
        assert_eq!(utf16_offset_to_char_offset(line, 3), 2); // after emoji (1 + 2 UTF-16 units)
        assert_eq!(utf16_offset_to_char_offset(line, 4), 3); // after 'b'
    }

    #[test]
    fn test_utf16_offset_to_char_offset_cjk() {
        // CJK characters are 1 UTF-16 code unit each
        let line = "a中b";
        assert_eq!(utf16_offset_to_char_offset(line, 0), 0); // before 'a'
        assert_eq!(utf16_offset_to_char_offset(line, 1), 1); // after 'a'
        assert_eq!(utf16_offset_to_char_offset(line, 2), 2); // after '中'
        assert_eq!(utf16_offset_to_char_offset(line, 3), 3); // after 'b'
    }

    // ============================================================================
    // SymbolConfig Tests
    // **Validates: Requirements 11.1, 11.2, 11.3**
    // ============================================================================

    #[test]
    fn test_symbol_config_default() {
        // **Validates: Requirement 11.1**
        // The default value for workspace_max_results SHALL be 1000
        let config = SymbolConfig::default();
        assert_eq!(config.workspace_max_results, 1000);
        assert_eq!(
            config.workspace_max_results,
            SymbolConfig::DEFAULT_WORKSPACE_MAX_RESULTS
        );
    }

    #[test]
    fn test_symbol_config_constants() {
        // **Validates: Requirement 11.3**
        // Valid range is 100-10000
        assert_eq!(SymbolConfig::MIN_WORKSPACE_MAX_RESULTS, 100);
        assert_eq!(SymbolConfig::MAX_WORKSPACE_MAX_RESULTS, 10000);
        assert_eq!(SymbolConfig::DEFAULT_WORKSPACE_MAX_RESULTS, 1000);
    }

    #[test]
    fn test_symbol_config_with_max_results_valid() {
        // **Validates: Requirement 11.3**
        // Values within range should be accepted as-is
        let config = SymbolConfig::with_max_results(500);
        assert_eq!(config.workspace_max_results, 500);

        let config = SymbolConfig::with_max_results(100);
        assert_eq!(config.workspace_max_results, 100);

        let config = SymbolConfig::with_max_results(10000);
        assert_eq!(config.workspace_max_results, 10000);

        let config = SymbolConfig::with_max_results(5000);
        assert_eq!(config.workspace_max_results, 5000);
    }

    #[test]
    fn test_symbol_config_with_max_results_clamp_low() {
        // **Validates: Requirement 11.3**
        // Values below minimum should be clamped to 100
        let config = SymbolConfig::with_max_results(50);
        assert_eq!(config.workspace_max_results, 100);

        let config = SymbolConfig::with_max_results(0);
        assert_eq!(config.workspace_max_results, 100);

        let config = SymbolConfig::with_max_results(99);
        assert_eq!(config.workspace_max_results, 100);
    }

    #[test]
    fn test_symbol_config_with_max_results_clamp_high() {
        // **Validates: Requirement 11.3**
        // Values above maximum should be clamped to 10000
        let config = SymbolConfig::with_max_results(20000);
        assert_eq!(config.workspace_max_results, 10000);

        let config = SymbolConfig::with_max_results(10001);
        assert_eq!(config.workspace_max_results, 10000);

        let config = SymbolConfig::with_max_results(usize::MAX);
        assert_eq!(config.workspace_max_results, 10000);
    }

    #[test]
    fn test_symbol_config_clone() {
        let config = SymbolConfig::with_max_results(750);
        let cloned = config.clone();
        assert_eq!(cloned.workspace_max_results, 750);
        assert_eq!(
            cloned.hierarchical_document_symbol_support,
            config.hierarchical_document_symbol_support
        );
    }

    #[test]
    fn test_symbol_config_debug() {
        let config = SymbolConfig::default();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("SymbolConfig"));
        assert!(debug_str.contains("workspace_max_results"));
        assert!(debug_str.contains("1000"));
        assert!(debug_str.contains("hierarchical_document_symbol_support"));
    }

    // ============================================================================
    // SymbolConfig hierarchical_document_symbol_support Tests
    // **Validates: Requirements 1.1, 1.2**
    // ============================================================================

    #[test]
    fn test_symbol_config_hierarchical_support_default_false() {
        // **Validates: Requirements 1.1, 1.2**
        // Default should be false (flat response) until client capability is detected
        let config = SymbolConfig::default();
        assert!(!config.hierarchical_document_symbol_support);
    }

    #[test]
    fn test_symbol_config_with_max_results_hierarchical_default_false() {
        // **Validates: Requirements 1.1, 1.2**
        // with_max_results should also default hierarchical support to false
        let config = SymbolConfig::with_max_results(500);
        assert!(!config.hierarchical_document_symbol_support);
    }

    #[test]
    fn test_symbol_config_hierarchical_support_can_be_set() {
        // **Validates: Requirements 1.1, 1.2**
        // The field should be settable after initialization
        let mut config = SymbolConfig::default();
        assert!(!config.hierarchical_document_symbol_support);

        config.hierarchical_document_symbol_support = true;
        assert!(config.hierarchical_document_symbol_support);

        config.hierarchical_document_symbol_support = false;
        assert!(!config.hierarchical_document_symbol_support);
    }

    #[test]
    fn test_build_package_scope_snapshot_scales_budget_with_seed_count() {
        // Regression test for the multi-seed BFS budget scaling in
        // build_package_scope_snapshot. Without scaling, the shared
        // max_transitive_dependents_visited budget would truncate the BFS
        // before reaching every chain's deepest ancestor in workspaces with
        // many open files — defeating the PR's goal of finding inherited
        // packages from closed parents.
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        const NUM_CHAINS: usize = 30;
        const CHAIN_LEN: usize = 10;
        const PER_SEED_BUDGET: usize = 200;

        let mut state = WorldState::new();
        state.cross_file_config.max_transitive_dependents_visited = PER_SEED_BUDGET;
        // 30 × 10 = 300 nodes total; with an unscaled shared budget of 200
        // the BFS would truncate.
        assert!(NUM_CHAINS * CHAIN_LEN > state.cross_file_config.max_transitive_dependents_visited);

        let workspace_root = Url::parse("file:///project").unwrap();
        let chain_url = |chain: usize, level: usize| -> Url {
            Url::parse(&format!("file:///project/c{}_l{}.R", chain, level)).unwrap()
        };

        let mut seeds: Vec<(Url, u32)> = Vec::with_capacity(NUM_CHAINS);
        for chain in 0..NUM_CHAINS {
            for level in 0..CHAIN_LEN - 1 {
                let parent = chain_url(chain, level);
                let child_path = format!("c{}_l{}.R", chain, level + 1);
                let meta = CrossFileMetadata {
                    sources: vec![ForwardSource {
                        path: child_path,
                        line: 1,
                        column: 0,
                        is_directive: false,
                        local: false,
                        chdir: false,
                        is_sys_source: false,
                        sys_source_global_env: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                state
                    .cross_file_graph
                    .update_file(&parent, &meta, Some(&workspace_root), |_| None);
            }
            seeds.push((chain_url(chain, 0), 0));
        }

        let snapshot = state.build_package_scope_snapshot(&seeds);

        // Walk each chain root → leaf via the snapshot's subgraph.
        // If the budget truncated, get_dependencies returns empty mid-walk.
        for chain in 0..NUM_CHAINS {
            let mut current = chain_url(chain, 0);
            for level in 0..CHAIN_LEN - 1 {
                let deps = snapshot.graph.get_dependencies(&current);
                assert!(
                    !deps.is_empty(),
                    "chain {} truncated at level {}: node {} missing from snapshot subgraph",
                    chain,
                    level,
                    current
                );
                current = deps[0].to.clone();
            }
            assert_eq!(current, chain_url(chain, CHAIN_LEN - 1));
        }
    }

    /// Finding 3 (#343): `process_workspace_file` must pair the document's
    /// `tree` with its **analysis text** (masked for Rmd) when extracting
    /// metadata and computing artifacts. The workspace scan currently never
    /// hands this function a chunk file (`is_stat_model_extension` excludes
    /// `.rmd`/`.qmd`), but the raw/masked pairing must stay analysis-consistent
    /// so a future scan-scope change can't silently mis-slice. Multibyte prose
    /// makes the regression a hard failure (mid-char slice), not a quiet one.
    #[test]
    fn process_workspace_file_pairs_masked_tree_with_masked_text() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("report.Rmd");
        // Multibyte prose contains `prose_symbol`; the chunk defines
        // `chunk_symbol`. A masked-consistent scan yields the chunk symbol only.
        let src =
            "Prélude éééé prose_symbol éééé.\n\n```{r}\nchunk_symbol <- function(a) a + 1\n```\n";
        fs::write(&path, src).unwrap();

        let processed = process_workspace_file(&path).expect("process_workspace_file must succeed");

        // Artifacts derive from the masked analysis text: the chunk symbol is
        // exported, the prose symbol is not.
        let interface = &processed.new_index_entry.artifacts.exported_interface;
        assert!(
            interface.keys().any(|k| &**k == "chunk_symbol"),
            "chunk-defined symbol must be in the exported interface, got {:?}",
            interface.keys().collect::<Vec<_>>()
        );
        assert!(
            !interface.keys().any(|k| &**k == "prose_symbol"),
            "prose token must NOT be in the exported interface, got {:?}",
            interface.keys().collect::<Vec<_>>()
        );
        // The document's tree must slice cleanly against its analysis text
        // (would panic on the multibyte prose if paired with raw text).
        let doc_tree = processed
            .document
            .tree
            .as_ref()
            .expect("Rmd doc must have a tree");
        let analysis = processed.document.analysis_text();
        assert!(
            tree_has_identifier(doc_tree, &analysis, "chunk_symbol"),
            "masked tree must contain the chunk-defined identifier"
        );
    }

    /// `resolve_system_file_in_workspace` is what every library-swap site
    /// (startup post-ready retry, `raven.refreshPackages`,
    /// `reconcile_after_config_recompute`) calls to re-resolve deferred
    /// `system.file()` sources once `lib_paths` become available. Exercise the
    /// full wiring at the `WorldState` level: a workspace-index entry whose
    /// source was deferred (indexed while `lib_paths` was empty) must resolve
    /// in place after the library swap, not just in a detached metadata value.
    #[test]
    fn resolve_system_file_in_workspace_re_resolves_after_library_swap() {
        use crate::cross_file::file_cache::FileSnapshot;
        use crate::cross_file::source_detect::SystemFileCall;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        use crate::workspace_index::IndexEntry;
        use std::sync::Arc;

        // "otherpkg" installed at libdir/otherpkg/helper.R (installed layout).
        let libdir = tempfile::tempdir().unwrap();
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();

        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        let metadata = CrossFileMetadata {
            sources: vec![ForwardSource {
                system_file: Some(SystemFileCall {
                    parts: vec!["helper.R".to_string()],
                    package: "otherpkg".to_string(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let entry = IndexEntry {
            contents: ropey::Rope::from_str(
                "source(system.file(\"helper.R\", package = \"otherpkg\"))\n",
            ),
            tree: None,
            loaded_packages: Vec::new(),
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: Some(1),
            },
            metadata: Arc::new(metadata),
            artifacts: Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 1,
        };

        let mut state = WorldState::new();
        state.workspace_index_new.insert(uri.clone(), entry);

        // Before the swap: lib_paths is empty, so the source stays deferred.
        state.resolve_system_file_in_workspace();
        let deferred = state
            .workspace_index_new
            .get(&uri)
            .expect("entry still indexed");
        assert!(
            deferred.metadata.sources[0].system_file.is_some(),
            "source must stay deferred while lib_paths is empty"
        );

        // The library swap: replace the Arc with a library whose lib_paths
        // contain the installed package — the same shape as the production
        // swap sites.
        let mut swapped = crate::package_library::PackageLibrary::new_empty();
        swapped.set_lib_paths(vec![libdir.path().to_path_buf()]);
        state.package_library = Arc::new(swapped);
        state.resolve_system_file_in_workspace();

        let resolved = state
            .workspace_index_new
            .get(&uri)
            .expect("entry still indexed");
        assert_eq!(resolved.metadata.sources.len(), 1);
        assert!(
            resolved.metadata.sources[0].system_file.is_some(),
            "system_file must be retained after resolution so package \
             lifecycle events can re-resolve"
        );
        let resolved_uri = resolved.metadata.sources[0]
            .resolved_uri
            .as_ref()
            .expect("resolved_uri must be set in the stored index entry");
        let resolved_path = resolved_uri.to_file_path().unwrap();
        assert!(
            resolved_path.ends_with("otherpkg/helper.R"),
            "must resolve into the new lib path, got {resolved_path:?}"
        );
    }

    /// Open buffers are authoritative for their canonical alias roots too. When
    /// a package event resolves a deferred `system.file()` source on a symlink
    /// spelling, the canonical graph node must be rebuilt from the same live
    /// metadata so canonical parents see the new external edge.
    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_system_file_in_workspace_rebuilds_canonical_alias_open_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();

        let content = "source(system.file(\"helper.R\", package = \"otherpkg\"))\n";
        std::fs::write(real.join("child.R"), content).unwrap();

        let libdir = tempfile::TempDir::new().unwrap();
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();

        let link_uri = Url::from_file_path(tmp.path().join("link").join("child.R")).unwrap();
        let canonical_uri = Url::from_file_path(real.join("child.R")).unwrap();
        let helper_uri = Url::from_file_path(pkg_dir.join("helper.R")).unwrap();

        let mut state = WorldState::new();
        state
            .workspace_folders
            .push(Url::from_file_path(tmp.path()).unwrap());
        state.open_document_with_language_id(link_uri.clone(), content, Some(1), Some("r"));
        state
            .document_store
            .open_with_metadata(
                link_uri.clone(),
                content,
                1,
                crate::chunks::ChunkKind::R,
                crate::cross_file::extract_metadata(content),
            )
            .await;

        assert_eq!(
            state.open_document_uri_for_authoritative_uri(&canonical_uri),
            Some(link_uri.clone()),
            "canonical child URI must resolve to the symlink-open buffer"
        );

        state.resolve_system_file_in_workspace();
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&canonical_uri)
                .is_empty(),
            "precondition: canonical root has no resolved edge while lib_paths are empty"
        );

        let mut swapped = crate::package_library::PackageLibrary::new_empty();
        swapped.set_lib_paths(vec![libdir.path().to_path_buf()]);
        state.package_library = Arc::new(swapped);
        let changed = state.resolve_system_file_in_workspace();

        assert!(
            changed.iter().any(|uri| uri == &link_uri),
            "the raw open URI should report changed system.file resolution"
        );
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&link_uri)
                .iter()
                .any(|edge| edge.to == helper_uri),
            "raw open graph root must gain the resolved system.file edge"
        );
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&canonical_uri)
                .iter()
                .any(|edge| edge.to == helper_uri),
            "canonical alias graph root must gain the resolved system.file edge"
        );
    }

    // ========================================================================
    // extract_data_packages unit tests (issue #429)
    // ========================================================================

    #[test]
    fn extract_data_packages_double_quote() {
        // data(api, package = "survey") → ["survey"]
        let doc = Document::new("data(api, package = \"survey\")\n", None);
        assert_eq!(doc.data_packages, vec!["survey".to_string()]);
    }

    #[test]
    fn extract_data_packages_namespace_single_quote() {
        // utils::data(x, package = 'foo') → ["foo"]
        let doc = Document::new("utils::data(x, package = 'foo')\n", None);
        assert_eq!(doc.data_packages, vec!["foo".to_string()]);
    }

    #[test]
    fn extract_data_packages_bare_no_package_arg() {
        // data(api) — no package= argument → empty
        let doc = Document::new("data(api)\n", None);
        assert!(
            doc.data_packages.is_empty(),
            "bare data() call must not produce any package names; got: {:?}",
            doc.data_packages
        );
    }

    #[test]
    fn extract_data_packages_non_literal_package_arg() {
        // data(api, package = pkg_var) — variable, not a string literal → empty
        let doc = Document::new("data(api, package = pkg_var)\n", None);
        assert!(
            doc.data_packages.is_empty(),
            "non-literal package= must not produce any package names; got: {:?}",
            doc.data_packages
        );
    }

    #[test]
    fn extract_data_packages_multi_call() {
        // Two data() calls in one document: both packages must be collected.
        let doc = Document::new(
            "data(a, package = \"p1\")\ndata(b, package = \"p2\")\n",
            None,
        );
        // The function does NOT deduplicate; assert the actual contract: both
        // packages appear in order (one entry per call site).
        assert_eq!(
            doc.data_packages,
            vec!["p1".to_string(), "p2".to_string()],
            "both package names must appear; got: {:?}",
            doc.data_packages
        );
    }

    #[test]
    fn extract_data_packages_recomputed_on_edit() {
        // Editing the document must recompute data_packages.
        let mut doc = Document::new("data(x, package = \"aaa\")\n", None);
        assert_eq!(doc.data_packages, vec!["aaa".to_string()]);

        // Full-document replacement (no range = full sync).
        doc.apply_change(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "data(x, package = \"bbb\")\n".to_string(),
        });
        assert_eq!(
            doc.data_packages,
            vec!["bbb".to_string()],
            "data_packages must follow the edit; got: {:?}",
            doc.data_packages
        );
    }
}
