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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::content_provider::ContentProvider;
use crate::indentation::IndentationStyle;
use ropey::Rope;
use tower_lsp::lsp_types::TextDocumentContentChangeEvent;

static NEXT_OPEN_INSTALL_INTENT_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_OPEN_CLOSE_INTENT_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_OPEN_LIFECYCLE_INTENT_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_WORKSPACE_SCAN_INTENT_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_WORKSPACE_SCAN_COMMIT_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_PACKAGE_LIBRARY_INSTALL_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_LIBRARY_REPLACEMENT_INTENT_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_LIBRARY_ROUTING_RECONCILE_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SYSTEM_FILE_ROUTING_OWNER_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_LIBPATH_WATCHER_OWNER_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_ANALYSIS_TRANSFER_FINALIZATION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SYSTEM_FILE_COMMIT_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_PACKAGE_SEED_INSTALL_ID: AtomicU64 = AtomicU64::new(1);

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
#[derive(Debug, Clone)]
pub struct IndentationSettings {
    /// Whether Tier 2 AST-aware indentation is enabled.
    pub enabled: bool,
    /// Parenthesized-argument formatting style.
    pub argument_style: IndentationStyle,
    /// Infix-operator continuation formatting style.
    pub infix_continuation_style: IndentationStyle,
}

impl Default for IndentationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            argument_style: IndentationStyle::Aligned,
            infix_continuation_style: IndentationStyle::Aligned,
        }
    }
}

/// One document's editor options synced by the client via
/// `raven/documentIndentUnitsChanged` (`WorldState::per_document_indent_options`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DocumentIndentOptions {
    /// Resolved `editor.tabSize` in `"auto"` mode. Patches the base
    /// `LintConfig::indentation_unit` in
    /// `effective_lint_config_for_document`; absent, the workspace-wide unit
    /// stays authoritative.
    pub indent_unit: Option<u32>,
    /// Resolved `editor.insertSpaces` (issue #614). Consumed only by
    /// `resolved_indentation_producer_policy`: a synced `false` marks a
    /// tabs-mode editor where the Tier 2 judge stands down, so the
    /// indentation lint's mismatch advice must not attribute a column to it.
    /// Absent keeps the advice available — the CLI's `[indentation]` project
    /// section implies an intentional producer policy with no editor to
    /// contradict it.
    pub insert_spaces: Option<bool>,
    /// Resolved `editor.formatOnType`. A synced `false` means VS Code never
    /// sends the Enter request, so mismatch advice cannot attribute an indent
    /// column to Raven's producer. Absent keeps legacy clients' behavior.
    pub format_on_type: Option<bool>,
}

use tower_lsp::lsp_types::Url;
use tree_sitter::InputEdit;
use tree_sitter::Parser;
use tree_sitter::Point;
use tree_sitter::Tree;

use crate::chunks::{ChunkKind, classify_chunk_document, classify_chunk_document_for};
use crate::content_provider::{DefaultContentProvider, OpenDocumentsView};
use crate::cross_file::revalidation::{CrossFileDiagnosticsGate, DiagnosticsEpoch};
use crate::cross_file::{
    CrossFileActivityState, CrossFileConfig, CrossFileFileCache, CrossFileRevalidationState,
    DependencyGraph, MetadataCache,
};
use crate::file_type::{FileType, file_type_from_language_id_or_uri, file_type_from_uri};
use crate::open_document_store::{
    AnalysisGeneration, OpenDocumentRecord, OpenDocumentStore, OpenRecordToken,
    PreparedOpenDocument, PreparedOpenMetadataReplacement,
};
use crate::package_library::{PackageLibrary, PackageLibraryRoutingLease};
use crate::parameter_resolver::SignatureCache;
use crate::workspace_index::{
    ClosedProvenance, ClosedRecordToken, CompleteRefreshToken, EnrichmentClaim, IndexEntry,
    PreparedWorkspaceIndexTargetedBatch, WorkspaceIndex, WorkspaceIndexTargetedChanges,
};

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
/// For plain R and JAGS documents `analysis_text()` *is* `text()`. Stan also
/// has two views when it contains recognized full-line Raven directives: those
/// lines are blanked geometry-preservingly before parsing.
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
    /// raw contents) and directive-bearing Stan documents, or `None` when no
    /// mask is needed. The `tree` is parsed from this when present. Kept in sync
    /// with `contents` by `apply_change`. Exposed read-only via
    /// [`analysis_text()`](Document::analysis_text).
    masked_text: Option<String>,
    pub version: Option<i32>,
    pub revision: u64,
}

fn point_after_insert(start: Point, inserted: &str) -> Point {
    match inserted.rfind('\n') {
        Some(last_newline) => Point::new(
            start.row + inserted.bytes().filter(|byte| *byte == b'\n').count(),
            inserted.len() - last_newline - 1,
        ),
        None => Point::new(start.row, start.column + inserted.len()),
    }
}

fn rope_end_point(contents: &Rope) -> Point {
    let row = contents.len_lines().saturating_sub(1);
    Point::new(row, contents.line(row).len_bytes())
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
    /// `tree` is parsed from the right text (masked when required) and
    /// `loaded_packages` is extracted from the same `(tree, text)` pair.
    pub(crate) fn new_with_kind(
        text: &str,
        version: Option<i32>,
        file_type: FileType,
        chunk_kind: ChunkKind,
    ) -> Self {
        let contents = Rope::from_str(text);
        // Mask Rmd/Quarto bodies so the R parser only sees real R code. JAGS
        // keeps raw text; Stan masks recognized Raven directives. Routed through the shared
        // `masked_analysis_text` chokepoint so this and the open-document authority can
        // never derive divergent analysis views.
        let masked_text = analysis_mask(chunk_kind, file_type, text);
        let analysis_text = masked_text.as_deref().unwrap_or(text);
        let tree = parse_document_text(analysis_text, file_type);
        // Extract from the SAME text the tree was parsed from, so `library()`
        // calls inside chunks are found and prose mentions are not.
        let loaded_packages = if file_type == FileType::R {
            extract_loaded_packages(&tree, analysis_text)
        } else {
            Vec::new()
        };
        let data_packages = if file_type == FileType::R {
            extract_data_packages(&tree, analysis_text)
        } else {
            Vec::new()
        };
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

    /// Apply one LSP content change and rebuild the derived analysis view.
    ///
    /// Single-change callers keep the historical API. `didChange` handlers
    /// should prefer [`Self::apply_changes`] so a notification containing
    /// several sequential edits reparses only the final text.
    pub fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
        self.apply_changes(std::iter::once(change));
    }

    /// Apply a sequential LSP change batch, then rebuild derived data once.
    ///
    /// LSP ranges in one notification are interpreted in order, against the
    /// text produced by the preceding change. The raw rope therefore still
    /// mutates once per event, while masking, parsing, and package extraction
    /// run exactly once for the notification's final text. `revision` advances
    /// once per event to preserve the pre-consolidation freshness identity.
    pub fn apply_changes(
        &mut self,
        changes: impl IntoIterator<Item = TextDocumentContentChangeEvent>,
    ) {
        let mut applied = 0_u64;
        // JAGS analysis text is always the raw text, so its stored tree can be
        // edited with the exact sequential LSP changes before one final parse.
        // Rmd masking and Stan directive masking make that unsafe for those
        // languages; R and Stan retain their existing full-reparse path.
        let mut incremental_jags_tree = (self.file_type == FileType::Jags)
            .then(|| self.tree.clone())
            .flatten();

        for change in changes {
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

                let edit = incremental_jags_tree.as_ref().map(|_| {
                    let start_byte = self.contents.char_to_byte(start_idx);
                    let old_end_byte = self.contents.char_to_byte(end_idx);
                    let start_column = start_line_text
                        .chars()
                        .take(start_char)
                        .map(char::len_utf8)
                        .sum();
                    let old_end_column = end_line_text
                        .chars()
                        .take(end_char)
                        .map(char::len_utf8)
                        .sum();
                    let start_position = Point::new(start_line, start_column);
                    InputEdit {
                        start_byte,
                        old_end_byte,
                        new_end_byte: start_byte + change.text.len(),
                        start_position,
                        old_end_position: Point::new(end_line, old_end_column),
                        new_end_position: point_after_insert(start_position, &change.text),
                    }
                });

                self.contents.remove(start_idx..end_idx);
                self.contents.insert(start_idx, &change.text);
                if let (Some(tree), Some(edit)) = (incremental_jags_tree.as_mut(), edit.as_ref()) {
                    tree.edit(edit);
                }
            } else {
                // Full document sync
                if let Some(tree) = incremental_jags_tree.as_mut() {
                    tree.edit(&InputEdit {
                        start_byte: 0,
                        old_end_byte: self.contents.len_bytes(),
                        new_end_byte: change.text.len(),
                        start_position: Point::new(0, 0),
                        old_end_position: rope_end_point(&self.contents),
                        new_end_position: point_after_insert(Point::new(0, 0), &change.text),
                    });
                }
                self.contents = Rope::from_str(&change.text);
            }

            applied = applied.wrapping_add(1);
        }

        if applied == 0 {
            return;
        }

        self.revision = self.revision.wrapping_add(applied);

        let raw_text = self.contents.to_string();
        // Re-derive the analysis text and parse the tree from it.
        //
        // R and Stan retain a full final-text parse. In particular, the
        // previous Rmd tree's byte offsets reference OLD masked text whereas
        // LSP edit ranges address raw text, so applying those edits to that
        // tree would corrupt it. JAGS has no mask: the edited prior tree above
        // is safe to reuse for this one final parse.
        self.masked_text = analysis_mask(self.chunk_kind, self.file_type, &raw_text);
        // `Some(masked)` for Rmd or directive-bearing Stan, `None` when the
        // analysis text equals raw source.
        let analysis_text = self.masked_text.as_deref().unwrap_or(&raw_text);

        self.tree = parse_document_text_with_old_tree(
            analysis_text,
            self.file_type,
            incremental_jags_tree.as_ref(),
        );
        if self.file_type == FileType::R {
            self.loaded_packages = extract_loaded_packages(&self.tree, analysis_text);
            self.data_packages = extract_data_packages(&self.tree, analysis_text);
        } else {
            self.loaded_packages.clear();
            self.data_packages.clear();
        }
    }

    pub fn text(&self) -> String {
        self.contents.to_string()
    }

    /// The text the [`tree`](Document::tree) was parsed from: the masked
    /// analysis text for Rmd/Quarto and directive-bearing Stan documents, the
    /// raw text otherwise.
    ///
    /// Use this — never [`text()`](Document::text) — whenever you slice the
    /// document by byte offsets taken from `tree` (e.g. `node.byte_range()` /
    /// `node.utf8_text(...)`). For plain R and JAGS this equals `text()`; Stan
    /// differs only on recognized full-line Raven directives. See the
    /// [`Document`] type docs for the full raw-vs-analysis invariant.
    pub fn analysis_text(&self) -> String {
        match &self.masked_text {
            Some(masked) => masked.clone(),
            None => self.contents.to_string(),
        }
    }

    /// Derive R cross-file metadata only for R documents. Native JAGS and Stan
    /// trees deliberately contribute no R scope, packages, sources, or directives.
    pub(crate) fn cross_file_metadata(&self) -> crate::cross_file::CrossFileMetadata {
        if self.file_type != FileType::R {
            crate::cross_file::CrossFileMetadata::default()
        } else {
            crate::cross_file::extract_metadata_from_analysis_for_kind(
                self.chunk_kind,
                &self.analysis_text(),
            )
        }
    }

    /// Compute metadata-dependent R scope artifacts, or an inert artifact set
    /// for non-R languages. This is the shared language boundary for open and closed
    /// document authorities.
    pub(crate) fn cross_file_artifacts(
        &self,
        uri: &Url,
        metadata: &crate::cross_file::CrossFileMetadata,
    ) -> crate::cross_file::scope::ScopeArtifacts {
        if self.file_type != FileType::R {
            return crate::cross_file::scope::ScopeArtifacts::default();
        }
        let analysis_text = self.analysis_text();
        self.tree
            .as_ref()
            .map_or_else(crate::cross_file::scope::ScopeArtifacts::default, |tree| {
                crate::cross_file::scope::compute_artifacts_with_metadata(
                    uri,
                    tree,
                    &analysis_text,
                    Some(metadata),
                )
            })
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

/// Project a full closed-index entry into an owned document view without
/// reading or parsing the file again.
pub(crate) fn document_from_workspace_entry(
    uri: &Url,
    entry: &crate::workspace_index::IndexEntry,
    chunk_kind: ChunkKind,
) -> Document {
    let file_type = file_type_from_uri(uri);
    let raw_text = entry.contents.to_string();
    let masked_text = analysis_mask(chunk_kind, file_type, &raw_text);
    Document {
        contents: entry.contents.clone(),
        tree: entry.tree.clone(),
        loaded_packages: entry.loaded_packages.clone(),
        data_packages: entry.data_packages.clone(),
        file_type,
        chunk_kind,
        masked_text,
        version: None,
        revision: 0,
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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

fn workspace_base_for_path_in(path: &Path, workspace_folders: &[Url]) -> Option<PathBuf> {
    workspace_folders
        .iter()
        .filter_map(|root| root.to_file_path().ok())
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
}

fn rebase_canonical_path_to_workspaces(
    canonical_path: &Path,
    workspace_folders: &[Url],
) -> Option<PathBuf> {
    let mut best: Option<(usize, PathBuf)> = None;
    for root_url in workspace_folders {
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

fn case_correct_open_path_for_workspaces(
    path: &Path,
    workspace_folders: &[Url],
) -> Option<PathBuf> {
    let base = workspace_base_for_path_in(path, workspace_folders)
        .or_else(|| filesystem_root_base(path))?;
    crate::cross_file::path_resolve::canonicalize_case_below_unique(&base, path)
}

fn symlink_target_open_path_for_workspaces(
    path: &Path,
    workspace_folders: &[Url],
) -> Option<PathBuf> {
    let canonical_path = fs::canonicalize(path).ok()?;
    let rebased = rebase_canonical_path_to_workspaces(&canonical_path, workspace_folders)
        .unwrap_or(canonical_path);
    case_correct_open_path_for_workspaces(&rebased, workspace_folders).or(Some(rebased))
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

/// Derive the byte/line-aligned source view used by the file type's parser.
fn analysis_mask(chunk_kind: ChunkKind, file_type: FileType, text: &str) -> Option<String> {
    match file_type {
        FileType::Stan => crate::stan::mask_raven_directives(text),
        FileType::Jags => None,
        FileType::R => crate::cross_file::masked_analysis_text(chunk_kind, text),
    }
}

#[cfg(test)]
thread_local! {
    /// Per-test-thread parse counter used to pin the single-parse batch-edit
    /// contract without introducing synchronization into production builds.
    static DOCUMENT_PARSE_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Counts final JAGS parses that received an edited prior tree.
    static JAGS_INCREMENTAL_PARSE_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Parse `text` (the already-prepared analysis text) into a
/// tree-sitter tree appropriate for `file_type`.
fn parse_document_text(text: &str, file_type: FileType) -> Option<Tree> {
    parse_document_text_with_old_tree(text, file_type, None)
}

fn parse_document_text_with_old_tree(
    text: &str,
    file_type: FileType,
    old_tree: Option<&Tree>,
) -> Option<Tree> {
    #[cfg(test)]
    DOCUMENT_PARSE_COUNT.with(|count| count.set(count.get().wrapping_add(1)));

    match file_type {
        FileType::R => parse_r_text(text),
        FileType::Jags => {
            #[cfg(test)]
            if old_tree.is_some() {
                JAGS_INCREMENTAL_PARSE_COUNT.with(|count| count.set(count.get().wrapping_add(1)));
            }
            crate::jags::parse_with_old_tree(text, old_tree)
        }
        FileType::Stan => crate::stan::parse(text),
    }
}

pub(crate) fn extract_loaded_packages(tree: &Option<Tree>, text: &str) -> Vec<String> {
    let Some(tree) = tree else {
        return Vec::new();
    };

    // Use the canonical detectors for lexical package loads and targets
    // pipeline worker-package declarations. `Document.loaded_packages`
    // also feeds CLI reporting, so conditional bare `p_load()` targets must
    // stay out until a graph-aware scope query proves their prerequisite.
    // Backend edit-time prefetching deliberately keeps its separate permissive
    // warm set; that path cannot affect semantic or user-visible package state.
    let mut packages: Vec<String> =
        crate::cross_file::source_detect::detect_library_calls(tree, text)
            .into_iter()
            .filter(|call| call.requires_attached.is_none())
            .map(|call| call.package)
            .collect();
    packages.extend(
        crate::cross_file::source_detect::detect_targets_pipeline_packages(tree, text)
            .into_iter()
            .map(|declaration| declaration.package),
    );
    packages.sort();
    packages.dedup();
    packages
}

/// Extract package names from `data(..., package = "pkg")` / `utils::data(...)`
/// calls (issue #429). Mirrors [`extract_loaded_packages`] but targets the
/// `package =` string-literal named argument of `data()` calls so the CLI can
/// warm those packages' `data/` enumeration for alias expansion. Only
/// string-literal `package =` values are collected (a variable package arg
/// can't be resolved statically).
pub(crate) fn extract_data_packages(tree: &Option<Tree>, text: &str) -> Vec<String> {
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
/// open documents, workspace indexes, the cross-file file cache, the dependency
/// graph, and diagnostic publication gates are all keyed
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
    /// Sole authority for editor-owned documents.
    pub(crate) documents: OpenDocumentStore,
    pub workspace_index: WorkspaceIndex,

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
    pub open_document_aliases: OpenDocumentAliases,
    // Workspace configuration
    pub workspace_folders: Vec<Url>,

    // Package function awareness
    // Manages installed packages, their exports, and caching for package-aware scope resolution
    // Requirement 13.4: THE Package_Cache SHALL support concurrent read access from multiple LSP handlers
    // Arc allows sharing across async tasks without holding WorldState lock
    pub package_library: Arc<PackageLibrary>,
    /// Never-reused identity of the currently installed package-library
    /// object. Value-equal replacements still receive a fresh identity.
    package_library_install_id: u64,
    /// In-place package-library content changes relevant to `system.file()`
    /// routing (package installation/removal beneath an unchanged libpath).
    package_library_content_generation: u64,
    /// Latest package-library replacement whose unpublished cache/routing
    /// candidate is still allowed to commit.
    ///
    /// Additive watcher mutations may still commit while this is present, but
    /// they preserve this intent. The replacement driver must then rebase the
    /// same intent onto the additive winner and repeat its unpublished
    /// clear/warm/derive round; it must never mint a replacement retry intent.
    library_replacement_lifecycle: Arc<parking_lot::Mutex<LibraryReplacementLifecycle>>,
    /// Synchronous shutdown fence shared with pre-seal Drop owners.
    routing_shutdown: Arc<AtomicBool>,
    /// Edge-triggered pickup signal for durable routing reconcile requests.
    ///
    /// Requests remain resident in `library_replacement_lifecycle`; this
    /// notification is only a wakeup hint, so a coalesced notification cannot
    /// lose work.
    library_routing_reconcile_wake: Arc<tokio::sync::Notify>,
    /// Monotonic durable edge paired with `library_routing_reconcile_wake`.
    ///
    /// Every producer advances this before notifying. Parked degraded repair
    /// therefore distinguishes a real edge from a consumed/coalesced Notify
    /// permit without changing its paced-attempt behavior.
    library_routing_reconcile_wake_generation: Arc<AtomicU64>,
    /// Durable token for eligibility changes that deliberately retain the
    /// same reconcile request identity.
    ///
    /// Ordinary request producers mint a fresh request ID and must not advance
    /// this generation. It exists solely so the pickup coordinator can
    /// distinguish an external eligibility edge from its own redeposit wake.
    library_routing_reconcile_eligibility_generation: Arc<AtomicU64>,
    /// Never-reused identity of the most recently installed package seed.
    package_seed_install_id: u64,
    /// Exact seed owner currently assigned to the coalesced deferred
    /// system-file convergence worker.
    pending_system_file_seed_retry: Option<PackageSeedInstalledIdentity>,
    /// Exact seed owner currently assigned to the coalesced deferred
    /// Rprofile+preamble convergence worker.
    pending_post_seed_refresh_retry: Option<PackageSeedInstalledIdentity>,
    /// A same-seed `system.file()` transfer completed while the combined
    /// post-seed owner was retaining the outer diagnostic ledger.
    pending_post_seed_system_transfer:
        Option<(PackageSeedInstalledIdentity, AnalysisTransferHandle)>,
    /// Whether the current post-seed coordinator must receive a routing
    /// transfer before its package tail may commit.
    pending_post_seed_requires_system_transfer: bool,
    /// Exact outer ledgers deposited by the deferred owner's callers before
    /// any post-seed or system worker is allowed to run.
    pending_post_seed_outer_handles: Vec<AnalysisTransferHandle>,
    pending_post_seed_outer_candidates: Vec<AnalysisTransferCandidate>,
    /// Warning notes detached from a routing transfer only after its durable
    /// post-seed/finalization owner has been registered.
    deferred_library_routing_build_notes: Vec<String>,
    /// Dedicated latest-owner identity for all `system.file()` routing inputs.
    ///
    /// This is deliberately narrower than `package_input_generation`: unrelated
    /// Rprofile, preamble, or namespace writes do not supersede convergence.
    system_file_routing_owner_generation: u64,

    // Caches
    pub help_cache: crate::help::HelpCache,
    pub html_help_cache: crate::help::HtmlHelpCache,
    pub signature_cache: Arc<SignatureCache>,
    pub cross_file_file_cache: CrossFileFileCache,
    pub diagnostics_gate: CrossFileDiagnosticsGate,
    /// Counted publication barrier for multi-commit analysis transitions.
    pub(crate) diagnostics_coherence: Arc<DiagnosticsCoherenceGate>,
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
    /// Deterministic didOpen reservation barrier used only by handler tests.
    #[cfg(test)]
    pub(crate) did_open_reservation_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) did_open_reservation_snapshot_for_test: Vec<(Url, u32)>,
    #[cfg(test)]
    pub(crate) did_change_reservation_snapshot_for_test: Vec<(Url, u64)>,
    #[cfg(test)]
    pub(crate) did_open_commit_snapshot_for_test: Option<DidOpenCommitSnapshot>,
    /// Deterministic barrier after detached derivation and before the
    /// commit-time alias-topology recheck.
    #[cfg(test)]
    pub(crate) did_open_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barriers around the OpenClose central CAS and empty
    /// publication while the diagnostics publish lock is held.
    #[cfg(test)]
    pub(crate) did_close_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) did_close_post_commit_pre_publish_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) close_resync_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic acknowledgement after a close resync has either
    /// committed or rejected its prepared disk observation.
    #[cfg(test)]
    pub(crate) close_resync_post_attempt_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) open_lifecycle_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) open_lifecycle_post_commit_pre_clear_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) open_lifecycle_post_unlock_pre_spawn_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) open_lifecycle_added_effects_complete_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barrier after a full workspace candidate is derived and
    /// before its central state CAS.
    #[cfg(test)]
    pub(crate) workspace_scan_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) watched_package_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barrier after an invocation-owned invalid-byte retry is
    /// admitted and before its existing delayed-retry timer starts.
    #[cfg(test)]
    pub(crate) watched_undecodable_retry_pre_delay_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) watched_batch_pre_finalize_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barrier between ordered update and deletion commits in
    /// the over-budget watched-batch fallback.
    #[cfg(test)]
    pub(crate) watched_batch_fallback_after_updates_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) watched_final_handoff_test_capture: FinalHandoffCapture<WatchedFinalHandoffForTest>,
    #[cfg(test)]
    pub(crate) config_reload_publish_test_capture: FinalHandoffCapture<ConfigReloadPublishForTest>,
    #[cfg(test)]
    pub(crate) analysis_revalidation_final_handoff_test_capture:
        FinalHandoffCapture<Vec<AnalysisRevalidationTicketFingerprint>>,
    #[cfg(test)]
    pub(crate) did_close_final_handoff_test_capture:
        FinalHandoffCapture<Vec<AnalysisRevalidationTicketFingerprint>>,
    #[cfg(test)]
    pub(crate) close_resync_final_handoff_test_capture:
        FinalHandoffCapture<Vec<CloseResyncConsumerForTest>>,
    /// Deterministic barrier after a config routing handoff is queued and
    /// before its tracked root waits for receiver acknowledgement.
    #[cfg(test)]
    pub(crate) config_system_file_post_send_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) diagnostics_post_publish_lock_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barrier after a successful atomic gate consume and before
    /// client publication, while the diagnostics publish lock remains held.
    #[cfg(test)]
    pub(crate) diagnostics_post_consume_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barrier at entry to the cancel-vs-consume backstop.
    #[cfg(test)]
    pub(crate) diagnostics_backstop_respawn_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Test-only causal ownership transferred from a receipt-owned
    /// diagnostics worker to the worker it supersedes.
    ///
    /// The map lives behind its own synchronous lock so scheduling can
    /// register the predecessor handoff before cancelling that predecessor.
    /// This closes the test-harness-only cancel/exit race without changing
    /// production diagnostics scheduling.
    #[cfg(test)]
    pub(crate) diagnostics_supersession_handoffs_for_test: DiagnosticsSupersessionHandoffMapForTest,
    /// Deterministic barrier after alias-reconcile derivation and before its
    /// commit-time topology recheck.
    #[cfg(test)]
    pub(crate) alias_reconcile_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barrier after open tar-parent derivation and before its
    /// central CAS. One-shot arming lets tests reject consecutive rounds.
    #[cfg(test)]
    pub(crate) open_tar_source_refresh_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barrier after the desired register becomes empty and
    /// before the single-flight coordinator releases admission.
    #[cfg(test)]
    pub(crate) open_tar_source_refresh_pre_release_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barrier after an on-demand package build and before its
    /// exact input/readiness CAS.
    #[cfg(test)]
    pub(crate) package_init_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic cancellation barrier after reconcile request claim and
    /// before any replacement guard/pre-seal ownership is armed.
    #[cfg(test)]
    pub(crate) package_init_post_claim_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Terminal reconcile-attempt barrier immediately before self-wake drain.
    #[cfg(test)]
    pub(crate) library_routing_reconcile_pre_drain_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Terminal reconcile-attempt barrier after the final eligibility reload
    /// and before parking.
    #[cfg(test)]
    pub(crate) library_routing_reconcile_post_reload_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Barrier after degraded repair's final attempt snapshots the durable
    /// wake edge and immediately before its park-boundary recheck.
    #[cfg(test)]
    pub(crate) degraded_reconcile_pre_park_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) system_file_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) system_file_pre_derivation_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) system_file_test_reject_remaining: usize,
    #[cfg(test)]
    pub(crate) system_file_test_commit_attempts: usize,
    /// Instance-scoped executor seam for paused-time routing tests. Production
    /// and the dedicated physical-lifetime tests leave this disabled.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) library_routing_derivation_on_tokio_for_test: bool,
    /// Instance-owned derivation lane keeps concurrent unit-test backends from
    /// spending one another's foreground routing deadlines.
    #[cfg(test)]
    pub(crate) library_routing_derivation_lane_for_test:
        Arc<crate::backend::LibraryRoutingDerivationLane>,
    #[cfg(test)]
    pub(crate) library_routing_test_reject_remaining: usize,
    #[cfg(test)]
    pub(crate) library_routing_test_commit_attempts: usize,
    /// Deterministic barrier after a deferred library-routing attempt retains
    /// its exact transfer ledger and before the retry backoff.
    #[cfg(test)]
    pub(crate) library_routing_deferred_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) library_routing_deferred_handles_for_test: Vec<AnalysisTransferHandle>,
    #[cfg(test)]
    pub(crate) library_routing_deferred_candidates_for_test: Vec<AnalysisTransferCandidate>,
    #[cfg(test)]
    pub(crate) library_routing_deferred_post_seed_for_test: Option<PackageSeedInstalledIdentity>,
    /// Deterministic barrier between detached overflow derivation and CAS.
    #[cfg(test)]
    pub(crate) open_edit_fallback_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    /// Deterministic barriers for detached live/post-seed package projections.
    #[cfg(test)]
    pub(crate) live_package_open_edit_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) post_seed_refresh_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) post_seed_refresh_test_reject_remaining: usize,
    #[cfg(test)]
    pub(crate) post_seed_refresh_test_commit_attempts: usize,
    #[cfg(test)]
    pub(crate) sysdata_fallback_pre_commit_test_pause:
        crate::cross_file::revalidation::DiagnosticsPublishPause,
    #[cfg(test)]
    pub(crate) sysdata_fallback_test_reject_remaining: usize,
    #[cfg(test)]
    pub(crate) sysdata_fallback_test_commit_attempts: usize,
    #[cfg(test)]
    pub(crate) force_open_edit_overflow_for_test: bool,
    #[cfg(test)]
    pub(crate) force_open_install_local_only_for_test: bool,
    /// Make on-demand package initialization complete in the supported
    /// degraded/not-ready state without consulting the host R installation.
    #[cfg(test)]
    pub(crate) force_package_library_not_ready_for_test: bool,
    /// One-shot synthetic package build for tests of package initialization or
    /// rebuild routing. This replaces only host-dependent R/provider discovery
    /// at the builder-result seam; production CAS, routing, and handoff still
    /// run.
    #[cfg(test)]
    pub(crate) package_library_build_outcome_for_test:
        Option<(Arc<crate::package_library::PackageLibrary>, bool)>,
    #[cfg(test)]
    pub(crate) force_package_init_stale_for_test: bool,

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

    /// Per-URI cache of the override-resolved per-document `LintConfig`
    /// (the expensive branch of `effective_lint_config_for_document`:
    /// glob matching, JSON patching, and a config re-parse). Interior
    /// mutability because resolution runs under the `WorldState` READ lock.
    /// Invalidated by the two writers of its inputs: cleared by
    /// `recompute_parsed_configs` (config layers / overrides changed) and by
    /// the `per_document_indent_options` swap in
    /// `raven/documentIndentUnitsChanged` (the patched base feeds
    /// resolution). Bounded by the set of open documents between clears.
    pub effective_lint_config_cache:
        std::sync::Mutex<std::collections::HashMap<String, crate::linting::LintConfig>>,

    /// Compiled `[workspace].exclude` entries. Empty when no project-level
    /// exclusions are configured. These apply to workspace/default discovery,
    /// indexing, watcher resync, on-demand indexing, and LSP diagnostics.
    pub workspace_exclusions: crate::config_file::CompiledWorkspaceExclusions,

    /// Per-document editor options sent by the client via
    /// `raven/documentIndentUnitsChanged`, keyed by URI string. All fields are
    /// independently optional; absent URIs (or absent fields) fall back
    /// exactly like an empty map — non-VS-Code clients, older extensions,
    /// and `raven check` never populate it.
    pub per_document_indent_options: std::collections::HashMap<String, DocumentIndentOptions>,

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
    /// Latest-owner lifecycle for durable open-parent `tar_source()` refresh.
    ///
    /// Each filesystem event supersedes the prior desired generation. One
    /// backend-owned coordinator per URI serializes physical filesystem walks
    /// and retries exact-basis conflicts until it commits, the open lifecycle
    /// disappears, or backend shutdown drains it.
    pub(crate) open_tar_source_refreshes: CrossFileRevalidationState,
    pub cross_file_activity: CrossFileActivityState,
    /// Editor resources eligible to own push diagnostics, when the client
    /// supplies an explicit UI-resource set.
    ///
    /// `None` preserves normal LSP behavior for clients that do not implement
    /// Raven's VS Code-specific tab notification: every `didOpen` document may
    /// receive diagnostics. `Some` keeps hidden client-created text models as
    /// analysis inputs while preventing them from acquiring Problems entries.
    pub editor_diagnostic_uris: Option<HashSet<Url>>,
    /// Freshness identity for the editor eligibility policy.
    ///
    /// Production writers replace the policy through
    /// [`Self::replace_editor_diagnostic_uris`] so detached open transitions
    /// can validate the policy without cloning its potentially unbounded set.
    editor_eligibility_generation: EditorEligibilityGeneration,
    /// Typed authority for the complete parsed analysis configuration.
    ///
    /// The sole parsed-config writer advances this after every recompute, so
    /// detached analysis families reject candidates captured before any
    /// effective settings transition, including the interval before later
    /// asynchronous package rebuild work completes.
    analysis_config_generation: AnalysisConfigGeneration,
    /// Never-reused-within-state identity for the complete editor-derived
    /// closed chunk-kind override map consumed by workspace scans.
    chunk_override_generation: ChunkOverrideGeneration,
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
    ///
    /// Production writes must use [`Self::record_editor_chunk_kind_override`]
    /// or [`Self::prune_editor_chunk_kind_override`] so detached scans observe
    /// the matching authority generation.
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
    /// Monotonic fence for filesystem events that may change static
    /// `targets::tar_source()` membership.
    tar_source_event_generation: u64,
    tar_source_watch_generation_counter: u64,
    /// Monotonic identities survive registry removal as tombstones.
    tar_source_watch_path_generations: HashMap<PathBuf, u64>,
    /// Durable bidirectional ownership registry for finalized tar requests.
    tar_source_watch_paths_by_parent: HashMap<Url, Vec<PathBuf>>,
    tar_source_parents_by_watch_path: HashMap<PathBuf, HashSet<Url>>,
    /// Exact applied watcher lifecycle; handle ownership is never exposed for
    /// cloning outside the central watcher CAS paths.
    libpath_watcher: LibpathWatcherState,
    /// Never-reused provenance for the active libpath watcher/consumer.
    ///
    /// Library replacement advances this in the same commit that publishes the
    /// successor library, so queued events from the retired watcher can never
    /// capture and mutate that successor during a post-commit restart gap.
    libpath_watcher_owner_generation: u64,
    pub package_library_ready: bool,
    /// ABA-safe ownership token for detached full-workspace scans.
    ///
    /// Closed-file writers and scan-affecting configuration changes advance
    /// this before their state can race a scan. A scan claims the exact token
    /// under the final write lock before installing its complete candidate.
    workspace_scan_generation: u64,
    /// Latest-arrival owner for top-level full workspace scans.
    workspace_scan_intent: Option<WorkspaceScanIntentState>,
    /// Unmarked diagnostic fanout owned by successful analysis commits.
    analysis_transfers: HashMap<AnalysisTransferIdentity, AnalysisTransferState>,
    /// Successful successor commits that inherited an older transfer.
    analysis_transfer_successors: HashMap<AnalysisTransferIdentity, AnalysisTransferIdentity>,
    /// Exact handles already consumed by a successful finalization, mapped to
    /// the exact diagnostic triggers that survived record filtering and the cap.
    analysis_transfers_consumed: HashMap<AnalysisTransferIdentity, Vec<(Url, DiagnosticsTrigger)>>,
    /// Finalization intents already completed, including fallback completion.
    analysis_transfer_finalizations: HashSet<AnalysisTransferFinalizationId>,
    /// Latest workspace transfer. Pending/failed scans do not change it.
    latest_workspace_scan_transfer: Option<AnalysisTransferIdentity>,
    /// Latest successful system-file transfer.
    latest_system_file_transfer: Option<AnalysisTransferIdentity>,
    /// Monotonic identity for graph/open-metadata mutations that do not
    /// necessarily change document text or either workspace-index version.
    workspace_graph_authority_generation: u64,
    /// Monotonic identity for the open URI set and immutable open metadata.
    ///
    /// Detached closed preparation enumerates open parents and consumes their
    /// metadata, so open/install/close/metadata-only replacements all advance
    /// this stamp even when the graph's edge set is unchanged.
    open_context_authority_generation: OpenContextAuthorityGeneration,
    /// Persistent ownership tombstones for detached `didOpen` installs.
    ///
    /// Entries remain after success, cancellation, and close so an
    /// absent→open→close cycle cannot make an older absent candidate current
    /// again. Every transition receives a process-wide never-reused identity.
    open_install_intents: HashMap<Url, OpenInstallIntentState>,
    /// Persistent ownership tombstones for detached `didClose` transactions.
    open_close_intents: HashMap<Url, OpenCloseIntentState>,
    /// Latest-arrival ownership for `raven/activeDocumentsChanged`.
    open_lifecycle_intent: Option<OpenLifecycleIntentState>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) open_pin_recompute_count: usize,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) watched_batch_test_reject_once: bool,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) watched_batch_test_reject_remaining: usize,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) watched_package_test_compute_fail_remaining: usize,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) watched_batch_test_commit_attempts: usize,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) analysis_revalidation_reservation_count: usize,
    /// Instrumentation for keeping ordinary analysis commits off the
    /// workspace-wide tar watch-registry sweep.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) tar_source_watch_full_rebuild_count: usize,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) tar_source_watch_parent_check_count: usize,
    /// Whether the background workspace scan has completed and the dependency
    /// graph has been populated from workspace entries. In `Auto` backward
    /// dependency mode, undefined variable diagnostics are deferred for files
    /// without explicit backward directives until this flag is true.
    pub workspace_scan_complete: bool,
    /// Container for all derived R package mode state. See package_state/mod.rs.
    pub package_state: crate::package_state::PackageState,
    /// Exact generation of the derived package-state record.
    package_state_record_generation: u64,
    /// Inputs to the package-mode `derive` function. Updated by event handlers
    /// before calling `apply_package_event`. See package_state::PackageInputs.
    pub package_inputs: crate::package_state::PackageInputs,
    /// Operational freshness identity for detached package-input seeds.
    ///
    /// Kept separate from semantic `package_state` so workspace-index
    /// application or derivation cannot reset freshness and make an older seed
    /// current again.
    pub(crate) package_input_lifecycle: crate::package_state::PackageInputLifecycle,
    /// Coalescing lifecycle for the one delayed package-seed convergence task.
    pub(crate) package_seed_retry: crate::package_state::PackageSeedRetryLifecycle,
    /// Additive lifecycle for deferred package-library routing ledgers.
    pub(crate) library_routing_retry: Arc<crate::package_state::PackageSeedRetryLifecycle>,
    pub(crate) watched_package_retry: Arc<crate::package_state::PackageSeedRetryLifecycle>,
    pub(crate) sysdata_fallback_retry: crate::package_state::PackageSeedRetryLifecycle,
}

/// A snapshot of the lifecycle and analysis configuration a diagnostics run
/// was triggered against: the URI's `(version, revision, epoch)` plus the
/// parsed-config generation, captured under a `WorldState` lock when the work
/// was spawned, carried through the debounce → compute → publish pipeline, and
/// re-compared at every freshness checkpoint via [`Self::is_stale`].
///
/// Bundling these fields keeps every checkpoint lifecycle- and config-aware by
/// construction. The loose `(version, revision)` pair this replaces cannot
/// identify a lifecycle (issue #603): a client may reopen at the same version,
/// and `Document::revision` restarts at 0 on every open, so a worker retired by
/// a close or tab removal could pass both comparisons against the URI's next
/// lifecycle. The epoch is globally unique per lifecycle and never reused.
/// The config generation prevents a worker computed under old diagnostic
/// settings from consuming a same-version force-republish marker after a live
/// configuration reload.
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
    analysis_config_generation: AnalysisConfigGeneration,
}

#[derive(Debug, Default)]
struct DiagnosticsCoherenceState {
    generation: u64,
    active: u32,
    deferred: HashSet<(Url, u64)>,
    suppressed_generation_through: Option<u64>,
}

/// Fail-closed publication barrier for a bounded sequence of globally visible
/// analysis commits that is coherent only as a whole.
///
/// Diagnostic computation remains lock-free. A worker captures
/// [`Self::generation`] with its analysis snapshot, then may publish only when
/// no barrier is active and that generation is still current. Advancing the
/// generation on both admission and release rejects workers that straddle the
/// boundary as well as workers that snapshot an intermediate commit.
#[derive(Debug, Default)]
pub(crate) struct DiagnosticsCoherenceGate {
    state: parking_lot::Mutex<DiagnosticsCoherenceState>,
    quiescent: tokio::sync::Notify,
}

impl DiagnosticsCoherenceGate {
    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().generation
    }

    pub(crate) fn publish_is_coherent(&self, captured_generation: u64) -> bool {
        let state = self.state.lock();
        state.active == 0 && state.generation == captured_generation
    }

    pub(crate) fn begin(self: &Arc<Self>) -> DiagnosticsCoherenceGuard {
        let mut state = self.state.lock();
        state.generation = state
            .generation
            .checked_add(1)
            .expect("diagnostics coherence generation exhausted");
        state.active = state
            .active
            .checked_add(1)
            .expect("diagnostics coherence barrier count exhausted");
        DiagnosticsCoherenceGuard {
            gate: Some(Arc::clone(self)),
            begin_generation: state.generation,
        }
    }

    pub(crate) async fn wait_until_quiescent(&self) {
        loop {
            let notified = self.quiescent.notified();
            if self.state.lock().active == 0 {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn claim_deferred(&self, uri: &Url, generation: u64) -> bool {
        let mut state = self.state.lock();
        if state
            .suppressed_generation_through
            .is_some_and(|through| generation <= through)
        {
            return false;
        }
        state.deferred.insert((uri.clone(), generation))
    }

    pub(crate) fn release_deferred(&self, uri: &Url, generation: u64) -> bool {
        let mut state = self.state.lock();
        state.deferred.remove(&(uri.clone(), generation));
        state
            .suppressed_generation_through
            .is_some_and(|through| generation <= through)
    }

    #[cfg(test)]
    pub(crate) fn snapshot_for_test(&self) -> (u64, u32, usize) {
        let state = self.state.lock();
        (state.generation, state.active, state.deferred.len())
    }
}

/// RAII ownership for one counted diagnostics-coherence reservation.
pub(crate) struct DiagnosticsCoherenceGuard {
    gate: Option<Arc<DiagnosticsCoherenceGate>>,
    begin_generation: u64,
}

impl DiagnosticsCoherenceGuard {
    pub(crate) fn suppress_deferred_on_cancel(&self) {
        let Some(gate) = &self.gate else {
            return;
        };
        let mut state = gate.state.lock();
        let through = state.generation.max(self.begin_generation);
        state.suppressed_generation_through = Some(
            state
                .suppressed_generation_through
                .map_or(through, |suppressed| suppressed.max(through)),
        );
    }
}

impl Drop for DiagnosticsCoherenceGuard {
    fn drop(&mut self) {
        let Some(gate) = self.gate.take() else {
            return;
        };
        let quiescent = {
            let mut state = gate.state.lock();
            state.active = state
                .active
                .checked_sub(1)
                .expect("diagnostics coherence guard released exactly once");
            state.generation = state
                .generation
                .checked_add(1)
                .expect("diagnostics coherence generation exhausted");
            state.active == 0
        };
        if quiescent {
            gate.quiescent.notify_waiters();
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct DidOpenCommitSnapshot {
    pub(crate) generation: AnalysisGeneration,
    pub(crate) epoch: DiagnosticsEpoch,
    pub(crate) tickets: Vec<(Url, DiagnosticsTrigger)>,
}

impl DiagnosticsTrigger {
    /// Capture `uri`'s current trigger snapshot. The caller must hold a
    /// `WorldState` lock for the duration so all reads are coherent.
    pub(crate) fn capture(state: &WorldState, uri: &Url) -> Self {
        let doc = state.documents.get(uri);
        Self {
            version: doc.and_then(|d| d.version),
            revision: doc.map(|d| d.revision),
            epoch: state.diagnostics_gate.current_epoch(uri),
            analysis_config_generation: state.analysis_config_generation,
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

/// Exact authority snapshot consumed by one prepared analysis transaction.
///
/// Fields are private so production callers can only obtain a basis through a
/// named [`WorldState`] capture method. Family-specific constructors include
/// every input read by that preparation path.
#[derive(Clone)]
pub(crate) struct AnalysisBasis {
    subject: AnalysisSubjectBasis,
    watched_file_generation: Option<u64>,
    tar_source_event_generation: u64,
    tar_source_watch_generations: Vec<(PathBuf, u64)>,
    graph_revision: u64,
    graph_authority_generation: u64,
    open_context_authority_generation: OpenContextAuthorityGeneration,
    analysis_config_generation: AnalysisConfigGeneration,
    context_authorities: Vec<AnalysisContextAuthority>,
    batch_overlay_contexts: Vec<Url>,
    open_transition: Option<OpenTransitionStamp>,
    package_input_generation: u64,
    package_config_generation: u64,
    system_file_routing: SystemFileRoutingStamp,
    analysis_config: AnalysisConfigStamp,
}

#[derive(Clone)]
enum AnalysisSubjectBasis {
    Pending(EnrichmentClaim),
    Complete(CompleteRefreshToken),
    Observed(ClosedRecordToken),
    Open(OpenRecordToken),
    OpenInstall(Box<OpenInstallSubjectBasis>),
    OpenClose(Box<OpenCloseSubjectBasis>),
}

/// Latest-arrival ownership for one detached `didOpen` install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenInstallIntentToken {
    uri: Url,
    generation: u64,
    /// Target slot identity at arrival. Unlike unrelated context authorities,
    /// this is never rebased across the one permitted retry.
    target: OpenRecordToken,
}

impl OpenInstallIntentToken {
    pub(crate) fn uri(&self) -> &Url {
        &self.uri
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenInstallIntentState {
    Pending(u64),
    Installed(u64),
    Cancelled(u64),
}

#[derive(Clone)]
struct OpenInstallSubjectBasis {
    intent: OpenInstallIntentToken,
    target: OpenRecordToken,
}

/// Latest-arrival ownership for one detached `didClose` transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenCloseIntentToken {
    uri: Url,
    generation: u64,
    /// Arrival-time record identity. Retries may refresh ancillary context but
    /// must never rebase onto an edit, metadata replacement, or reopen.
    target: OpenRecordToken,
}

/// Process-wide never-reused arrival identity for one active-document
/// notification. `timestamp_ms` is payload only and never participates in
/// ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenLifecycleIntentToken {
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenLifecycleIntentState {
    Pending(u64),
    Committed(u64),
}

/// Latest-arrival ownership for one top-level full workspace scan.
///
/// The process-wide generation is never reused. Both allowed full attempts
/// retain the same token; a newer driver permanently supersedes the older one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkspaceScanIntentToken {
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceScanIntentState {
    Pending(u64),
    Committed(u64),
}

impl OpenCloseIntentToken {
    pub(crate) fn uri(&self) -> &Url {
        &self.uri
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenCloseIntentState {
    Pending(u64),
    Closed(u64),
    Cancelled(u64),
}

#[derive(Clone)]
struct OpenCloseSubjectBasis {
    intent: OpenCloseIntentToken,
    target: OpenRecordToken,
}

#[derive(Clone)]
struct OpenTransitionStamp {
    diagnostic_epoch: Option<DiagnosticsEpoch>,
    editor_eligibility_generation: EditorEligibilityGeneration,
    closed_index_version: u64,
    raw_cache_generation: u64,
    /// Current authoritative owners actually consulted for the target's raw,
    /// registered-canonical, and prospective-canonical URI spellings.
    ///
    /// Each open record has at most [`WorldState::MAX_OPEN_ALIASES_PER_RECORD`]
    /// canonical aliases, and the prospective calculation has the same bound.
    /// After URI de-duplication this list is therefore fixed-size (at most five
    /// tokens including the target spelling), in deterministic URI order.
    alias_owner_tokens: Vec<OpenRecordToken>,
    raw_authorities: Vec<(Url, Option<crate::cross_file::file_cache::FileSnapshot>)>,
}

/// Typed freshness identity for the open-record/alias/lifecycle authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenContextAuthorityGeneration(u64);

/// Typed freshness identity for the editor diagnostics-eligibility policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EditorEligibilityGeneration(u64);

/// Typed freshness identity for every parsed analysis setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnalysisConfigGeneration(u64);

/// Typed freshness identity for the editor-derived closed chunk-kind map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkOverrideGeneration(u64);

/// Exact pre-I/O inputs for one workspace scan attempt.
///
/// This is captured after the top-level driver mints its intent but before
/// disk I/O. A changed generation or newer intent rejects the result without
/// allowing an older driver to rebase onto the newer request.
#[derive(Clone)]
pub(crate) struct WorkspaceScanInputBasis {
    intent: WorkspaceScanIntentToken,
    scan_generation: u64,
    tar_source_event_generation: u64,
    analysis_config_generation: AnalysisConfigGeneration,
    chunk_override_generation: ChunkOverrideGeneration,
    pub(crate) workspace_folders: Vec<Url>,
    pub(crate) max_chain_depth: usize,
    pub(crate) max_transitive_dependents_visited: usize,
    pub(crate) exclusion_patterns: Vec<String>,
    pub(crate) index_workspace: bool,
}

impl WorkspaceScanInputBasis {
    pub(crate) fn scan_generation(&self) -> u64 {
        self.scan_generation
    }
}

/// Exact post-scan authority snapshot used by detached graph and metadata
/// derivation.
#[derive(Clone)]
pub(crate) struct WorkspaceScanDerivationBasis {
    input: WorkspaceScanInputBasis,
    graph_revision: u64,
    graph_authority_generation: u64,
    open_context_authority_generation: OpenContextAuthorityGeneration,
    workspace_index_version: u64,
    workspace_index_max_files: usize,
    workspace_index_max_file_size_bytes: usize,
    workspace_index_artifact_capacity: usize,
    workspace_index_pinned: HashSet<Url>,
    package_input_generation: u64,
    package_config_generation: u64,
    system_file_routing: SystemFileRoutingStamp,
    open_records: std::collections::BTreeMap<Url, OpenRecordToken>,
}

impl WorkspaceScanDerivationBasis {
    pub(crate) fn workspace_index_max_files(&self) -> usize {
        self.workspace_index_max_files
    }

    pub(crate) fn workspace_index_max_file_size_bytes(&self) -> usize {
        self.workspace_index_max_file_size_bytes
    }

    pub(crate) fn workspace_index_artifact_capacity(&self) -> usize {
        self.workspace_index_artifact_capacity
    }

    pub(crate) fn system_file_inputs(&self) -> (Option<String>, Option<PathBuf>, Vec<PathBuf>) {
        (
            self.system_file_routing.workspace_name.clone(),
            self.system_file_routing.workspace_root.clone(),
            self.system_file_routing.library_paths.clone(),
        )
    }
}

#[derive(Clone)]
enum AnalysisContextAuthority {
    Closed(ClosedRecordToken),
    Raw {
        uri: Url,
        snapshot: Option<crate::cross_file::file_cache::FileSnapshot>,
    },
}

impl AnalysisContextAuthority {
    fn uri(&self) -> &Url {
        match self {
            Self::Closed(token) => token.uri(),
            Self::Raw { uri, .. } => uri,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SystemFileRoutingStamp {
    owner: SystemFileRoutingOwnerIdentity,
    package_state_record_generation: u64,
    package_library_install_id: u64,
    package_library_content_generation: u64,
    workspace_name: Option<String>,
    workspace_root: Option<PathBuf>,
    library_paths: Vec<PathBuf>,
}

#[derive(Clone, PartialEq, Eq)]
struct AnalysisConfigStamp {
    workspace_folders: Vec<Url>,
    max_chain_depth: usize,
    max_forward_depth: usize,
    max_backward_depth: usize,
    on_demand_indexing_enabled: bool,
    packages_enabled: bool,
    revalidation_debounce_ms: u64,
    exclusion_patterns: Vec<String>,
    chunk_kind: ChunkKind,
}

/// One closed analysis derived off-lock and ready for an all-or-nothing commit.
pub(crate) struct PreparedClosedAnalysis {
    pub(crate) basis: AnalysisBasis,
    pub(crate) uri: Url,
    pub(crate) entry: IndexEntry,
    pub(crate) snapshot: crate::cross_file::file_cache::FileSnapshot,
    pub(crate) content: String,
    pub(crate) graph_metadata: Arc<crate::cross_file::CrossFileMetadata>,
    pub(crate) workspace_root: Option<Url>,
    pub(crate) parent_content: HashMap<Url, String>,
    pub(crate) additional_graph: Vec<PreparedGraphProjection>,
    pub(crate) wd_children: Vec<Url>,
}

pub(crate) struct PreparedGraphProjection {
    pub(crate) uri: Url,
    pub(crate) metadata: Arc<crate::cross_file::CrossFileMetadata>,
    pub(crate) parent_content: HashMap<Url, String>,
    pub(crate) make_non_lending: bool,
}

impl PreparedGraphProjection {
    pub(crate) fn new(
        uri: Url,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
        parent_content: HashMap<Url, String>,
        make_non_lending: bool,
    ) -> Self {
        Self {
            uri,
            metadata,
            parent_content,
            make_non_lending,
        }
    }
}

impl PreparedClosedAnalysis {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        basis: AnalysisBasis,
        uri: Url,
        entry: IndexEntry,
        snapshot: crate::cross_file::file_cache::FileSnapshot,
        content: String,
        graph_metadata: Arc<crate::cross_file::CrossFileMetadata>,
        workspace_root: Option<Url>,
        parent_content: HashMap<Url, String>,
        additional_graph: Vec<PreparedGraphProjection>,
        wd_children: Vec<Url>,
    ) -> Self {
        Self {
            basis,
            uri,
            entry,
            snapshot,
            content,
            graph_metadata,
            workspace_root,
            parent_content,
            additional_graph,
            wd_children,
        }
    }

    pub(crate) fn declare_batch_overlay_contexts(&mut self, targets: &HashSet<Url>) {
        self.basis.batch_overlay_contexts = self
            .basis
            .context_authorities
            .iter()
            .map(AnalysisContextAuthority::uri)
            .filter(|uri| targets.contains(*uri))
            .cloned()
            .collect();
        self.basis
            .batch_overlay_contexts
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        self.basis.batch_overlay_contexts.dedup();
    }
}

impl PreparedOpenEditAnalysis {
    pub(crate) fn new(
        edit: PreparedOpenEdit,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
        plan: PreparedOpenCommitPlan,
    ) -> Self {
        Self {
            basis: edit.basis,
            uri: edit.uri,
            prepared: edit.prepared,
            metadata,
            package: None,
            plan,
        }
    }

    pub(crate) fn with_package(
        edit: PreparedOpenEdit,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
        package: PreparedPackageProjection,
        plan: PreparedOpenCommitPlan,
    ) -> Self {
        Self {
            basis: edit.basis,
            uri: edit.uri,
            prepared: edit.prepared,
            metadata,
            package: Some(package),
            plan,
        }
    }

    pub(crate) fn into_prepared_edit(self) -> PreparedOpenEdit {
        PreparedOpenEdit {
            basis: self.basis,
            uri: self.uri,
            prepared: self.prepared,
        }
    }
}

impl PreparedOpenMetadataAnalysis {
    pub(crate) fn new(
        basis: AnalysisBasis,
        uri: Url,
        expected: AnalysisGeneration,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
        plan: PreparedOpenCommitPlan,
    ) -> Self {
        Self {
            basis,
            uri,
            expected,
            metadata,
            plan,
        }
    }
}

impl PreparedOpenAliasReconcileAnalysis {
    pub(crate) fn new(
        basis: AnalysisBasis,
        uri: Url,
        expected: AnalysisGeneration,
        aliases: Vec<Url>,
        plan: PreparedOpenCommitPlan,
    ) -> Self {
        Self {
            basis,
            uri,
            expected,
            aliases,
            plan,
        }
    }
}

impl PreparedOpenInstallAnalysis {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        basis: AnalysisBasis,
        intent: OpenInstallIntentToken,
        uri: Url,
        document: Document,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
        artifacts: Arc<crate::cross_file::scope::ScopeArtifacts>,
        aliases: Vec<Url>,
        package: Option<PreparedPackageProjection>,
        plan: PreparedOpenCommitPlan,
    ) -> Self {
        Self {
            basis,
            intent,
            uri,
            document,
            metadata,
            artifacts,
            aliases,
            package,
            plan,
        }
    }
}

impl PreparedOpenCloseAnalysis {
    #[allow(
        clippy::too_many_arguments,
        reason = "the constructor makes every independently prepared close authority explicit"
    )]
    pub(crate) fn new(
        basis: AnalysisBasis,
        intent: OpenCloseIntentToken,
        uri: Url,
        expected_aliases: Vec<Url>,
        package: Option<PreparedPackageProjection>,
        plan: PreparedOpenCommitPlan,
        resync: Vec<OpenCloseResyncTicket>,
        disk_observations: Vec<OpenCloseDiskObservation>,
        watched_roots: Vec<(Url, Option<u64>, ChunkKind)>,
    ) -> Self {
        Self {
            basis,
            intent,
            uri,
            expected_aliases,
            package,
            plan,
            resync,
            disk_observations,
            watched_roots,
        }
    }

    pub(crate) fn disk_observations(&self) -> &[OpenCloseDiskObservation] {
        &self.disk_observations
    }
}

pub(crate) enum PreparedAnalysisCommit {
    Upsert(Box<PreparedClosedAnalysis>),
    Remove { basis: Box<AnalysisBasis>, uri: Url },
    WatchedBatch(Box<PreparedWatchedBatchAnalysis>),
    WorkspaceScan(Box<PreparedWorkspaceScanAnalysis>),
    SystemFile(Box<PreparedSystemFileAnalysis>),
    OpenEdit(Box<PreparedOpenEditAnalysis>),
    OpenMetadata(Box<PreparedOpenMetadataAnalysis>),
    OpenAliasReconcile(Box<PreparedOpenAliasReconcileAnalysis>),
    OpenInstall(Box<PreparedOpenInstallAnalysis>),
    OpenClose(Box<PreparedOpenCloseAnalysis>),
}

/// Bounded post-commit check for whether authoritative tar watch topology may
/// have changed.
///
/// Ordinary commits name only the parents whose open/closed authority changed.
/// A full workspace replacement skips the gate because every parent may have
/// changed. The existing full rebuild remains the single writer of both
/// registry directions and the root-generation tombstones.
enum TarSourceWatchRegistryRefresh {
    Full,
    Parents(Vec<Url>),
}

impl TarSourceWatchRegistryRefresh {
    fn push_parent(&mut self, parent: Option<Url>) {
        if let (Self::Parents(parents), Some(parent)) = (self, parent) {
            parents.push(parent);
        }
    }
}

/// One all-or-none watched-file transaction spanning closed-file and package
/// authorities. Every basis is preflighted before either projection mutates
/// central state.
pub(crate) struct PreparedWatchedBatchAnalysis {
    pub(crate) mutations: Vec<PreparedClosedMutation>,
    pub(crate) package: Option<(PackageProjectionBasis, PreparedPackageProjection)>,
    pub(crate) package_open_records: std::collections::BTreeMap<Url, OpenRecordToken>,
    pub(crate) watched_generations: Vec<(Url, u64)>,
    /// Install an exact transfer ledger with the package-only CAS before an
    /// over-budget caller performs ordered closed-file commits.
    pub(crate) durable_package_handoff: bool,
}

#[cfg(test)]
impl PreparedWatchedBatchAnalysis {
    pub(crate) fn closed_for_test(mutations: Vec<PreparedClosedMutation>) -> Self {
        Self {
            mutations,
            package: None,
            package_open_records: Default::default(),
            watched_generations: Vec::new(),
            durable_package_handoff: false,
        }
    }
}

#[derive(Clone)]
struct SystemFileAnalysisBasis {
    routing: SystemFileRoutingStamp,
    tar_source_event_generation: u64,
    workspace_index_version: u64,
    workspace_index_max_files: usize,
    workspace_index_max_file_size_bytes: usize,
    workspace_index_artifact_capacity: usize,
    workspace_index_pinned: HashSet<Url>,
    graph_revision: u64,
    graph_authority_generation: u64,
    open_context_authority_generation: OpenContextAuthorityGeneration,
    analysis_config_generation: AnalysisConfigGeneration,
    chunk_override_generation: ChunkOverrideGeneration,
    workspace_folders: Vec<Url>,
    exclusion_patterns: Vec<String>,
    max_chain_depth: usize,
    open_records: std::collections::BTreeMap<Url, OpenRecordToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LibraryRoutingMutation {
    Replacement,
    Changed,
    /// Unknown watcher loss while retaining the exact active watcher lineage.
    ///
    /// Unlike targeted `Changed`, this clears and fully warms the cache,
    /// re-derives every `system.file()` source, and republishes all open
    /// documents. Unlike `Dropped`, it does not replace the watcher.
    FullRescan,
    /// One exact watcher-independent repair after a watcher-only primary and
    /// recovery attach both failed.
    DegradedReconcile,
    Dropped,
}

/// Never-reused owner of one package-library replacement request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LibraryReplacementIntent(u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibraryRoutingReconcileTelemetry {
    pub(crate) package_config_generation: u64,
    pub(crate) package_input_generation: u64,
    pub(crate) packages_enabled: bool,
    pub(crate) packages_r_path: Option<PathBuf>,
    pub(crate) packages_additional_library_paths: Vec<PathBuf>,
    pub(crate) workspace_folders: Vec<Url>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibraryRoutingReconcileRequest {
    pub(crate) id: u64,
    pub(crate) telemetry: LibraryRoutingReconcileTelemetry,
}

#[derive(Default)]
struct LibraryReplacementLifecycle {
    pending: Option<LibraryReplacementIntent>,
    reconcile_required: Option<LibraryRoutingReconcileRequest>,
    pre_seal: Option<LibraryRoutingPreSealDeposit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LibraryReplacementAbortPolicy {
    Reconcile,
    NoReconcile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibraryRoutingPreSealPostSeed {
    pub(crate) root: PathBuf,
    pub(crate) identity: PackageSeedInstalledIdentity,
    pub(crate) deferred_system_file: Option<PackageSeedInstalledIdentity>,
}

#[derive(Debug, Clone)]
pub(crate) struct LibraryRoutingPreSealDeposit {
    pub(crate) id: u64,
    replacement_intent: Option<LibraryReplacementIntent>,
    pub(crate) telemetry: LibraryRoutingReconcileTelemetry,
    pub(crate) handles: Vec<AnalysisTransferHandle>,
    pub(crate) candidates: Vec<AnalysisTransferCandidate>,
    pub(crate) fallback: Vec<Url>,
    pub(crate) post_seed: Option<LibraryRoutingPreSealPostSeed>,
    pub(crate) retired_post_seed_owners: Vec<PackageSeedInstalledIdentity>,
    pub(crate) build_notes: Vec<String>,
}

impl LibraryRoutingPreSealDeposit {
    fn from_basis(basis: &LibraryRoutingBasis) -> Self {
        Self {
            id: Self::mint_id(),
            replacement_intent: basis.replacement_intent,
            telemetry: LibraryRoutingReconcileTelemetry {
                package_config_generation: basis.package_config_generation,
                package_input_generation: basis.package_input_generation,
                packages_enabled: basis.packages_enabled,
                packages_r_path: basis.packages_r_path.clone(),
                packages_additional_library_paths: basis.packages_additional_library_paths.clone(),
                workspace_folders: basis.workspace_folders.clone(),
            },
            handles: Vec::new(),
            candidates: Vec::new(),
            fallback: Vec::new(),
            post_seed: None,
            retired_post_seed_owners: Vec::new(),
            build_notes: Vec::new(),
        }
    }

    fn mint_id() -> u64 {
        NEXT_LIBRARY_ROUTING_RECONCILE_REQUEST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("library-routing pre-seal identity exhausted")
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.handles.is_empty()
            && self.candidates.is_empty()
            && self.fallback.is_empty()
            && self.post_seed.is_none()
            && self.retired_post_seed_owners.is_empty()
            && self.build_notes.is_empty()
    }

    pub(crate) fn add_obligations(
        &mut self,
        handles: Vec<AnalysisTransferHandle>,
        candidates: Vec<AnalysisTransferCandidate>,
        fallback: Vec<Url>,
        post_seed: Option<LibraryRoutingPreSealPostSeed>,
        build_notes: Vec<String>,
    ) {
        let incoming = Self {
            id: Self::mint_id(),
            replacement_intent: self.replacement_intent,
            telemetry: self.telemetry.clone(),
            handles,
            candidates,
            fallback,
            post_seed,
            retired_post_seed_owners: Vec::new(),
            build_notes,
        };
        if incoming.is_empty() {
            return;
        }
        self.merge(incoming);
    }

    fn merge(&mut self, mut incoming: Self) {
        self.id = Self::mint_id();
        match (self.replacement_intent, incoming.replacement_intent) {
            (Some(existing), Some(candidate)) if candidate.0 > existing.0 => {
                self.replacement_intent = Some(candidate);
                self.telemetry = incoming.telemetry.clone();
            }
            (None, Some(candidate)) => {
                self.replacement_intent = Some(candidate);
                self.telemetry = incoming.telemetry.clone();
            }
            (None, None) => {
                // Additive ledgers have no intent ordering; retain the most
                // recently deposited telemetry snapshot.
                self.telemetry = incoming.telemetry.clone();
            }
            (Some(_), Some(_)) | (Some(_), None) => {}
        }
        for handle in incoming.handles.drain(..) {
            if !self.handles.contains(&handle) {
                self.handles.push(handle);
            }
        }
        for candidate in incoming.candidates.drain(..) {
            let existing = self
                .candidates
                .iter()
                .position(|prior| prior.uri == candidate.uri && prior.record == candidate.record);
            if let Some(index) = existing {
                let replace = matches!(
                    candidate.reservation,
                    AnalysisTransferReservationPolicy::Subject { .. }
                ) || !matches!(
                    self.candidates[index].reservation,
                    AnalysisTransferReservationPolicy::Subject { .. }
                );
                if replace {
                    self.candidates[index] = candidate;
                }
            } else {
                self.candidates.push(candidate);
            }
        }
        for uri in incoming.fallback.drain(..) {
            if !self.fallback.contains(&uri) {
                self.fallback.push(uri);
            }
        }
        if let Some(incoming_post_seed) = incoming.post_seed.take() {
            match self.post_seed.take() {
                Some(mut existing)
                    if existing.identity.seed_install_id
                        == incoming_post_seed.identity.seed_install_id =>
                {
                    if existing.deferred_system_file.is_none() {
                        existing.deferred_system_file = incoming_post_seed.deferred_system_file;
                    } else if incoming_post_seed.deferred_system_file
                        != existing.deferred_system_file
                        && let Some(retired) = incoming_post_seed.deferred_system_file
                    {
                        self.retired_post_seed_owners.push(retired);
                    }
                    self.post_seed = Some(existing);
                }
                Some(existing)
                    if existing.identity.seed_install_id
                        > incoming_post_seed.identity.seed_install_id =>
                {
                    self.retire_pre_seal_post_seed(incoming_post_seed);
                    self.post_seed = Some(existing);
                }
                Some(existing) => {
                    self.retire_pre_seal_post_seed(existing);
                    self.post_seed = Some(incoming_post_seed);
                }
                None => self.post_seed = Some(incoming_post_seed),
            }
        }
        self.retired_post_seed_owners
            .extend(incoming.retired_post_seed_owners);
        self.retired_post_seed_owners
            .sort_unstable_by_key(|identity| identity.seed_install_id);
        self.retired_post_seed_owners.dedup();
        self.build_notes.extend(incoming.build_notes);
    }

    fn retire_pre_seal_post_seed(&mut self, owner: LibraryRoutingPreSealPostSeed) {
        self.retired_post_seed_owners.push(owner.identity);
        if let Some(system) = owner.deferred_system_file
            && system != owner.identity
        {
            self.retired_post_seed_owners.push(system);
        }
    }
}

#[derive(Clone)]
pub(crate) struct LibraryRoutingPreSealOwner {
    lifecycle: Arc<parking_lot::Mutex<LibraryReplacementLifecycle>>,
    routing_shutdown: Arc<AtomicBool>,
    reconcile_wake: Arc<tokio::sync::Notify>,
    reconcile_wake_generation: Arc<AtomicU64>,
}

fn notify_library_routing_reconcile_edge(wake: &tokio::sync::Notify, generation: &AtomicU64) {
    generation
        .fetch_update(Ordering::Release, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("library-routing wake generation exhausted");
    wake.notify_waiters();
    wake.notify_one();
}

impl LibraryRoutingPreSealOwner {
    pub(crate) fn deposit(&self, mut deposit: LibraryRoutingPreSealDeposit) {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        let mut lifecycle = self.lifecycle.lock();
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        if lifecycle.pending == deposit.replacement_intent {
            lifecycle.pending = None;
        }
        if let Some(existing) = lifecycle.pre_seal.as_mut() {
            existing.merge(deposit);
        } else {
            deposit.id = LibraryRoutingPreSealDeposit::mint_id();
            lifecycle.pre_seal = Some(deposit);
        }
        let (id, telemetry) = lifecycle
            .pre_seal
            .as_ref()
            .map(|stored| (stored.id, stored.telemetry.clone()))
            .expect("pre-seal deposit was installed");
        lifecycle.reconcile_required = Some(LibraryRoutingReconcileRequest { id, telemetry });
        drop(lifecycle);
        // Wake every current durable-obligation consumer (the generic
        // replacement coordinator and any terminal-degraded watcher repair),
        // then retain one permit for a consumer that has not parked yet.
        notify_library_routing_reconcile_edge(
            &self.reconcile_wake,
            &self.reconcile_wake_generation,
        );
    }
}

/// Synchronous wake-token owner for additive Changed/Dropped routing.
///
/// Telemetry is diagnostic-only: pickup must always capture the current
/// package-init key and must never use this snapshot for ABA-sensitive
/// eligibility. Concrete obligations live only in the pre-seal ledger; this
/// owner deliberately carries no handles, candidates, or post-seed identity.
#[derive(Clone)]
pub(crate) struct LibraryRoutingReconcileOwner {
    lifecycle: Arc<parking_lot::Mutex<LibraryReplacementLifecycle>>,
    routing_shutdown: Arc<AtomicBool>,
    reconcile_wake: Arc<tokio::sync::Notify>,
    reconcile_wake_generation: Arc<AtomicU64>,
    telemetry: LibraryRoutingReconcileTelemetry,
}

impl LibraryRoutingReconcileOwner {
    pub(crate) fn request(&self) {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        let id = NEXT_LIBRARY_ROUTING_RECONCILE_REQUEST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("library-routing reconcile request identity exhausted");
        let mut lifecycle = self.lifecycle.lock();
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        // Requests are interchangeable wake tokens. Never mutate `pending`
        // replacement currency or the separately durable pre-seal ledger.
        lifecycle.reconcile_required = Some(LibraryRoutingReconcileRequest {
            id,
            telemetry: self.telemetry.clone(),
        });
        drop(lifecycle);
        notify_library_routing_reconcile_edge(
            &self.reconcile_wake,
            &self.reconcile_wake_generation,
        );
    }
}

/// Cancellation-safe ownership of one reconcile request removed for pickup.
///
/// The exact request identity is restored on abandonment so a cancelled
/// foreground pickup cannot masquerade as fresh retry currency. A request
/// deposited after this claim wins by simple slot presence.
pub(crate) struct LibraryRoutingReconcileClaim {
    lifecycle: Arc<parking_lot::Mutex<LibraryReplacementLifecycle>>,
    routing_shutdown: Arc<AtomicBool>,
    reconcile_wake: Arc<tokio::sync::Notify>,
    reconcile_wake_generation: Arc<AtomicU64>,
    request: Option<LibraryRoutingReconcileRequest>,
}

impl LibraryRoutingReconcileClaim {
    pub(crate) fn request(&self) -> &LibraryRoutingReconcileRequest {
        self.request
            .as_ref()
            .expect("armed reconcile claim retains its exact request")
    }

    /// Transfer responsibility to an already-armed replacement guard and
    /// pre-seal obligation.
    pub(crate) fn consume(mut self) {
        let _lifecycle = self.lifecycle.lock();
        self.request = None;
    }
}

impl Drop for LibraryRoutingReconcileClaim {
    fn drop(&mut self) {
        let Some(request) = self.request.take() else {
            return;
        };
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        let mut lifecycle = self.lifecycle.lock();
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        if lifecycle.reconcile_required.is_some() {
            return;
        }
        lifecycle.reconcile_required = Some(request);
        drop(lifecycle);
        notify_library_routing_reconcile_edge(
            &self.reconcile_wake,
            &self.reconcile_wake_generation,
        );
    }
}

/// Synchronous cancellation owner for one unpublished replacement.
///
/// `Drop` deliberately locks only [`LibraryReplacementLifecycle`], never
/// [`WorldState`]. If cancellation destroys the exact current obligation
/// before publication, it retires that intent and deposits a fresh reconcile
/// request for a later async pickup. A newer intent wins and is left intact.
pub(crate) struct PendingLibraryReplacementGuard {
    lifecycle: Arc<parking_lot::Mutex<LibraryReplacementLifecycle>>,
    routing_shutdown: Arc<AtomicBool>,
    reconcile_wake: Arc<tokio::sync::Notify>,
    reconcile_wake_generation: Arc<AtomicU64>,
    intent: LibraryReplacementIntent,
    telemetry: LibraryRoutingReconcileTelemetry,
    abort_policy: LibraryReplacementAbortPolicy,
    armed: bool,
}

impl PendingLibraryReplacementGuard {
    /// Retire a deliberately redundant replacement bundle atomically.
    ///
    /// `preserved` contains only adopted currency or concrete pre-seal
    /// content; a fresh vacuous payload is passed as `None`. The exact pending
    /// intent and both Drop owners are therefore resolved under one lifecycle
    /// mutex, with no partial-take window that can manufacture phantom
    /// reconciliation. A newer intent is never cleared.
    pub(crate) fn retire_bundle_without_reconcile(
        mut self,
        mut preserved: Option<LibraryRoutingPreSealDeposit>,
    ) {
        let mut lifecycle = self.lifecycle.lock();
        if self.routing_shutdown.load(Ordering::Acquire) {
            self.armed = false;
            return;
        }
        if lifecycle.pending == Some(self.intent) {
            lifecycle.pending = None;
        }
        self.armed = false;
        let wake = if let Some(mut deposit) = preserved.take() {
            if let Some(existing) = lifecycle.pre_seal.as_mut() {
                existing.merge(deposit);
            } else {
                deposit.id = LibraryRoutingPreSealDeposit::mint_id();
                lifecycle.pre_seal = Some(deposit);
            }
            let (id, telemetry) = lifecycle
                .pre_seal
                .as_ref()
                .map(|stored| (stored.id, stored.telemetry.clone()))
                .expect("preserved pre-seal deposit was installed");
            lifecycle.reconcile_required = Some(LibraryRoutingReconcileRequest { id, telemetry });
            true
        } else {
            false
        };
        drop(lifecycle);
        if wake {
            notify_library_routing_reconcile_edge(
                &self.reconcile_wake,
                &self.reconcile_wake_generation,
            );
        }
    }
}

impl Drop for PendingLibraryReplacementGuard {
    fn drop(&mut self) {
        if !self.armed || self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        let mut lifecycle = self.lifecycle.lock();
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        if lifecycle.pending != Some(self.intent) {
            return;
        }
        lifecycle.pending = None;
        if self.abort_policy == LibraryReplacementAbortPolicy::NoReconcile {
            return;
        }
        let id = NEXT_LIBRARY_ROUTING_RECONCILE_REQUEST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("library-routing reconcile request identity exhausted");
        lifecycle.reconcile_required = Some(LibraryRoutingReconcileRequest {
            id,
            telemetry: self.telemetry.clone(),
        });
        drop(lifecycle);
        notify_library_routing_reconcile_edge(
            &self.reconcile_wake,
            &self.reconcile_wake_generation,
        );
    }
}

/// Exact old authority owned by one package-library routing driver.
///
/// The library `Arc` and operation epoch cover cache work that deliberately
/// does not advance `WorldState` generations. Package/config/routing fields
/// prevent a detached build or watcher event from rebasing onto a successor
/// intent.
#[derive(Clone)]
pub(crate) struct LibraryRoutingBasis {
    pub(crate) library: Arc<PackageLibrary>,
    cache_operation_epoch: u64,
    routing: SystemFileRoutingStamp,
    pub(crate) ready: bool,
    package_input_generation: u64,
    package_config_generation: u64,
    package_state_record_generation: u64,
    packages_enabled: bool,
    packages_r_path: Option<PathBuf>,
    packages_additional_library_paths: Vec<PathBuf>,
    packages_watch_library_paths: bool,
    packages_watch_debounce_ms: u64,
    workspace_folders: Vec<Url>,
    watcher_owner: Option<LibpathWatcherOwner>,
    replacement_intent: Option<LibraryReplacementIntent>,
    adopted_reconcile_obligation: bool,
    mutation: LibraryRoutingMutation,
}

impl LibraryRoutingBasis {
    pub(crate) fn cache_operation_epoch(&self) -> u64 {
        self.cache_operation_epoch
    }

    pub(crate) fn is_replacement(&self) -> bool {
        self.mutation == LibraryRoutingMutation::Replacement
    }

    pub(crate) fn keeps_current_watcher(&self) -> bool {
        matches!(
            self.mutation,
            LibraryRoutingMutation::Changed
                | LibraryRoutingMutation::FullRescan
                | LibraryRoutingMutation::DegradedReconcile
        )
    }

    pub(crate) fn system_file_routing_owner(&self) -> SystemFileRoutingOwnerIdentity {
        self.routing.owner
    }

    pub(crate) fn should_watch_library_paths(&self, ready: bool, library: &PackageLibrary) -> bool {
        self.packages_enabled
            && self.packages_watch_library_paths
            && ready
            && !library.lib_paths().is_empty()
    }

    pub(crate) fn watcher_debounce_ms(&self) -> u64 {
        self.packages_watch_debounce_ms
    }
}

impl OpenPackageWarmBasis {
    pub(crate) fn record_successfully_warmed(&mut self, packages: HashSet<String>) {
        self.requested_packages = packages.clone();
        self.successfully_warmed = packages;
    }
}

#[derive(Clone)]
pub(crate) struct ProspectiveLibraryRouting {
    install_id: u64,
    content_generation: u64,
    routing_owner: SystemFileRoutingOwnerIdentity,
    watcher_owner: LibpathWatcherOwner,
    routing: SystemFileRoutingStamp,
}

impl ProspectiveLibraryRouting {
    pub(crate) fn watcher_owner(&self) -> LibpathWatcherOwner {
        self.watcher_owner
    }
}

pub(crate) struct PreparedLibraryRoutingAnalysis {
    pub(crate) basis: LibraryRoutingBasis,
    pub(crate) prospective: ProspectiveLibraryRouting,
    pub(crate) library: Arc<PackageLibrary>,
    pub(crate) ready: bool,
    pub(crate) warm_basis: Option<OpenPackageWarmBasis>,
    pub(crate) system_file: PreparedSystemFileAnalysis,
    pub(crate) watcher: PreparedLibpathWatcherInstall,
}

#[derive(Clone)]
pub(crate) enum PreparedLibpathWatcherInstall {
    Keep,
    Active {
        handle: Arc<crate::libpath_watcher::LibpathWatcherHandle>,
        journal: Arc<crate::libpath_watcher::LibpathWatchJournal>,
        recovery: bool,
    },
    Disabled,
    AttachFailed {
        recovery: bool,
        can_recover: bool,
    },
}

impl std::fmt::Debug for PreparedLibpathWatcherInstall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keep => formatter.write_str("Keep"),
            Self::Active { recovery, .. } => formatter
                .debug_struct("Active")
                .field("recovery", recovery)
                .finish_non_exhaustive(),
            Self::Disabled => formatter.write_str("Disabled"),
            Self::AttachFailed {
                recovery,
                can_recover,
            } => formatter
                .debug_struct("AttachFailed")
                .field("recovery", recovery)
                .field("can_recover", can_recover)
                .finish(),
        }
    }
}

impl PreparedLibpathWatcherInstall {
    fn is_buffering_active(&self) -> bool {
        match self {
            Self::Active { journal, .. } => journal.is_buffering(),
            Self::Keep | Self::Disabled | Self::AttachFailed { .. } => true,
        }
    }
}

/// Compact proof that an unpublished replacement cache was warmed for the
/// exact open/scope/package authorities that remain current at its final CAS.
///
/// This deliberately contains only tokens and generations. Package collection
/// and cross-file scope traversal happen from a detached snapshot before the R
/// warm; the final state write compares this proof synchronously and never
/// performs scope work while holding `WorldState`.
#[derive(Clone)]
pub(crate) struct OpenPackageWarmBasis {
    candidate_library: Arc<PackageLibrary>,
    workspace_index_version: u64,
    workspace_index_max_files: usize,
    workspace_index_max_file_size_bytes: usize,
    workspace_index_artifact_capacity: usize,
    workspace_index_pinned: HashSet<Url>,
    graph_revision: u64,
    graph_authority_generation: u64,
    open_context_authority_generation: OpenContextAuthorityGeneration,
    editor_eligibility_generation: EditorEligibilityGeneration,
    analysis_config_generation: AnalysisConfigGeneration,
    chunk_override_generation: ChunkOverrideGeneration,
    raw_cache_generation: u64,
    package_input_generation: u64,
    package_config_generation: u64,
    package_state_record_generation: u64,
    workspace_folders: Vec<Url>,
    exclusion_patterns: Vec<String>,
    max_chain_depth: usize,
    max_transitive_dependents_visited: usize,
    backward_dependencies: crate::cross_file::config::BackwardDependencyMode,
    open_records: std::collections::BTreeMap<Url, OpenRecordToken>,
    requested_packages: HashSet<String>,
    successfully_warmed: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibraryRoutingTransferredEffects {
    pub(crate) handle: AnalysisTransferHandle,
    pub(crate) changed_uris: Vec<Url>,
    pub(crate) restart_owner: Option<LibpathWatcherOwner>,
}

struct CapturedSystemFileOpen {
    uri: Url,
    token: OpenRecordToken,
    metadata: Arc<crate::cross_file::CrossFileMetadata>,
    document: Arc<OpenDocumentRecord>,
    graph_roots: Vec<Url>,
}

/// Immutable input for one fully detached `system.file()` convergence pass.
pub(crate) struct CapturedSystemFileAnalysis {
    basis: SystemFileAnalysisBasis,
    only_packages: Option<HashSet<String>>,
    artifacts: Vec<(Url, crate::workspace_index::ArtifactEntry)>,
    full_content: HashMap<Url, String>,
    raw_content: HashMap<Url, String>,
    open: Vec<CapturedSystemFileOpen>,
    graph: DependencyGraph,
    exclusions: crate::config_file::CompiledWorkspaceExclusions,
}

/// Filesystem and graph work completed off the state lock.
pub(crate) struct PreparedSystemFileDraft {
    basis: SystemFileAnalysisBasis,
    index_changes: WorkspaceIndexTargetedChanges,
    open_metadata: Vec<PreparedWorkspaceOpenMetadata>,
    graph: DependencyGraph,
    changed_uris: Vec<Url>,
    content_changed_uris: HashSet<Url>,
    external_observations: Vec<SystemFileExternalObservation>,
}

pub(crate) struct PreparedSystemFileAnalysis {
    basis: SystemFileAnalysisBasis,
    index: Option<PreparedWorkspaceIndexTargetedBatch>,
    open_metadata: Vec<PreparedWorkspaceOpenMetadata>,
    graph: DependencyGraph,
    changed_uris: Vec<Url>,
    content_changed_uris: HashSet<Url>,
    external_observations: Vec<SystemFileExternalObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SystemFileExternalIdentity {
    Valid(crate::cross_file::file_cache::FileSnapshot),
    Missing,
    InvalidBytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemFileExternalObservation {
    path: PathBuf,
    identity: SystemFileExternalIdentity,
}

fn observe_system_file_external_path(path: &Path) -> SystemFileExternalObservation {
    observe_system_file_external(path).0
}

/// Read and classify one external candidate from a single opened file.
///
/// The returned text, snapshot, and validity classification all describe the
/// same bytes. Detached derivation must consume this text instead of reopening
/// the path; otherwise a write between identity capture and parsing could let
/// stale artifacts pass the pre-commit identity check.
fn observe_system_file_external(path: &Path) -> (SystemFileExternalObservation, Option<String>) {
    use std::io::Read;

    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            let identity = if error.kind() == std::io::ErrorKind::NotFound {
                SystemFileExternalIdentity::Missing
            } else {
                SystemFileExternalIdentity::InvalidBytes
            };
            return (
                SystemFileExternalObservation {
                    path: path.to_path_buf(),
                    identity,
                },
                None,
            );
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(_) => {
            return (
                SystemFileExternalObservation {
                    path: path.to_path_buf(),
                    identity: SystemFileExternalIdentity::InvalidBytes,
                },
                None,
            );
        }
    };
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return (
            SystemFileExternalObservation {
                path: path.to_path_buf(),
                identity: SystemFileExternalIdentity::InvalidBytes,
            },
            None,
        );
    }
    let Ok(content) = decode_source(bytes) else {
        return (
            SystemFileExternalObservation {
                path: path.to_path_buf(),
                identity: SystemFileExternalIdentity::InvalidBytes,
            },
            None,
        );
    };
    let identity = SystemFileExternalIdentity::Valid(
        crate::cross_file::file_cache::FileSnapshot::with_content_hash(&metadata, &content),
    );
    (
        SystemFileExternalObservation {
            path: path.to_path_buf(),
            identity,
        },
        Some(content),
    )
}

impl PreparedSystemFileAnalysis {
    pub(crate) fn external_observations_are_current(&self) -> bool {
        self.external_observations
            .iter()
            .all(|expected| observe_system_file_external_path(&expected.path) == *expected)
    }

    #[cfg(test)]
    pub(crate) fn corrupt_last_open_token_for_test(&mut self) {
        if let Some(last) = self.open_metadata.last_mut() {
            last.token = OpenRecordToken::absent_for_test(last.uri.clone());
        }
    }
}

/// One open metadata/artifact replacement derived from the exact immutable
/// record named by `token`.
pub(crate) struct PreparedWorkspaceOpenMetadata {
    uri: Url,
    token: OpenRecordToken,
    prepared: PreparedOpenMetadataReplacement,
}

impl PreparedWorkspaceOpenMetadata {
    pub(crate) fn new(
        uri: Url,
        token: OpenRecordToken,
        prepared: PreparedOpenMetadataReplacement,
    ) -> Self {
        Self {
            uri,
            token,
            prepared,
        }
    }
}

/// Complete off-lock workspace replacement candidate.
///
/// The central commit validates both phases plus every open token before the
/// first mutation, then installs this exact index/graph/open projection.
pub(crate) struct PreparedWorkspaceScanAnalysis {
    input: WorkspaceScanInputBasis,
    basis: WorkspaceScanDerivationBasis,
    artifact_only: Vec<(Url, crate::workspace_index::ArtifactEntry)>,
    full_records: Vec<(
        Url,
        crate::workspace_index::IndexEntry,
        crate::workspace_index::ClosedProvenance,
    )>,
    graph: DependencyGraph,
    open_metadata: Vec<PreparedWorkspaceOpenMetadata>,
    workspace_index_pins: HashSet<Url>,
}

impl PreparedWorkspaceScanAnalysis {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        input: WorkspaceScanInputBasis,
        basis: WorkspaceScanDerivationBasis,
        artifact_only: Vec<(Url, crate::workspace_index::ArtifactEntry)>,
        full_records: Vec<(
            Url,
            crate::workspace_index::IndexEntry,
            crate::workspace_index::ClosedProvenance,
        )>,
        graph: DependencyGraph,
        open_metadata: Vec<PreparedWorkspaceOpenMetadata>,
        workspace_index_pins: HashSet<Url>,
    ) -> Self {
        Self {
            input,
            basis,
            artifact_only,
            full_records,
            graph,
            open_metadata,
            workspace_index_pins,
        }
    }

    /// Corrupt only the final prepared open identity while leaving both
    /// authority bases current, so atomic preflight tests reach the
    /// per-target validation rather than an earlier global-basis rejection.
    #[cfg(test)]
    pub(crate) fn corrupt_last_open_token_for_test(&mut self) {
        self.open_metadata
            .sort_unstable_by(|left, right| left.uri.as_str().cmp(right.uri.as_str()));
        let last = self
            .open_metadata
            .last_mut()
            .expect("test candidate has at least one open target");
        last.token = OpenRecordToken::absent_for_test(last.uri.clone());
    }
}

pub(crate) struct PreparedOpenInstallAnalysis {
    basis: AnalysisBasis,
    intent: OpenInstallIntentToken,
    uri: Url,
    document: Document,
    metadata: Arc<crate::cross_file::CrossFileMetadata>,
    artifacts: Arc<crate::cross_file::scope::ScopeArtifacts>,
    aliases: Vec<Url>,
    package: Option<PreparedPackageProjection>,
    plan: PreparedOpenCommitPlan,
}

pub(crate) struct PreparedOpenCloseAnalysis {
    basis: AnalysisBasis,
    intent: OpenCloseIntentToken,
    uri: Url,
    expected_aliases: Vec<Url>,
    package: Option<PreparedPackageProjection>,
    plan: PreparedOpenCommitPlan,
    resync: Vec<OpenCloseResyncTicket>,
    disk_observations: Vec<OpenCloseDiskObservation>,
    watched_roots: Vec<(Url, Option<u64>, ChunkKind)>,
}

/// Whole package-mode projection reduced off-lock and installed atomically
/// with its owning analysis transaction.
pub(crate) struct PreparedPackageProjection {
    pub(crate) inputs: crate::package_state::PackageInputs,
    pub(crate) state: crate::package_state::PackageState,
    routing_owner: PackageRoutingOwnerPolicy,
}

impl PreparedPackageProjection {
    pub(crate) fn new(
        inputs: crate::package_state::PackageInputs,
        state: crate::package_state::PackageState,
    ) -> Self {
        Self {
            inputs,
            state,
            routing_owner: PackageRoutingOwnerPolicy::IfChanged,
        }
    }

    pub(crate) fn new_seed(
        inputs: crate::package_state::PackageInputs,
        state: crate::package_state::PackageState,
    ) -> Self {
        Self {
            inputs,
            state,
            routing_owner: PackageRoutingOwnerPolicy::FreshSeedOwner,
        }
    }
}

/// Exact raw+derived package authority captured by a detached standalone
/// package projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageProjectionBasis {
    package_input_generation: u64,
    package_state_record_generation: u64,
    workspace_scan_generation: u64,
    package_config_generation: u64,
    open_context_authority_generation: OpenContextAuthorityGeneration,
    workspace_root: Option<std::path::PathBuf>,
    workspace_folders: Vec<Url>,
    exclusion_patterns: Vec<String>,
    package_mode: crate::cross_file::config::PackageMode,
    model_rprofile: bool,
    post_seed_ownership: PostSeedPackageProjectionOwnership,
}

/// Exact authorities consumed by one detached startup sysdata fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SysdataFallbackBasis {
    seed_install_id: u64,
    workspace_root: std::path::PathBuf,
    package: PackageProjectionBasis,
    package_library_install_id: u64,
    package_library_content_generation: u64,
    configured_r_path: Option<std::path::PathBuf>,
    runtime_r_path: std::path::PathBuf,
    runtime_identity: crate::r_subprocess::RRuntimeIdentity,
    analysis_config_generation: AnalysisConfigGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SysdataFallbackOwner {
    seed_install_id: u64,
    pub(crate) workspace_root: std::path::PathBuf,
}

pub(crate) struct SysdataFallbackCommitEffects {
    pub(crate) routing_owner: Option<SystemFileRoutingOwnerIdentity>,
    pub(crate) candidates: Vec<AnalysisTransferCandidate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostSeedPackageProjectionOwnership {
    Unrestricted,
    ForegroundExact(PackageSeedInstalledIdentity),
    ForegroundCurrent(PackageSeedInstalledIdentity),
    Coordinator(PackageSeedInstalledIdentity),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageProjectionInstallRejected {
    StaleBasis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackageRoutingOwnerPolicy {
    /// Mint a new routing owner only when the effective package name or root
    /// changes.
    IfChanged,
    /// A full seed/reseed owns a fresh routing lifecycle even when its
    /// effective name and root compare equal.
    FreshSeedOwner,
}

/// Immutable fresh-disk convergence work released only after the close
/// transaction and its empty diagnostic publication succeed.
#[derive(Debug, Clone)]
pub(crate) struct OpenCloseResyncTicket {
    pub(crate) uri: Url,
    pub(crate) chunk_kind: ChunkKind,
    pub(crate) old_metadata: Option<Arc<crate::cross_file::CrossFileMetadata>>,
    pub(crate) old_interface_hash: Option<u64>,
    pub(crate) expected_watched_generation: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenCloseDiskObservation {
    pub(crate) uri: Url,
    pub(crate) snapshot: Option<crate::cross_file::file_cache::FileSnapshot>,
    /// `false` means the exact observed bytes were undecodable source and the
    /// selected close disposition is Remove rather than InstallClosed.
    pub(crate) decoded_source: bool,
}

pub(crate) struct PreparedOpenCloseDiskInstall {
    pub(crate) uri: Url,
    pub(crate) entry: IndexEntry,
    pub(crate) snapshot: crate::cross_file::file_cache::FileSnapshot,
    pub(crate) content: String,
}

impl PartialEq for OpenCloseResyncTicket {
    fn eq(&self, other: &Self) -> bool {
        self.uri == other.uri
            && self.chunk_kind == other.chunk_kind
            && self.old_interface_hash == other.old_interface_hash
            && self.expected_watched_generation == other.expected_watched_generation
            && match (&self.old_metadata, &other.old_metadata) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for OpenCloseResyncTicket {}

pub(crate) struct PreparedOpenEditAnalysis {
    basis: AnalysisBasis,
    uri: Url,
    prepared: PreparedOpenDocument,
    metadata: Arc<crate::cross_file::CrossFileMetadata>,
    package: Option<PreparedPackageProjection>,
    plan: PreparedOpenCommitPlan,
}

/// One edit prepared from an exact open-analysis basis.
///
/// Preparation is read-only. Raw-cache invalidation and every other visible
/// effect stay behind [`WorldState::try_commit_analysis`].
pub(crate) struct PreparedOpenEdit {
    basis: AnalysisBasis,
    uri: Url,
    prepared: PreparedOpenDocument,
}

impl PreparedOpenEdit {
    pub(crate) fn document(&self) -> &Document {
        self.prepared.document()
    }
}

pub(crate) struct PreparedOpenMetadataAnalysis {
    basis: AnalysisBasis,
    uri: Url,
    expected: AnalysisGeneration,
    metadata: Arc<crate::cross_file::CrossFileMetadata>,
    plan: PreparedOpenCommitPlan,
}

pub(crate) struct PreparedOpenAliasReconcileAnalysis {
    basis: AnalysisBasis,
    uri: Url,
    expected: AnalysisGeneration,
    aliases: Vec<Url>,
    plan: PreparedOpenCommitPlan,
}

/// Immutable inputs for detached didOpen metadata re-enrichment.
///
/// Captured coherently with `basis` under one brief `WorldState` lock. Parsing,
/// inherited-WD traversal, alias-root projection, and path filtering run only
/// after the lock is dropped.
#[derive(Clone)]
pub(crate) struct CapturedOpenMetadataAnalysis {
    basis: AnalysisBasis,
    pub(crate) uri: Url,
    pub(crate) expected: AnalysisGeneration,
    pub(crate) chunk_kind: ChunkKind,
    pub(crate) file_type: FileType,
    pub(crate) analysis_text: String,
    pub(crate) old_metadata: Arc<crate::cross_file::CrossFileMetadata>,
    pub(crate) workspace_root: Option<Url>,
    pub(crate) max_chain_depth: usize,
    pub(crate) workspace_name: Option<String>,
    pub(crate) package_workspace_root: Option<PathBuf>,
    pub(crate) library_paths: Vec<PathBuf>,
    pub(crate) exclusions: crate::config_file::CompiledWorkspaceExclusions,
    pub(crate) graph_roots: Vec<Url>,
    /// Coherent graph snapshot retained for detached fanout/debug analysis.
    /// The commit basis owns the operational graph-revision identity.
    pub(crate) _graph: DependencyGraph,
    pub(crate) metadata_map: HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>>,
    pub(crate) content_map: HashMap<Url, String>,
    pub(crate) raw_content: HashMap<Url, (String, ChunkKind)>,
}

/// One graph-root projection derived from an exact open-analysis basis.
pub(crate) struct PreparedOpenGraphProjection {
    pub(crate) uri: Url,
    pub(crate) graph_metadata: Arc<crate::cross_file::CrossFileMetadata>,
    pub(crate) old_metadata: Option<Arc<crate::cross_file::CrossFileMetadata>>,
    pub(crate) new_metadata: Arc<crate::cross_file::CrossFileMetadata>,
    pub(crate) parent_content: HashMap<Url, String>,
    pub(crate) make_non_lending: bool,
}

/// All state-derived effects that become valid only if an open record commits.
#[derive(Default)]
pub(crate) struct PreparedOpenCommitPlan {
    pub(crate) graph: Vec<PreparedOpenGraphProjection>,
    pub(crate) reset_closed_roots: Vec<Url>,
    /// Retire mutable closed caches for roots that are becoming open while
    /// preserving incoming graph edges and the shadowed disk artifact tier.
    ///
    /// The disk artifact remains as the immediate fallback when the buffer
    /// closes, until close resync atomically replaces it. Closed-file commits
    /// are independently vetoed while an open owner exists.
    pub(crate) retire_closed_roots: Vec<Url>,
    pub(crate) package_event: Option<(Url, Arc<str>)>,
    pub(crate) package_fanout_uris: Vec<Url>,
    pub(crate) package_source_interface_fanout: bool,
    pub(crate) packages_to_prefetch: Vec<String>,
    pub(crate) refresh_pins: bool,
    /// Candidates already selected by an earlier phase of the same handler.
    /// The seam unions, reprioritizes, caps, and marks them with newly-derived
    /// graph/WD/package fanout so no marker can outlive cap eviction.
    pub(crate) seed_revalidation_uris: Vec<Url>,
    /// The handler publishes the subject synchronously (project-excluded
    /// buffers). Exclude it before applying the fanout cap so one dependent
    /// can still be selected when the cap is one.
    pub(crate) direct_subject_publish: bool,
    /// Override for operations such as didOpen re-enrichment, whose subject
    /// uses the dependent debounce rather than the edited-file debounce.
    pub(crate) subject_debounce_ms: Option<u64>,
    /// Close transitions remove the subject record before applying this plan.
    /// The replacement interface comes from a surviving alias or retained
    /// closed shadow, and the closed subject itself is never scheduled.
    pub(crate) closing_subject: bool,
    pub(crate) replacement_interface_hash: Option<u64>,
    /// Fresh disk projections selected and parsed off-lock, then revalidated
    /// under the diagnostics publish lock immediately before the close CAS.
    pub(crate) close_disk_installs: Vec<PreparedOpenCloseDiskInstall>,
}

#[derive(Default)]
struct OpenCommitPlanEffects {
    revalidations: Vec<AnalysisRevalidationTicket>,
    transfer_candidates: Vec<AnalysisTransferCandidate>,
    package_routing_owner: Option<SystemFileRoutingOwnerIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenAnalysisCommitOutcome {
    pub(crate) generation: AnalysisGeneration,
    pub(crate) provenance: crate::open_document_store::OpenDocumentProvenance,
    pub(crate) packages_to_prefetch: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenCloseCommitOutcome {
    pub(crate) resync: Vec<OpenCloseResyncTicket>,
}

pub(crate) enum PreparedClosedMutation {
    Upsert(Box<PreparedClosedAnalysis>),
    Remove { basis: Box<AnalysisBasis>, uri: Url },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnalysisRevalidationTicket {
    pub(crate) uri: Url,
    pub(crate) trigger: DiagnosticsTrigger,
    pub(crate) debounce_ms: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnalysisRevalidationTicketFingerprint {
    pub(crate) uri: Url,
    pub(crate) trigger: DiagnosticsTrigger,
    pub(crate) debounce_ms: u64,
}

#[cfg(test)]
impl From<&AnalysisRevalidationTicket> for AnalysisRevalidationTicketFingerprint {
    fn from(ticket: &AnalysisRevalidationTicket) -> Self {
        Self {
            uri: ticket.uri.clone(),
            trigger: ticket.trigger,
            debounce_ms: ticket.debounce_ms,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchedFinalHandoffOutcome {
    Finalized,
    RetiredBeforeFinalHandoff,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchedFinalHandoffForTest {
    pub(crate) outcome: WatchedFinalHandoffOutcome,
    pub(crate) reserved: Vec<AnalysisRevalidationTicketFingerprint>,
    pub(crate) transferred: Vec<AnalysisRevalidationTicketFingerprint>,
}

#[cfg(test)]
impl Default for WatchedFinalHandoffForTest {
    fn default() -> Self {
        Self {
            outcome: WatchedFinalHandoffOutcome::RetiredBeforeFinalHandoff,
            reserved: Vec::new(),
            transferred: Vec::new(),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigReloadPublishForTest {
    pub(crate) scheduled: Vec<Url>,
    pub(crate) completed: Vec<Url>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CloseResyncConsumerForTest {
    Reserved(AnalysisRevalidationTicketFingerprint),
    Affected(Url),
}

#[cfg(test)]
impl CloseResyncConsumerForTest {
    pub(crate) fn uri(&self) -> &Url {
        match self {
            Self::Reserved(ticket) => &ticket.uri,
            Self::Affected(uri) => uri,
        }
    }
}

#[cfg(test)]
struct FinalHandoffCaptureGate<T> {
    payload: std::sync::Mutex<Option<T>>,
    operation_id: u64,
    owner: String,
    claimed: AtomicBool,
    recorded: AtomicBool,
    completed: AtomicBool,
    outstanding: std::sync::Mutex<std::collections::BTreeMap<u64, &'static str>>,
    abnormal_exits: std::sync::Mutex<Vec<(u64, &'static str)>>,
    next_child_id: AtomicU64,
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completion: tokio::sync::Notify,
}

#[cfg(test)]
impl<T> FinalHandoffCaptureGate<T> {
    fn new(operation_id: u64, owner: String) -> Self {
        Self {
            payload: std::sync::Mutex::new(None),
            operation_id,
            owner,
            claimed: AtomicBool::new(false),
            recorded: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            outstanding: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            abnormal_exits: std::sync::Mutex::new(Vec::new()),
            next_child_id: AtomicU64::new(1),
            arrived: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            completion: tokio::sync::Notify::new(),
        }
    }

    fn register(self: &Arc<Self>, label: &'static str) -> FinalHandoffCompletionToken<T> {
        let id = self.register_id(label);
        FinalHandoffCompletionToken {
            gate: Arc::clone(self),
            id: Some(id),
        }
    }

    fn register_id(&self, label: &'static str) -> u64 {
        let id = self.next_child_id.fetch_add(1, Ordering::Relaxed);
        let mut outstanding = self.outstanding.lock().unwrap();
        assert!(
            !self.completed.load(Ordering::Acquire),
            "cannot register a child after causal completion"
        );
        let previous = outstanding.insert(id, label);
        debug_assert!(previous.is_none());
        id
    }

    fn release_id(&self, id: u64, abnormal: bool) {
        let mut outstanding = self.outstanding.lock().unwrap();
        let label = outstanding
            .remove(&id)
            .expect("causal completion token must own one registered child");
        if abnormal {
            self.abnormal_exits.lock().unwrap().push((id, label));
        }
        let completed = outstanding.is_empty();
        if completed {
            self.completed.store(true, Ordering::Release);
        }
        drop(outstanding);
        if completed {
            self.completion.notify_waiters();
        }
    }
}

#[cfg(test)]
trait FinalHandoffCausalGate: Send + Sync {
    fn register_causal_child(&self, label: &'static str) -> u64;
    fn release_causal_child(&self, id: u64, abnormal: bool);
}

#[cfg(test)]
impl<T: Send + Sync> FinalHandoffCausalGate for FinalHandoffCaptureGate<T> {
    fn register_causal_child(&self, label: &'static str) -> u64 {
        self.register_id(label)
    }

    fn release_causal_child(&self, id: u64, abnormal: bool) {
        self.release_id(id, abnormal);
    }
}

/// Type-erased test-only access to one final-handoff capture's causal gate.
///
/// Diagnostic backstops do not know the typed payload captured by the
/// originating handler. This context lets them register a labeled child
/// synchronously before spawn while retaining the capture's exact completion
/// and abnormal-exit semantics.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct FinalHandoffCausalContext {
    gate: Arc<dyn FinalHandoffCausalGate>,
}

#[cfg(test)]
impl FinalHandoffCausalContext {
    pub(crate) fn child(&self, label: &'static str) -> FinalHandoffCausalToken {
        let id = self.gate.register_causal_child(label);
        FinalHandoffCausalToken {
            context: self.clone(),
            id: Some(id),
        }
    }
}

/// Type-erased causal child used by diagnostics work that recursively spawns.
#[cfg(test)]
pub(crate) struct FinalHandoffCausalToken {
    context: FinalHandoffCausalContext,
    id: Option<u64>,
}

#[cfg(test)]
pub(crate) type DiagnosticsSupersessionHandoffMapForTest = Arc<
    std::sync::Mutex<HashMap<(Url, u64), (FinalHandoffCausalContext, FinalHandoffCausalToken)>>,
>;

#[cfg(test)]
impl FinalHandoffCausalToken {
    pub(crate) fn finish(mut self) {
        self.release(false);
    }

    fn release(&mut self, abnormal: bool) {
        if let Some(id) = self.id.take() {
            self.context.gate.release_causal_child(id, abnormal);
        }
    }
}

#[cfg(test)]
impl Drop for FinalHandoffCausalToken {
    fn drop(&mut self) {
        self.release(true);
    }
}

/// Test-only one-shot capture claimed by the handler invocation that was
/// armed immediately before it started.
///
/// Claiming at the handler boundary, rather than writing a shared "latest"
/// snapshot at finalization time, prevents unrelated setup or deferred work
/// from overwriting or satisfying the target invocation's assertion.
///
/// A claim captures the first final handoff reached by that invocation,
/// including its in-place CAS/package retries and deferred-routing tail.
/// Empty payloads are valid. Separately spawned work, such as a delayed
/// undecodable watched-file retry, belongs to a new invocation and does not
/// inherit the claim. If cloned finalizers race, only the first records and
/// pauses; later calls return without replacing the payload.
///
/// The winning recorder also receives the causal root token. It registers
/// labeled finite descendants synchronously before spawn, finishes the root
/// only after admission closes, and lets the last token durably signal
/// completion. Dropping an unfinished token records an abnormal exit so task
/// cancellation or unwind cannot masquerade as success. Payload arrival and
/// causal completion are separate boundaries.
#[cfg(test)]
pub(crate) struct FinalHandoffCapture<T> {
    armed: std::sync::Mutex<Option<Arc<FinalHandoffCaptureGate<T>>>>,
}

#[cfg(test)]
impl<T> Default for FinalHandoffCapture<T> {
    fn default() -> Self {
        Self {
            armed: std::sync::Mutex::new(None),
        }
    }
}

#[cfg(test)]
impl<T> FinalHandoffCapture<T> {
    pub(crate) fn arm(&self) -> FinalHandoffCaptureHandle<T> {
        self.arm_for("")
    }

    pub(crate) fn arm_for(&self, owner: impl Into<String>) -> FinalHandoffCaptureHandle<T> {
        static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);
        let operation_id = NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed);
        let gate = Arc::new(FinalHandoffCaptureGate::new(operation_id, owner.into()));
        let replaced = self.armed.lock().unwrap().replace(gate.clone());
        assert!(
            replaced.is_none(),
            "a final-handoff capture is already armed"
        );
        FinalHandoffCaptureHandle { gate }
    }

    pub(crate) fn claim(&self) -> Option<FinalHandoffCaptureClaim<T>> {
        self.armed.lock().unwrap().take().map(|gate| {
            gate.claimed.store(true, Ordering::Release);
            FinalHandoffCaptureClaim { gate }
        })
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct FinalHandoffCaptureClaim<T> {
    gate: Arc<FinalHandoffCaptureGate<T>>,
}

#[cfg(test)]
impl<T> FinalHandoffCaptureClaim<T> {
    fn record(&self, payload: T) -> Option<FinalHandoffCompletionToken<T>> {
        if self.gate.recorded.swap(true, Ordering::AcqRel) {
            return None;
        }
        let root = self.gate.register("root");
        *self.gate.payload.lock().unwrap() = Some(payload);
        self.gate.arrived.notify_one();
        Some(root)
    }

    /// Record the first final handoff and return its causal root token.
    ///
    /// The caller must register every finite child before spawning it, then
    /// drop the returned root only after no more children can be admitted.
    /// Cloned finalizers that lose the first-record race receive `None` and
    /// cannot complete the winning invocation.
    pub(crate) async fn record_and_pause(
        &self,
        payload: T,
    ) -> Option<FinalHandoffCompletionToken<T>> {
        let root = self.record(payload)?;
        self.gate.release.notified().await;
        Some(root)
    }

    /// Open a causal phase before its first typed handoff is known.
    ///
    /// The caller must register every phase child before dropping this root.
    /// Use [`Self::record_and_pause_in_phase`] for the first payload; cloned
    /// finalizers may call it, but only the winner records and pauses.
    pub(crate) fn begin_causal_phase(&self) -> FinalHandoffCompletionToken<T> {
        self.gate.register("root")
    }

    /// Record the first typed handoff under an already-open causal phase.
    pub(crate) async fn record_and_pause_in_phase(&self, payload: T) {
        if self.gate.recorded.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.gate.payload.lock().unwrap() = Some(payload);
        self.gate.arrived.notify_one();
        self.gate.release.notified().await;
    }

    /// Record an owned empty/no-op handoff that has no asynchronous tail.
    pub(crate) fn record_completed(&self, payload: T) {
        if let Some(root) = self.record(payload) {
            root.finish();
        }
    }

    /// Record a terminal claim-lineage drop.
    ///
    /// Normal last-owner retirement is a completed empty handoff. Unwinding
    /// drops the root unfinished so the existing abnormal-exit diagnostics
    /// distinguish a panic from an intentional terminal return.
    fn record_terminal_drop(&self, payload: T) {
        if let Some(root) = self.record(payload)
            && !std::thread::panicking()
        {
            root.finish();
        }
    }
}

/// Shared ownership of one claimed final-handoff lineage.
///
/// Retry batches and deferred finalizers may clone this wrapper freely, but
/// they never clone the underlying claim. Normal finalization records through
/// [`Self::claim`]. If every owner exits before that boundary, the last owner
/// records a typed terminal payload so tests cannot hang at
/// `claimed=true, recorded=false`. The payload's type must distinguish that
/// retirement from a real final handoff.
#[cfg(test)]
pub(crate) struct FinalHandoffClaimLineage<T: Default> {
    inner: Arc<FinalHandoffClaimLineageInner<T>>,
}

#[cfg(test)]
impl<T: Default> Clone for FinalHandoffClaimLineage<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
impl<T: Default> FinalHandoffClaimLineage<T> {
    pub(crate) fn new(claim: FinalHandoffCaptureClaim<T>) -> Self {
        Self {
            inner: Arc::new(FinalHandoffClaimLineageInner { claim }),
        }
    }

    pub(crate) fn claim(&self) -> &FinalHandoffCaptureClaim<T> {
        &self.inner.claim
    }
}

#[cfg(test)]
struct FinalHandoffClaimLineageInner<T: Default> {
    claim: FinalHandoffCaptureClaim<T>,
}

#[cfg(test)]
impl<T: Default> Drop for FinalHandoffClaimLineageInner<T> {
    fn drop(&mut self) {
        self.claim.record_terminal_drop(T::default());
    }
}

/// Panic- and cancellation-safe ownership of one finite descendant of a
/// captured handler invocation.
///
/// Registration is synchronous and happens before the descendant is spawned.
/// Dropping the last token durably completes the capture and wakes receipt
/// waiters. A label remains visible in timeout diagnostics until that exact
/// descendant exits.
#[cfg(test)]
pub(crate) struct FinalHandoffCompletionToken<T> {
    gate: Arc<FinalHandoffCaptureGate<T>>,
    id: Option<u64>,
}

#[cfg(test)]
impl<T> FinalHandoffCompletionToken<T> {
    pub(crate) fn child(&self, label: &'static str) -> Self {
        self.gate.register(label)
    }

    pub(crate) fn causal_context(&self) -> FinalHandoffCausalContext
    where
        T: Send + Sync + 'static,
    {
        FinalHandoffCausalContext {
            gate: self.gate.clone(),
        }
    }

    pub(crate) fn finish(mut self) {
        self.release(false);
    }

    fn release(&mut self, abnormal: bool) {
        let Some(id) = self.id.take() else {
            return;
        };
        self.gate.release_id(id, abnormal);
    }
}

#[cfg(test)]
impl<T> Drop for FinalHandoffCompletionToken<T> {
    fn drop(&mut self) {
        self.release(true);
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FinalHandoffCaptureStatus {
    pub(crate) operation_id: u64,
    pub(crate) owner: String,
    pub(crate) claimed: bool,
    pub(crate) recorded: bool,
    pub(crate) completed: bool,
    pub(crate) outstanding: Vec<(u64, &'static str)>,
    pub(crate) abnormal_exits: Vec<(u64, &'static str)>,
}

#[cfg(test)]
pub(crate) struct FinalHandoffCaptureHandle<T> {
    gate: Arc<FinalHandoffCaptureGate<T>>,
}

#[cfg(test)]
impl<T: Clone> FinalHandoffCaptureHandle<T> {
    pub(crate) async fn wait_payload(&self) -> T {
        loop {
            if let Some(payload) = self.gate.payload.lock().unwrap().clone() {
                return payload;
            }
            self.gate.arrived.notified().await;
        }
    }

    pub(crate) fn release(&self) {
        self.gate.release.notify_one();
    }

    pub(crate) async fn wait_completed(&self) {
        loop {
            let notified = self.gate.completion.notified();
            if self.gate.completed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn status(&self) -> FinalHandoffCaptureStatus {
        FinalHandoffCaptureStatus {
            operation_id: self.gate.operation_id,
            owner: self.gate.owner.clone(),
            claimed: self.gate.claimed.load(Ordering::Acquire),
            recorded: self.gate.recorded.load(Ordering::Acquire),
            completed: self.gate.completed.load(Ordering::Acquire),
            outstanding: self
                .gate
                .outstanding
                .lock()
                .unwrap()
                .iter()
                .map(|(id, label)| (*id, *label))
                .collect(),
            abnormal_exits: self.gate.abnormal_exits.lock().unwrap().clone(),
        }
    }
}

/// Exact unmarked diagnostic work carried across a multi-phase analysis
/// transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AnalysisTransferCandidate {
    pub(crate) uri: Url,
    record: OpenRecordToken,
    trigger: DiagnosticsTrigger,
    reservation: AnalysisTransferReservationPolicy,
}

/// Reservation semantics captured by the outer transaction and retained
/// through transfer union, capping, and delayed finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnalysisTransferReservationPolicy {
    /// Edited/open subject: always wins the cap, keeps its exact debounce, and
    /// relies on its document version instead of a force-republish marker.
    Subject { debounce_ms: u64 },
    /// Dependent/system/scan candidate: activity-prioritized, force-marked,
    /// and scheduled with the current dependent debounce.
    Dependent,
}

/// One-shot identity for unmarked workspace-scan fanout transferred to
/// post-seed/config orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct WorkspaceScanTransferIdentity {
    intent_generation: u64,
    commit_generation: u64,
    committed_scan_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceScanTransferredEffects {
    pub(crate) handle: AnalysisTransferHandle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemFileTransferredEffects {
    pub(crate) handle: AnalysisTransferHandle,
    pub(crate) changed_uris: Vec<Url>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PackageSeedInstalledIdentity {
    pub(crate) seed_install_id: u64,
    pub(crate) package_config_generation: u64,
    pub(crate) package_input_generation: u64,
    pub(crate) package_state_record_generation: u64,
    pub(crate) system_file_routing_owner: SystemFileRoutingOwnerIdentity,
    pub(crate) package_library_install_id: u64,
    pub(crate) package_library_content_generation: u64,
}

/// Exact never-reused owner of one `system.file()` routing lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SystemFileRoutingOwnerIdentity(u64);

/// Exact provenance of one libpath watcher/consumer lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct LibpathWatcherOwner(u64);

/// Applied state of the libpath watcher for the current exact owner lineage.
///
/// `AwaitingRecovery` is a one-shot token: only the same never-reused owner may
/// install the recovery watcher. `Degraded` is terminal until an external
/// settings/library edge starts a fresh primary lineage. A watcher-only
/// transition into degradation carries one exact `reconcile_pending`
/// obligation to clear/warm the current library without another watcher
/// attachment; a routing-CAS degradation has already done that work and stores
/// no obligation. `ActiveUnapplied` preserves a healthy old watcher when a
/// same-coverage settings-only replacement fails to attach.
enum LibpathWatcherState {
    Disabled,
    Active {
        handle: Option<Arc<crate::libpath_watcher::LibpathWatcherHandle>>,
        journal: Arc<crate::libpath_watcher::LibpathWatchJournal>,
        is_recovery: bool,
        applied: LibpathWatcherSpec,
    },
    AwaitingRecovery,
    Degraded {
        reconcile_pending: bool,
    },
    ActiveUnapplied {
        handle: Option<Arc<crate::libpath_watcher::LibpathWatcherHandle>>,
        journal: Arc<crate::libpath_watcher::LibpathWatchJournal>,
        is_recovery: bool,
        applied: LibpathWatcherSpec,
        desired: LibpathWatcherSpec,
    },
}

#[derive(Clone, PartialEq, Eq)]
struct LibpathWatcherSpec {
    paths: Vec<PathBuf>,
    debounce_ms: u64,
}

#[derive(Clone, PartialEq, Eq)]
enum LibpathWatcherLifecycleSignature {
    Disabled,
    Active {
        is_recovery: bool,
        applied: LibpathWatcherSpec,
    },
    AwaitingRecovery,
    Degraded {
        reconcile_pending: bool,
    },
    ActiveUnapplied {
        is_recovery: bool,
        applied: LibpathWatcherSpec,
        desired: LibpathWatcherSpec,
    },
}

impl LibpathWatcherState {
    fn active_journal(&self) -> Option<&Arc<crate::libpath_watcher::LibpathWatchJournal>> {
        match self {
            Self::Active { journal, .. } | Self::ActiveUnapplied { journal, .. } => Some(journal),
            Self::Disabled | Self::AwaitingRecovery | Self::Degraded { .. } => None,
        }
    }

    fn signature(&self) -> LibpathWatcherLifecycleSignature {
        match self {
            Self::Disabled => LibpathWatcherLifecycleSignature::Disabled,
            Self::Active {
                is_recovery,
                applied,
                ..
            } => LibpathWatcherLifecycleSignature::Active {
                is_recovery: *is_recovery,
                applied: applied.clone(),
            },
            Self::AwaitingRecovery => LibpathWatcherLifecycleSignature::AwaitingRecovery,
            Self::Degraded { reconcile_pending } => LibpathWatcherLifecycleSignature::Degraded {
                reconcile_pending: *reconcile_pending,
            },
            Self::ActiveUnapplied {
                is_recovery,
                applied,
                desired,
                ..
            } => LibpathWatcherLifecycleSignature::ActiveUnapplied {
                is_recovery: *is_recovery,
                applied: applied.clone(),
                desired: desired.clone(),
            },
        }
    }

    /// Retire callback delivery synchronously and return the OS handle for
    /// potentially blocking teardown after the `WorldState` lock is released.
    fn retire(&mut self) -> Option<Arc<crate::libpath_watcher::LibpathWatcherHandle>> {
        let old = std::mem::replace(self, Self::Disabled);
        match old {
            Self::Active {
                handle, journal, ..
            }
            | Self::ActiveUnapplied {
                handle, journal, ..
            } => {
                journal.close();
                handle
            }
            Self::Disabled | Self::AwaitingRecovery | Self::Degraded { .. } => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct LibpathWatcherSwapBasis {
    library: Arc<PackageLibrary>,
    install_id: u64,
    content_generation: u64,
    current_owner: LibpathWatcherOwner,
    lifecycle: LibpathWatcherLifecycleSignature,
    pub(crate) prospective_owner: LibpathWatcherOwner,
    ready: bool,
    packages_enabled: bool,
    watch_enabled: bool,
    debounce_ms: u64,
    library_paths: Vec<PathBuf>,
}

pub(crate) struct LibpathWatcherSwapCommit {
    pub(crate) retired_handle: Option<Arc<crate::libpath_watcher::LibpathWatcherHandle>>,
    pub(crate) recovery_owner: Option<LibpathWatcherOwner>,
    pub(crate) degraded_reconcile_owner: Option<LibpathWatcherOwner>,
}

impl LibpathWatcherSwapBasis {
    pub(crate) fn should_watch(&self) -> bool {
        self.packages_enabled && self.watch_enabled && self.ready && !self.library_paths.is_empty()
    }

    pub(crate) fn library_paths(&self) -> Vec<PathBuf> {
        self.library_paths.clone()
    }

    pub(crate) fn debounce_ms(&self) -> u64 {
        self.debounce_ms
    }

    fn desired_spec(&self) -> LibpathWatcherSpec {
        LibpathWatcherSpec {
            paths: self.library_paths.clone(),
            debounce_ms: self.debounce_ms,
        }
    }
}

impl WorldState {
    /// Whether the complete package seed/library/routing record installed by a
    /// seed transaction is still the current owner.
    pub(crate) fn package_seed_installed_identity_is_current(
        &self,
        identity: PackageSeedInstalledIdentity,
    ) -> bool {
        self.package_input_generation() == identity.package_input_generation
            && self.package_state_record_generation == identity.package_state_record_generation
            && self.package_config_generation == identity.package_config_generation
            && self.system_file_routing_owner_identity() == identity.system_file_routing_owner
            && self.package_library_install_id == identity.package_library_install_id
            && self.package_library_content_generation
                == identity.package_library_content_generation
            && self.package_seed_install_id == identity.seed_install_id
    }

    /// Whether `identity` still owns post-seed convergence after package,
    /// library, or configuration generations have advanced.
    ///
    /// Only a new seed installation transfers this ownership. Broader
    /// generations may change without an orchestrator that owns a successor
    /// source-following tail, so they require current-basis recapture instead
    /// of terminal cancellation.
    pub(crate) fn package_seed_tail_owner_is_current(
        &self,
        identity: PackageSeedInstalledIdentity,
    ) -> bool {
        self.package_seed_install_id == identity.seed_install_id
    }

    pub(crate) fn begin_system_file_seed_retry(
        &mut self,
        identity: PackageSeedInstalledIdentity,
    ) -> bool {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return false;
        }
        if self.pending_system_file_seed_retry.is_some() {
            return false;
        }
        self.pending_system_file_seed_retry = Some(identity);
        true
    }

    pub(crate) fn system_file_seed_retry_is_current(
        &self,
        identity: PackageSeedInstalledIdentity,
    ) -> bool {
        self.pending_system_file_seed_retry == Some(identity)
    }

    pub(crate) fn system_file_seed_retry_owner(&self) -> Option<PackageSeedInstalledIdentity> {
        self.pending_system_file_seed_retry
    }

    pub(crate) fn complete_system_file_seed_retry(
        &mut self,
        identity: PackageSeedInstalledIdentity,
    ) {
        if self.pending_system_file_seed_retry == Some(identity) {
            self.pending_system_file_seed_retry = None;
        }
    }

    pub(crate) fn begin_post_seed_refresh_retry(
        &mut self,
        identity: PackageSeedInstalledIdentity,
    ) -> PostSeedRefreshOwnerRegistration {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return PostSeedRefreshOwnerRegistration::Shutdown;
        }
        match self.pending_post_seed_refresh_retry {
            None => {
                self.pending_post_seed_refresh_retry = Some(identity);
                PostSeedRefreshOwnerRegistration::Added
            }
            Some(current) if current == identity => PostSeedRefreshOwnerRegistration::ExistingSame,
            Some(current) => PostSeedRefreshOwnerRegistration::ExistingDifferent(current),
        }
    }

    /// Registers one post-seed coordinator and its exact routing dependency.
    ///
    /// Both ownership records are inspected and, for a new coordinator, installed
    /// under the caller's single `WorldState` write lock. A different existing
    /// owner is never replaced, so no worker can observe a post-seed owner without
    /// the routing dependency that must complete before its tail may finalize.
    pub(crate) fn begin_post_seed_refresh_retry_with_system_dependency(
        &mut self,
        identity: PackageSeedInstalledIdentity,
        requires_system_file_retry: bool,
    ) -> PostSeedRefreshOwnerRegistration {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return PostSeedRefreshOwnerRegistration::Shutdown;
        }
        match self.pending_post_seed_refresh_retry {
            Some(current) if current != identity => {
                return PostSeedRefreshOwnerRegistration::ExistingDifferent(current);
            }
            _ => {}
        }
        if requires_system_file_retry
            && let Some(current) = self.pending_system_file_seed_retry
            && current != identity
        {
            return PostSeedRefreshOwnerRegistration::ExistingDifferent(current);
        }

        let registration = self.begin_post_seed_refresh_retry(identity);
        if registration == PostSeedRefreshOwnerRegistration::Added && requires_system_file_retry {
            let added = self.begin_system_file_seed_retry(identity);
            debug_assert!(added || self.system_file_seed_retry_is_current(identity));
        }
        if registration == PostSeedRefreshOwnerRegistration::Added {
            self.pending_post_seed_requires_system_transfer = requires_system_file_retry;
        }
        registration
    }

    pub(crate) fn post_seed_refresh_retry_is_current(
        &self,
        identity: PackageSeedInstalledIdentity,
    ) -> bool {
        self.pending_post_seed_refresh_retry == Some(identity)
    }

    pub(crate) fn post_seed_refresh_retry_owner(&self) -> Option<PackageSeedInstalledIdentity> {
        self.pending_post_seed_refresh_retry
    }

    pub(crate) fn complete_post_seed_refresh_retry(
        &mut self,
        identity: PackageSeedInstalledIdentity,
    ) {
        if self.pending_post_seed_refresh_retry == Some(identity) {
            self.pending_post_seed_refresh_retry = None;
            self.pending_post_seed_requires_system_transfer = false;
            self.pending_post_seed_outer_handles.clear();
            self.pending_post_seed_outer_candidates.clear();
        }
        if self
            .pending_post_seed_system_transfer
            .as_ref()
            .is_some_and(|(owner, _)| *owner == identity)
        {
            self.pending_post_seed_system_transfer = None;
        }
    }

    pub(crate) fn retain_post_seed_outer_finalization(
        &mut self,
        identity: PackageSeedInstalledIdentity,
        handles: impl IntoIterator<Item = AnalysisTransferHandle>,
        candidates: impl IntoIterator<Item = AnalysisTransferCandidate>,
    ) -> bool {
        if self.pending_post_seed_refresh_retry != Some(identity) {
            return false;
        }
        self.pending_post_seed_outer_handles.extend(handles);
        self.pending_post_seed_outer_candidates.extend(candidates);
        true
    }

    pub(crate) fn take_post_seed_outer_finalization(
        &mut self,
        identity: PackageSeedInstalledIdentity,
    ) -> Option<(Vec<AnalysisTransferHandle>, Vec<AnalysisTransferCandidate>)> {
        (self.pending_post_seed_refresh_retry == Some(identity)).then(|| {
            (
                std::mem::take(&mut self.pending_post_seed_outer_handles),
                std::mem::take(&mut self.pending_post_seed_outer_candidates),
            )
        })
    }

    pub(crate) fn retain_post_seed_system_transfer(
        &mut self,
        identity: PackageSeedInstalledIdentity,
        handle: AnalysisTransferHandle,
    ) -> bool {
        if self.pending_post_seed_refresh_retry != Some(identity) {
            return false;
        }
        self.pending_post_seed_system_transfer = Some((identity, handle));
        true
    }

    pub(crate) fn take_post_seed_system_transfer(
        &mut self,
        identity: PackageSeedInstalledIdentity,
    ) -> Option<AnalysisTransferHandle> {
        if self
            .pending_post_seed_system_transfer
            .as_ref()
            .is_some_and(|(owner, _)| *owner == identity)
        {
            return self
                .pending_post_seed_system_transfer
                .take()
                .map(|(_, handle)| handle);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SystemFileTransferIdentity {
    routing_owner: SystemFileRoutingOwnerIdentity,
    commit_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AnalysisTransferIdentity {
    WorkspaceScan(WorkspaceScanTransferIdentity),
    SystemFile(SystemFileTransferIdentity),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AnalysisTransferHandle {
    identity: AnalysisTransferIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostSeedRefreshOwnerRegistration {
    Added,
    ExistingSame,
    ExistingDifferent(PackageSeedInstalledIdentity),
    Shutdown,
}

#[cfg(test)]
impl AnalysisTransferHandle {
    pub(crate) fn system_file_routing_owner_for_test(
        self,
    ) -> Option<SystemFileRoutingOwnerIdentity> {
        match self.identity {
            AnalysisTransferIdentity::WorkspaceScan(_) => None,
            AnalysisTransferIdentity::SystemFile(identity) => Some(identity.routing_owner),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageRoutingCommitEffects {
    pub(crate) owner: SystemFileRoutingOwnerIdentity,
    pub(crate) candidates: Vec<AnalysisTransferCandidate>,
    pub(crate) handoff: Option<AnalysisTransferHandle>,
}

#[derive(Clone, Default)]
struct LibraryRoutingTail {
    post_seed: Option<LibraryRoutingPreSealPostSeed>,
    retired_post_seed_owners: Vec<PackageSeedInstalledIdentity>,
    build_notes: Vec<String>,
}

impl LibraryRoutingTail {
    fn from_deposit(deposit: &mut LibraryRoutingPreSealDeposit) -> Option<Self> {
        let tail = Self {
            post_seed: deposit.post_seed.take(),
            retired_post_seed_owners: std::mem::take(&mut deposit.retired_post_seed_owners),
            build_notes: std::mem::take(&mut deposit.build_notes),
        };
        (!tail.is_empty()).then_some(tail)
    }

    fn is_empty(&self) -> bool {
        self.post_seed.is_none()
            && self.retired_post_seed_owners.is_empty()
            && self.build_notes.is_empty()
    }

    fn merge(&mut self, mut incoming: Self) {
        if let Some(incoming_post_seed) = incoming.post_seed.take() {
            match self.post_seed.take() {
                Some(mut existing)
                    if existing.identity.seed_install_id
                        == incoming_post_seed.identity.seed_install_id =>
                {
                    if existing.deferred_system_file.is_none() {
                        existing.deferred_system_file = incoming_post_seed.deferred_system_file;
                    } else if incoming_post_seed.deferred_system_file
                        != existing.deferred_system_file
                        && let Some(retired) = incoming_post_seed.deferred_system_file
                    {
                        self.retired_post_seed_owners.push(retired);
                    }
                    self.post_seed = Some(existing);
                }
                Some(existing)
                    if existing.identity.seed_install_id
                        > incoming_post_seed.identity.seed_install_id =>
                {
                    self.retire_post_seed(incoming_post_seed);
                    self.post_seed = Some(existing);
                }
                Some(existing) => {
                    self.retire_post_seed(existing);
                    self.post_seed = Some(incoming_post_seed);
                }
                None => self.post_seed = Some(incoming_post_seed),
            }
        }
        self.retired_post_seed_owners
            .append(&mut incoming.retired_post_seed_owners);
        self.retired_post_seed_owners
            .sort_unstable_by_key(|identity| identity.seed_install_id);
        self.retired_post_seed_owners.dedup();
        self.build_notes.append(&mut incoming.build_notes);
    }

    fn retire_post_seed(&mut self, owner: LibraryRoutingPreSealPostSeed) {
        self.retired_post_seed_owners.push(owner.identity);
        if let Some(system) = owner.deferred_system_file
            && system != owner.identity
        {
            self.retired_post_seed_owners.push(system);
        }
    }
}

#[derive(Clone)]
struct AnalysisTransferState {
    candidates: Vec<AnalysisTransferCandidate>,
    /// Package-routing obligations stay on the successor lineage until a
    /// finalizer atomically registers their durable retry owner.
    routing_tail: Option<LibraryRoutingTail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LibraryRoutingTailClaim {
    None,
    NotesOnly,
    PostSeedAdded(LibraryRoutingPreSealPostSeed),
    PostSeedExisting,
    Blocked,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AnalysisTransferFinalizationId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnalysisTransferRejection {
    AlreadyConsumed {
        handle: AnalysisTransferHandle,
    },
    Superseded {
        previous: AnalysisTransferHandle,
        successor: AnalysisTransferHandle,
    },
    MissingOrWrongOwner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AnalysisTransferFinalization {
    Committed(Vec<AnalysisRevalidationTicket>),
    AlreadyFinalized,
}

/// Parsed payload owned by one active-document notification arrival.
pub(crate) struct PreparedOpenLifecycleBatch {
    intent: OpenLifecycleIntentToken,
    active_uri: Option<Url>,
    visible_uris: Vec<Url>,
    timestamp_ms: u64,
    diagnostic_uris: Option<HashSet<Url>>,
}

impl PreparedOpenLifecycleBatch {
    pub(crate) fn new(
        intent: OpenLifecycleIntentToken,
        active_uri: Option<Url>,
        visible_uris: Vec<Url>,
        timestamp_ms: u64,
        diagnostic_uris: Option<HashSet<Url>>,
    ) -> Self {
        Self {
            intent,
            active_uri,
            visible_uris,
            timestamp_ms,
            diagnostic_uris,
        }
    }

    pub(crate) fn has_lifecycle_transition(&self) -> bool {
        self.diagnostic_uris.is_some()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenLifecycleBatchEffects {
    pub(crate) removed_clears: Vec<Url>,
    pub(crate) added_tickets: Vec<AnalysisRevalidationTicket>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AnalysisCommitEffects {
    pub(crate) revalidations: Vec<AnalysisRevalidationTicket>,
    /// Unmarked fanout discovered by dynamic closed enrichment. Normal
    /// on-demand callers ignore it; OpenInstall prerequisite convergence
    /// carries it into the one final reservation.
    pub(crate) affected_candidates: Vec<Url>,
    pub(crate) open: Option<OpenAnalysisCommitOutcome>,
    pub(crate) close: Option<OpenCloseCommitOutcome>,
    pub(crate) workspace_scan: Option<WorkspaceScanTransferredEffects>,
    pub(crate) system_file: Option<SystemFileTransferredEffects>,
    /// Unmarked outer open-transaction fanout held until exact-owner
    /// `system.file()` convergence contributes its transfer candidates.
    pub(crate) package_routing: Option<PackageRoutingCommitEffects>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnalysisCommitRejected {
    StaleBasis,
}

impl WorldState {
    /// A record can acquire at most one case-corrected and one symlink-target
    /// alias. Their order is stable: case spelling first, symlink target
    /// second, with duplicates removed while preserving that order.
    const MAX_OPEN_ALIASES_PER_RECORD: usize = 2;

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
        // Only real open documents benefit from cross-request memoization.
        // `raven check` resolves one-entry worker overlays whose URIs are not
        // stored in `self.documents`; caching those one-shot lookups would add
        // mutex contention and retain one config per checked file until exit.
        let cacheable = self.documents.contains_key(uri);
        let mut base = self.lint_config.clone();
        if let Some(unit) = self
            .per_document_indent_options
            .get(uri.as_str())
            .and_then(|options| options.indent_unit)
        {
            base.indentation_unit = unit;
        }
        if self.lint_overrides.is_empty() {
            return base;
        }
        if cacheable
            && let Ok(cache) = self.effective_lint_config_cache.lock()
            && let Some(hit) = cache.get(uri.as_str())
        {
            return hit.clone();
        }
        let resolved = crate::config_file::resolve_lint_for_document(
            &base,
            &self.merged_linting_section,
            &self.lint_overrides,
            uri,
        );
        if cacheable && let Ok(mut cache) = self.effective_lint_config_cache.lock() {
            cache.insert(uri.as_str().to_owned(), resolved.clone());
        }
        resolved
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

    fn mint_package_library_install_id() -> u64 {
        NEXT_PACKAGE_LIBRARY_INSTALL_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("package-library install identity counter exhausted")
    }

    fn mint_system_file_routing_owner_generation() -> u64 {
        NEXT_SYSTEM_FILE_ROUTING_OWNER_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("system.file routing owner generation counter exhausted")
    }

    fn mint_libpath_watcher_owner_generation() -> u64 {
        NEXT_LIBPATH_WATCHER_OWNER_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("libpath watcher owner generation counter exhausted")
    }

    pub(crate) fn libpath_watcher_owner(&self) -> LibpathWatcherOwner {
        LibpathWatcherOwner(self.libpath_watcher_owner_generation)
    }

    /// Retire the current watcher before any asynchronous teardown/spawn work.
    #[cfg(test)]
    pub(crate) fn begin_libpath_watcher_restart(&mut self) -> LibpathWatcherOwner {
        assert!(!self.routing_shutdown.load(Ordering::Acquire));
        self.libpath_watcher_owner_generation = Self::mint_libpath_watcher_owner_generation();
        assert!(
            self.libpath_watcher.retire().is_none(),
            "test-only direct retirement cannot tear down an OS watcher under WorldState"
        );
        notify_library_routing_reconcile_edge(
            &self.library_routing_reconcile_wake,
            &self.library_routing_reconcile_wake_generation,
        );
        self.libpath_watcher_owner()
    }

    pub(crate) fn libpath_watcher_owner_is_current(&self, owner: LibpathWatcherOwner) -> bool {
        !self.routing_shutdown.load(Ordering::Acquire) && self.libpath_watcher_owner() == owner
    }

    pub(crate) fn degraded_libpath_reconcile_is_current(&self, owner: LibpathWatcherOwner) -> bool {
        self.libpath_watcher_owner_is_current(owner)
            && matches!(
                self.libpath_watcher,
                LibpathWatcherState::Degraded {
                    reconcile_pending: true
                }
            )
    }

    #[cfg(test)]
    pub(crate) fn degraded_libpath_reconcile_pending_for_test(&self) -> Option<bool> {
        match self.libpath_watcher {
            LibpathWatcherState::Degraded { reconcile_pending } => Some(reconcile_pending),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn has_active_libpath_watcher_for_test(&self) -> bool {
        matches!(
            self.libpath_watcher,
            LibpathWatcherState::Active { .. } | LibpathWatcherState::ActiveUnapplied { .. }
        )
    }

    #[cfg(test)]
    pub(crate) fn install_libpath_journal_for_test(
        &mut self,
    ) -> Arc<crate::libpath_watcher::LibpathWatchJournal> {
        let journal = crate::libpath_watcher::LibpathWatchJournal::new_buffering();
        assert!(journal.try_activate());
        let _ = self.libpath_watcher.retire();
        self.libpath_watcher = LibpathWatcherState::Active {
            handle: None,
            journal: Arc::clone(&journal),
            is_recovery: false,
            applied: LibpathWatcherSpec {
                paths: self.package_library.lib_paths().to_vec(),
                debounce_ms: self.cross_file_config.packages_watch_debounce_ms,
            },
        };
        journal
    }

    pub(crate) fn capture_libpath_watcher_swap_basis(&self) -> Option<LibpathWatcherSwapBasis> {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return None;
        }
        Some(LibpathWatcherSwapBasis {
            library: Arc::clone(&self.package_library),
            install_id: self.package_library_install_id,
            content_generation: self.package_library_content_generation,
            current_owner: self.libpath_watcher_owner(),
            lifecycle: self.libpath_watcher.signature(),
            prospective_owner: LibpathWatcherOwner(Self::mint_libpath_watcher_owner_generation()),
            ready: self.package_library_ready,
            packages_enabled: self.cross_file_config.packages_enabled,
            watch_enabled: self.cross_file_config.packages_watch_library_paths,
            debounce_ms: self.cross_file_config.packages_watch_debounce_ms,
            library_paths: self.package_library.lib_paths().to_vec(),
        })
    }

    pub(crate) fn capture_libpath_watcher_recovery_basis(
        &self,
        owner: LibpathWatcherOwner,
    ) -> Option<LibpathWatcherSwapBasis> {
        if self.libpath_watcher_owner() != owner
            || !matches!(self.libpath_watcher, LibpathWatcherState::AwaitingRecovery)
        {
            return None;
        }
        let mut basis = self.capture_libpath_watcher_swap_basis()?;
        // The AwaitingRecovery owner is the never-reused token. Recovery does
        // not mint a second lineage; its CAS must consume this exact token.
        basis.prospective_owner = owner;
        Some(basis)
    }

    pub(crate) fn try_commit_libpath_watcher_swap(
        &mut self,
        basis: &LibpathWatcherSwapBasis,
        watcher: PreparedLibpathWatcherInstall,
    ) -> Result<LibpathWatcherSwapCommit, PreparedLibpathWatcherInstall> {
        let exact_recovery = basis.prospective_owner == basis.current_owner;
        let supersedes_watcher_owner = basis.prospective_owner != basis.current_owner;
        let watcher_recovery_shape_is_valid = match &watcher {
            PreparedLibpathWatcherInstall::Active { recovery, .. }
            | PreparedLibpathWatcherInstall::AttachFailed { recovery, .. } => {
                *recovery == exact_recovery
            }
            PreparedLibpathWatcherInstall::Disabled => true,
            PreparedLibpathWatcherInstall::Keep => false,
        };
        if self.routing_shutdown.load(Ordering::Acquire)
            || !Arc::ptr_eq(&self.package_library, &basis.library)
            || self.package_library_install_id != basis.install_id
            || self.package_library_content_generation != basis.content_generation
            || self.libpath_watcher_owner() != basis.current_owner
            || self.libpath_watcher.signature() != basis.lifecycle
            || self.package_library_ready != basis.ready
            || self.cross_file_config.packages_enabled != basis.packages_enabled
            || self.cross_file_config.packages_watch_library_paths != basis.watch_enabled
            || self.cross_file_config.packages_watch_debounce_ms != basis.debounce_ms
            || self.package_library.lib_paths() != basis.library_paths
            || (exact_recovery
                && !matches!(self.libpath_watcher, LibpathWatcherState::AwaitingRecovery))
            || !watcher_recovery_shape_is_valid
            || !watcher.is_buffering_active()
        {
            return Err(watcher);
        }
        let (retired_handle, recovery_owner, degraded_reconcile_owner) = match watcher {
            PreparedLibpathWatcherInstall::Active {
                handle,
                journal,
                recovery,
            } => {
                let retired = self.libpath_watcher.retire();
                assert!(
                    journal.try_activate(),
                    "watcher-only CAS must activate an exact buffering journal"
                );
                self.libpath_watcher_owner_generation = basis.prospective_owner.0;
                self.libpath_watcher = LibpathWatcherState::Active {
                    handle: Some(handle),
                    journal,
                    is_recovery: recovery,
                    applied: basis.desired_spec(),
                };
                (retired, None, None)
            }
            PreparedLibpathWatcherInstall::Disabled => {
                let retired = self.libpath_watcher.retire();
                self.libpath_watcher_owner_generation = basis.prospective_owner.0;
                (retired, None, None)
            }
            PreparedLibpathWatcherInstall::AttachFailed {
                recovery,
                can_recover,
            } => {
                if recovery {
                    let retired = self.libpath_watcher.retire();
                    self.libpath_watcher_owner_generation = basis.prospective_owner.0;
                    self.libpath_watcher = LibpathWatcherState::Degraded {
                        reconcile_pending: true,
                    };
                    (retired, None, Some(basis.prospective_owner))
                } else if matches!(
                    &self.libpath_watcher,
                    LibpathWatcherState::Active { applied, .. }
                        | LibpathWatcherState::ActiveUnapplied { applied, .. }
                        if applied.paths == basis.library_paths
                ) {
                    let old =
                        std::mem::replace(&mut self.libpath_watcher, LibpathWatcherState::Disabled);
                    self.libpath_watcher = match old {
                        LibpathWatcherState::Active {
                            handle,
                            journal,
                            is_recovery,
                            applied,
                        }
                        | LibpathWatcherState::ActiveUnapplied {
                            handle,
                            journal,
                            is_recovery,
                            applied,
                            ..
                        } => LibpathWatcherState::ActiveUnapplied {
                            handle,
                            journal,
                            is_recovery,
                            applied,
                            desired: basis.desired_spec(),
                        },
                        _ => unreachable!("the active watcher shape was prechecked"),
                    };
                    // Failed same-coverage settings application retains the
                    // exact applied owner and callback lifecycle.
                    (None, None, None)
                } else if can_recover {
                    let retired = self.libpath_watcher.retire();
                    self.libpath_watcher_owner_generation = basis.prospective_owner.0;
                    self.libpath_watcher = LibpathWatcherState::AwaitingRecovery;
                    (retired, Some(basis.prospective_owner), None)
                } else {
                    let retired = self.libpath_watcher.retire();
                    self.libpath_watcher_owner_generation = basis.prospective_owner.0;
                    self.libpath_watcher = LibpathWatcherState::Degraded {
                        reconcile_pending: true,
                    };
                    (retired, None, Some(basis.prospective_owner))
                }
            }
            rejected @ PreparedLibpathWatcherInstall::Keep => return Err(rejected),
        };
        if supersedes_watcher_owner {
            // A terminal degraded worker can be parked on the shared wake.
            // Publishing the new watcher owner is therefore itself a wake
            // edge: the old root must observe supersession promptly instead
            // of lingering until its heartbeat.
            notify_library_routing_reconcile_edge(
                &self.library_routing_reconcile_wake,
                &self.library_routing_reconcile_wake_generation,
            );
        }
        Ok(LibpathWatcherSwapCommit {
            retired_handle,
            recovery_owner,
            degraded_reconcile_owner,
        })
    }

    /// Install a newly built package library and mint every authority identity
    /// consumed by detached package and `system.file()` work.
    #[cfg(test)]
    pub(crate) fn install_package_library(
        &mut self,
        library: Arc<PackageLibrary>,
        ready: bool,
    ) -> LibpathWatcherOwner {
        self.package_library = library;
        self.package_library_install_id = Self::mint_package_library_install_id();
        self.package_library_content_generation = 0;
        self.system_file_routing_owner_generation =
            Self::mint_system_file_routing_owner_generation();
        self.refresh_local_dev_overlay();
        self.package_library_ready = ready;
        self.bump_package_config_generation();
        self.begin_libpath_watcher_restart()
    }

    /// Record an in-place package-library content change after cache
    /// invalidation and warmup have completed.
    #[cfg(test)]
    pub(crate) fn record_package_library_content_change(&mut self) {
        self.package_library_content_generation = self
            .package_library_content_generation
            .checked_add(1)
            .expect("package-library content generation exhausted");
        self.system_file_routing_owner_generation =
            Self::mint_system_file_routing_owner_generation();
    }

    /// Mint routing ownership even when the installed routing values compare
    /// equal, as happens when a package seed is replayed.
    pub(crate) fn record_system_file_routing_owner_change(&mut self) {
        self.system_file_routing_owner_generation =
            Self::mint_system_file_routing_owner_generation();
    }

    #[cfg(test)]
    pub(crate) fn system_file_routing_owner_generation(&self) -> u64 {
        self.system_file_routing_owner_generation
    }

    #[cfg(test)]
    pub(crate) fn package_state_record_generation_for_test(&self) -> u64 {
        self.package_state_record_generation
    }

    #[cfg(test)]
    pub(crate) fn package_library_content_generation_for_test(&self) -> u64 {
        self.package_library_content_generation
    }

    #[cfg(test)]
    pub(crate) fn package_library_install_id_for_test(&self) -> u64 {
        self.package_library_install_id
    }

    pub(crate) fn system_file_routing_owner_identity(&self) -> SystemFileRoutingOwnerIdentity {
        SystemFileRoutingOwnerIdentity(self.system_file_routing_owner_generation)
    }

    pub(crate) fn record_package_seed_installed(&mut self) -> PackageSeedInstalledIdentity {
        self.package_seed_install_id = NEXT_PACKAGE_SEED_INSTALL_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("package-seed install identity counter exhausted");
        PackageSeedInstalledIdentity {
            seed_install_id: self.package_seed_install_id,
            package_config_generation: self.package_config_generation,
            package_input_generation: self.package_input_generation(),
            package_state_record_generation: self.package_state_record_generation,
            system_file_routing_owner: self.system_file_routing_owner_identity(),
            package_library_install_id: self.package_library_install_id,
            package_library_content_generation: self.package_library_content_generation,
        }
    }

    fn system_file_routing_stamp(&self) -> SystemFileRoutingStamp {
        let (workspace_name, workspace_root, library_paths) = self.snapshot_system_file_inputs();
        SystemFileRoutingStamp {
            owner: self.system_file_routing_owner_identity(),
            package_state_record_generation: self.package_state_record_generation,
            package_library_install_id: self.package_library_install_id,
            package_library_content_generation: self.package_library_content_generation,
            workspace_name,
            workspace_root,
            library_paths,
        }
    }

    pub(crate) fn capture_library_routing_basis(
        &mut self,
        expected_library: &Arc<PackageLibrary>,
        cache_operation_epoch: u64,
        mutation: LibraryRoutingMutation,
        watcher_owner: Option<LibpathWatcherOwner>,
    ) -> Option<LibraryRoutingBasis> {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return None;
        }
        if !Arc::ptr_eq(&self.package_library, expected_library)
            || watcher_owner.is_some_and(|owner| !self.libpath_watcher_owner_is_current(owner))
        {
            return None;
        }
        let replacement_intent = match mutation {
            LibraryRoutingMutation::Replacement => {
                let generation = NEXT_LIBRARY_REPLACEMENT_INTENT_GENERATION
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                        current.checked_add(1)
                    })
                    .expect("library-replacement intent generation counter exhausted");
                let intent = LibraryReplacementIntent(generation);
                let mut lifecycle = self.library_replacement_lifecycle.lock();
                let adopted_reconcile_obligation = lifecycle.reconcile_required.is_some();
                lifecycle.pending = Some(intent);
                // The current exact capture adopts any previously deposited
                // reconcile obligation. If this owner is cancelled, its guard
                // deposits a fresh never-reused request.
                lifecycle.reconcile_required = None;
                drop(lifecycle);
                // Stored below after the match so additive captures remain
                // explicitly obligation-free.
                Some((intent, adopted_reconcile_obligation))
            }
            LibraryRoutingMutation::Changed
            | LibraryRoutingMutation::FullRescan
            | LibraryRoutingMutation::DegradedReconcile
            | LibraryRoutingMutation::Dropped => None,
        };
        let (replacement_intent, adopted_reconcile_obligation) = replacement_intent
            .map(|(intent, adopted)| (Some(intent), adopted))
            .unwrap_or((None, false));
        Some(LibraryRoutingBasis {
            library: Arc::clone(&self.package_library),
            cache_operation_epoch,
            routing: self.system_file_routing_stamp(),
            ready: self.package_library_ready,
            package_input_generation: self.package_input_generation(),
            package_config_generation: self.package_config_generation,
            package_state_record_generation: self.package_state_record_generation,
            packages_enabled: self.cross_file_config.packages_enabled,
            packages_r_path: self.cross_file_config.packages_r_path.clone(),
            packages_additional_library_paths: self
                .cross_file_config
                .packages_additional_library_paths
                .clone(),
            packages_watch_library_paths: self.cross_file_config.packages_watch_library_paths,
            packages_watch_debounce_ms: self.cross_file_config.packages_watch_debounce_ms,
            workspace_folders: self.workspace_folders.clone(),
            watcher_owner,
            replacement_intent,
            adopted_reconcile_obligation,
            mutation,
        })
    }

    fn library_replacement_basis_is_current(&self, basis: &LibraryRoutingBasis) -> bool {
        match basis.mutation {
            LibraryRoutingMutation::Replacement => {
                self.library_replacement_lifecycle.lock().pending == basis.replacement_intent
            }
            LibraryRoutingMutation::Changed
            | LibraryRoutingMutation::FullRescan
            | LibraryRoutingMutation::DegradedReconcile
            | LibraryRoutingMutation::Dropped => basis.replacement_intent.is_none(),
        }
    }

    fn library_watch_settings_are_current(&self, basis: &LibraryRoutingBasis) -> bool {
        self.cross_file_config.packages_watch_library_paths == basis.packages_watch_library_paths
            && self.cross_file_config.packages_watch_debounce_ms == basis.packages_watch_debounce_ms
    }

    /// Retire an unpublished replacement only when it still owns the pending
    /// slot. A newer replacement keeps its own intent and inherited/current
    /// open-document convergence responsibility.
    #[cfg(test)]
    pub(crate) fn abort_library_replacement(&mut self, basis: &LibraryRoutingBasis) {
        let mut lifecycle = self.library_replacement_lifecycle.lock();
        if basis.mutation == LibraryRoutingMutation::Replacement
            && lifecycle.pending == basis.replacement_intent
        {
            lifecycle.pending = None;
        }
    }

    /// Arm synchronous cancellation ownership while the caller still holds the
    /// state lock used to capture `basis`; this leaves no await/cancellation gap
    /// between intent publication and guard ownership.
    pub(crate) fn guard_library_replacement(
        &self,
        basis: &LibraryRoutingBasis,
        abort_policy: LibraryReplacementAbortPolicy,
    ) -> Option<PendingLibraryReplacementGuard> {
        let intent = basis.replacement_intent?;
        let lifecycle = self.library_replacement_lifecycle.lock();
        if lifecycle.pending != Some(intent) {
            return None;
        }
        drop(lifecycle);
        Some(PendingLibraryReplacementGuard {
            lifecycle: Arc::clone(&self.library_replacement_lifecycle),
            routing_shutdown: Arc::clone(&self.routing_shutdown),
            reconcile_wake: Arc::clone(&self.library_routing_reconcile_wake),
            reconcile_wake_generation: Arc::clone(&self.library_routing_reconcile_wake_generation),
            intent,
            telemetry: LibraryRoutingReconcileTelemetry {
                package_config_generation: basis.package_config_generation,
                package_input_generation: basis.package_input_generation,
                packages_enabled: basis.packages_enabled,
                packages_r_path: basis.packages_r_path.clone(),
                packages_additional_library_paths: basis.packages_additional_library_paths.clone(),
                workspace_folders: basis.workspace_folders.clone(),
            },
            abort_policy,
            armed: true,
        })
    }

    pub(crate) fn adopt_library_routing_pre_seal(
        &self,
        basis: &LibraryRoutingBasis,
    ) -> (
        LibraryRoutingPreSealOwner,
        LibraryRoutingPreSealDeposit,
        bool,
    ) {
        let mut lifecycle = self.library_replacement_lifecycle.lock();
        let adopted_existing = lifecycle.pre_seal.is_some();
        let mut deposit = lifecycle
            .pre_seal
            .take()
            .unwrap_or_else(|| LibraryRoutingPreSealDeposit::from_basis(basis));
        deposit.replacement_intent = basis.replacement_intent;
        deposit.telemetry = LibraryRoutingPreSealDeposit::from_basis(basis).telemetry;
        (
            LibraryRoutingPreSealOwner {
                lifecycle: Arc::clone(&self.library_replacement_lifecycle),
                routing_shutdown: Arc::clone(&self.routing_shutdown),
                reconcile_wake: Arc::clone(&self.library_routing_reconcile_wake),
                reconcile_wake_generation: Arc::clone(
                    &self.library_routing_reconcile_wake_generation,
                ),
            },
            deposit,
            adopted_existing || basis.adopted_reconcile_obligation,
        )
    }

    pub(crate) fn capture_current_library_routing_pre_seal(
        &self,
    ) -> (LibraryRoutingPreSealOwner, LibraryRoutingPreSealDeposit) {
        let telemetry = LibraryRoutingReconcileTelemetry {
            package_config_generation: self.package_config_generation,
            package_input_generation: self.package_input_generation(),
            packages_enabled: self.cross_file_config.packages_enabled,
            packages_r_path: self.cross_file_config.packages_r_path.clone(),
            packages_additional_library_paths: self
                .cross_file_config
                .packages_additional_library_paths
                .clone(),
            workspace_folders: self.workspace_folders.clone(),
        };
        let mut lifecycle = self.library_replacement_lifecycle.lock();
        let deposit = lifecycle
            .pre_seal
            .take()
            .unwrap_or(LibraryRoutingPreSealDeposit {
                id: LibraryRoutingPreSealDeposit::mint_id(),
                replacement_intent: lifecycle.pending,
                telemetry,
                handles: Vec::new(),
                candidates: Vec::new(),
                fallback: Vec::new(),
                post_seed: None,
                retired_post_seed_owners: Vec::new(),
                build_notes: Vec::new(),
            });
        (
            LibraryRoutingPreSealOwner {
                lifecycle: Arc::clone(&self.library_replacement_lifecycle),
                routing_shutdown: Arc::clone(&self.routing_shutdown),
                reconcile_wake: Arc::clone(&self.library_routing_reconcile_wake),
                reconcile_wake_generation: Arc::clone(
                    &self.library_routing_reconcile_wake_generation,
                ),
            },
            deposit,
        )
    }

    pub(crate) fn claim_library_routing_reconcile_request(
        &self,
    ) -> Option<LibraryRoutingReconcileClaim> {
        let request = self
            .library_replacement_lifecycle
            .lock()
            .reconcile_required
            .take()?;
        Some(LibraryRoutingReconcileClaim {
            lifecycle: Arc::clone(&self.library_replacement_lifecycle),
            routing_shutdown: Arc::clone(&self.routing_shutdown),
            reconcile_wake: Arc::clone(&self.library_routing_reconcile_wake),
            reconcile_wake_generation: Arc::clone(&self.library_routing_reconcile_wake_generation),
            request: Some(request),
        })
    }

    /// Capture a synchronous wake owner for additive Changed/Dropped work.
    pub(crate) fn library_routing_reconcile_owner_current(&self) -> LibraryRoutingReconcileOwner {
        LibraryRoutingReconcileOwner {
            lifecycle: Arc::clone(&self.library_replacement_lifecycle),
            routing_shutdown: Arc::clone(&self.routing_shutdown),
            reconcile_wake: Arc::clone(&self.library_routing_reconcile_wake),
            reconcile_wake_generation: Arc::clone(&self.library_routing_reconcile_wake_generation),
            telemetry: LibraryRoutingReconcileTelemetry {
                package_config_generation: self.package_config_generation,
                package_input_generation: self.package_input_generation(),
                packages_enabled: self.cross_file_config.packages_enabled,
                packages_r_path: self.cross_file_config.packages_r_path.clone(),
                packages_additional_library_paths: self
                    .cross_file_config
                    .packages_additional_library_paths
                    .clone(),
                workspace_folders: self.workspace_folders.clone(),
            },
        }
    }

    pub(crate) fn request_library_routing_reconcile_current(&self) {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        let id = NEXT_LIBRARY_ROUTING_RECONCILE_REQUEST_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("library-routing reconcile request identity exhausted");
        let mut lifecycle = self.library_replacement_lifecycle.lock();
        if self.routing_shutdown.load(Ordering::Acquire) {
            return;
        }
        lifecycle.reconcile_required = Some(LibraryRoutingReconcileRequest {
            id,
            telemetry: LibraryRoutingReconcileTelemetry {
                package_config_generation: self.package_config_generation,
                package_input_generation: self.package_input_generation(),
                packages_enabled: self.cross_file_config.packages_enabled,
                packages_r_path: self.cross_file_config.packages_r_path.clone(),
                packages_additional_library_paths: self
                    .cross_file_config
                    .packages_additional_library_paths
                    .clone(),
                workspace_folders: self.workspace_folders.clone(),
            },
        });
        drop(lifecycle);
        notify_library_routing_reconcile_edge(
            &self.library_routing_reconcile_wake,
            &self.library_routing_reconcile_wake_generation,
        );
    }

    /// Durable reconcile request currently available to the pickup
    /// coordinator. This is a peek: the request remains resident until an
    /// initialization attempt claims it.
    pub(crate) fn library_routing_reconcile_request(
        &self,
    ) -> Option<LibraryRoutingReconcileRequest> {
        self.library_replacement_lifecycle
            .lock()
            .reconcile_required
            .clone()
    }

    pub(crate) fn library_routing_reconcile_wake(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.library_routing_reconcile_wake)
    }

    pub(crate) fn library_routing_reconcile_wake_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.library_routing_reconcile_wake_generation)
    }

    pub(crate) fn library_routing_reconcile_eligibility_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.library_routing_reconcile_eligibility_generation)
    }

    pub(crate) fn routing_is_shutdown(&self) -> bool {
        self.routing_shutdown.load(Ordering::Acquire)
    }

    /// Wake the resident reconcile coordinator after an external eligibility
    /// change, such as packages being re-enabled.
    pub(crate) fn notify_library_routing_reconcile(&self) {
        self.library_routing_reconcile_eligibility_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .expect("library-routing eligibility generation exhausted");
        notify_library_routing_reconcile_edge(
            &self.library_routing_reconcile_wake,
            &self.library_routing_reconcile_wake_generation,
        );
    }

    #[cfg(test)]
    pub(crate) fn library_routing_reconcile_request_for_test(
        &self,
    ) -> Option<LibraryRoutingReconcileRequest> {
        self.library_replacement_lifecycle
            .lock()
            .reconcile_required
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn library_routing_pre_seal_is_empty_for_test(&self) -> bool {
        self.library_replacement_lifecycle.lock().pre_seal.is_none()
    }

    #[cfg(test)]
    pub(crate) fn library_replacement_basis_is_current_for_test(
        &self,
        basis: &LibraryRoutingBasis,
    ) -> bool {
        self.library_replacement_basis_is_current(basis)
    }

    /// Rebase the same pending replacement intent onto an additive
    /// package-library winner. Configuration/package-input changes are not an
    /// additive lineage and therefore reject instead of silently adopting a
    /// different replacement request.
    pub(crate) fn rebase_library_replacement_basis(
        &self,
        basis: &LibraryRoutingBasis,
        expected_library: &Arc<PackageLibrary>,
        cache_operation_epoch: u64,
    ) -> Option<LibraryRoutingBasis> {
        if basis.mutation != LibraryRoutingMutation::Replacement
            || self.library_replacement_lifecycle.lock().pending != basis.replacement_intent
            || !self.library_watch_settings_are_current(basis)
            || !Arc::ptr_eq(&self.package_library, expected_library)
            || self.package_input_generation() != basis.package_input_generation
            || self.package_config_generation != basis.package_config_generation
            || self.package_state_record_generation != basis.package_state_record_generation
            || self.package_library_ready != basis.ready
            || self.cross_file_config.packages_enabled != basis.packages_enabled
            || self.cross_file_config.packages_r_path != basis.packages_r_path
            || self.cross_file_config.packages_additional_library_paths
                != basis.packages_additional_library_paths
            || self.workspace_folders != basis.workspace_folders
        {
            return None;
        }
        let mut rebased = basis.clone();
        rebased.library = Arc::clone(&self.package_library);
        rebased.cache_operation_epoch = cache_operation_epoch;
        rebased.routing = self.system_file_routing_stamp();
        Some(rebased)
    }

    pub(crate) fn library_routing_basis_is_current(
        &self,
        basis: &LibraryRoutingBasis,
        lease: &PackageLibraryRoutingLease<'_>,
    ) -> bool {
        Arc::ptr_eq(&self.package_library, &basis.library)
            && self.library_replacement_basis_is_current(basis)
            && self.library_watch_settings_are_current(basis)
            && basis.library.cache_operation_epoch(lease) == basis.cache_operation_epoch
            && self.system_file_routing_stamp() == basis.routing
            && self.package_library_ready == basis.ready
            && self.package_input_generation() == basis.package_input_generation
            && self.package_config_generation == basis.package_config_generation
            && self.package_state_record_generation == basis.package_state_record_generation
            && self.cross_file_config.packages_enabled == basis.packages_enabled
            && self.cross_file_config.packages_r_path == basis.packages_r_path
            && self.cross_file_config.packages_additional_library_paths
                == basis.packages_additional_library_paths
            && self.workspace_folders == basis.workspace_folders
            && basis
                .watcher_owner
                .is_none_or(|owner| self.libpath_watcher_owner_is_current(owner))
    }

    pub(crate) fn refresh_library_routing_cache_epoch(
        &self,
        basis: &LibraryRoutingBasis,
        cache_operation_epoch: u64,
    ) -> Option<LibraryRoutingBasis> {
        if !Arc::ptr_eq(&self.package_library, &basis.library)
            || !self.library_replacement_basis_is_current(basis)
            || !self.library_watch_settings_are_current(basis)
            || self.system_file_routing_stamp() != basis.routing
            || self.package_library_ready != basis.ready
            || self.package_input_generation() != basis.package_input_generation
            || self.package_config_generation != basis.package_config_generation
            || self.package_state_record_generation != basis.package_state_record_generation
            || self.cross_file_config.packages_enabled != basis.packages_enabled
            || self.cross_file_config.packages_r_path != basis.packages_r_path
            || self.cross_file_config.packages_additional_library_paths
                != basis.packages_additional_library_paths
            || self.workspace_folders != basis.workspace_folders
            || basis
                .watcher_owner
                .is_some_and(|owner| !self.libpath_watcher_owner_is_current(owner))
        {
            return None;
        }
        let mut refreshed = basis.clone();
        refreshed.cache_operation_epoch = cache_operation_epoch;
        Some(refreshed)
    }

    pub(crate) fn prospective_library_routing(
        &self,
        basis: &LibraryRoutingBasis,
        library: &PackageLibrary,
    ) -> Option<ProspectiveLibraryRouting> {
        if !Arc::ptr_eq(&self.package_library, &basis.library)
            || !self.library_replacement_basis_is_current(basis)
            || !self.library_watch_settings_are_current(basis)
            || self.system_file_routing_stamp() != basis.routing
            || self.package_input_generation() != basis.package_input_generation
            || self.package_config_generation != basis.package_config_generation
            || self.package_state_record_generation != basis.package_state_record_generation
            || self.cross_file_config.packages_enabled != basis.packages_enabled
            || self.cross_file_config.packages_r_path != basis.packages_r_path
            || self.cross_file_config.packages_additional_library_paths
                != basis.packages_additional_library_paths
            || self.workspace_folders != basis.workspace_folders
            || basis
                .watcher_owner
                .is_some_and(|owner| !self.libpath_watcher_owner_is_current(owner))
        {
            return None;
        }
        let install_id = match basis.mutation {
            LibraryRoutingMutation::Replacement => Self::mint_package_library_install_id(),
            LibraryRoutingMutation::Changed
            | LibraryRoutingMutation::FullRescan
            | LibraryRoutingMutation::DegradedReconcile
            | LibraryRoutingMutation::Dropped => basis.routing.package_library_install_id,
        };
        let content_generation = match basis.mutation {
            LibraryRoutingMutation::Replacement => 0,
            LibraryRoutingMutation::Changed
            | LibraryRoutingMutation::FullRescan
            | LibraryRoutingMutation::DegradedReconcile
            | LibraryRoutingMutation::Dropped => basis
                .routing
                .package_library_content_generation
                .checked_add(1)
                .expect("package-library content generation exhausted"),
        };
        let routing_owner =
            SystemFileRoutingOwnerIdentity(Self::mint_system_file_routing_owner_generation());
        let watcher_owner = match basis.mutation {
            LibraryRoutingMutation::Changed
            | LibraryRoutingMutation::FullRescan
            | LibraryRoutingMutation::DegradedReconcile => basis
                .watcher_owner
                .expect("watcher-owner-preserving routing must retain its provenance"),
            LibraryRoutingMutation::Replacement | LibraryRoutingMutation::Dropped => {
                LibpathWatcherOwner(Self::mint_libpath_watcher_owner_generation())
            }
        };
        let mut routing = basis.routing.clone();
        routing.owner = routing_owner;
        routing.package_library_install_id = install_id;
        routing.package_library_content_generation = content_generation;
        routing.library_paths = library.lib_paths().to_vec();
        Some(ProspectiveLibraryRouting {
            install_id,
            content_generation,
            routing_owner,
            watcher_owner,
            routing,
        })
    }

    /// Capture the compact authority half of open-package warming. The caller
    /// takes the scope snapshot under the same state read lock, then performs
    /// collection and R/provider work after releasing it.
    pub(crate) fn capture_open_package_warm_basis(
        &self,
        candidate_library: &Arc<PackageLibrary>,
    ) -> OpenPackageWarmBasis {
        let index = self.workspace_index.authority_snapshot();
        OpenPackageWarmBasis {
            candidate_library: Arc::clone(candidate_library),
            workspace_index_version: index.version,
            workspace_index_max_files: self.workspace_index.config().max_files,
            workspace_index_max_file_size_bytes: self.workspace_index.config().max_file_size_bytes,
            workspace_index_artifact_capacity: index.artifact_capacity_limit,
            workspace_index_pinned: index.pinned,
            graph_revision: self.cross_file_graph.edge_revision(),
            graph_authority_generation: self.workspace_graph_authority_generation,
            open_context_authority_generation: self.open_context_authority_generation,
            editor_eligibility_generation: self.editor_eligibility_generation,
            analysis_config_generation: self.analysis_config_generation,
            chunk_override_generation: self.chunk_override_generation,
            raw_cache_generation: self.cross_file_file_cache.content_generation(),
            package_input_generation: self.package_input_generation(),
            package_config_generation: self.package_config_generation,
            package_state_record_generation: self.package_state_record_generation,
            workspace_folders: self.workspace_folders.clone(),
            exclusion_patterns: self.workspace_exclusions.patterns().to_vec(),
            max_chain_depth: self.cross_file_config.max_chain_depth,
            max_transitive_dependents_visited: self
                .cross_file_config
                .max_transitive_dependents_visited,
            backward_dependencies: self.cross_file_config.backward_dependencies,
            open_records: self
                .documents
                .keys()
                .map(|uri| (uri.clone(), self.documents.record_token(uri)))
                .collect(),
            requested_packages: HashSet::new(),
            successfully_warmed: HashSet::new(),
        }
    }

    fn open_package_warm_basis_is_current(
        &self,
        basis: &OpenPackageWarmBasis,
        candidate_library: &Arc<PackageLibrary>,
    ) -> bool {
        let index = self.workspace_index.authority_snapshot();
        let current_open: std::collections::BTreeMap<_, _> = self
            .documents
            .keys()
            .map(|uri| (uri.clone(), self.documents.record_token(uri)))
            .collect();
        Arc::ptr_eq(&basis.candidate_library, candidate_library)
            && index.version == basis.workspace_index_version
            && self.workspace_index.config().max_files == basis.workspace_index_max_files
            && self.workspace_index.config().max_file_size_bytes
                == basis.workspace_index_max_file_size_bytes
            && index.artifact_capacity_limit == basis.workspace_index_artifact_capacity
            && index.pinned == basis.workspace_index_pinned
            && self.cross_file_graph.edge_revision() == basis.graph_revision
            && self.workspace_graph_authority_generation == basis.graph_authority_generation
            && self.open_context_authority_generation == basis.open_context_authority_generation
            && self.editor_eligibility_generation == basis.editor_eligibility_generation
            && self.analysis_config_generation == basis.analysis_config_generation
            && self.chunk_override_generation == basis.chunk_override_generation
            && self.cross_file_file_cache.content_generation() == basis.raw_cache_generation
            && self.package_input_generation() == basis.package_input_generation
            && self.package_config_generation == basis.package_config_generation
            && self.package_state_record_generation == basis.package_state_record_generation
            && self.workspace_folders == basis.workspace_folders
            && self.workspace_exclusions.patterns() == basis.exclusion_patterns.as_slice()
            && self.cross_file_config.max_chain_depth == basis.max_chain_depth
            && self.cross_file_config.max_transitive_dependents_visited
                == basis.max_transitive_dependents_visited
            && self.cross_file_config.backward_dependencies == basis.backward_dependencies
            && current_open == basis.open_records
            && basis
                .requested_packages
                .is_subset(&basis.successfully_warmed)
    }

    pub(crate) fn workspace_scan_generation(&self) -> u64 {
        self.workspace_scan_generation
    }

    fn mint_workspace_scan_intent_generation() -> u64 {
        NEXT_WORKSPACE_SCAN_INTENT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("workspace-scan intent generation counter exhausted")
    }

    fn mint_workspace_scan_commit_generation() -> u64 {
        NEXT_WORKSPACE_SCAN_COMMIT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("workspace-scan commit generation counter exhausted")
    }

    fn mint_system_file_commit_generation() -> u64 {
        NEXT_SYSTEM_FILE_COMMIT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("system.file commit generation counter exhausted")
    }

    pub(crate) fn begin_analysis_transfer_finalization() -> AnalysisTransferFinalizationId {
        AnalysisTransferFinalizationId(
            NEXT_ANALYSIS_TRANSFER_FINALIZATION_ID
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    current.checked_add(1)
                })
                .expect("analysis-transfer finalization identity counter exhausted"),
        )
    }

    fn install_analysis_transfer(
        &mut self,
        identity: AnalysisTransferIdentity,
        previous: Option<AnalysisTransferIdentity>,
        mut candidates: Vec<AnalysisTransferCandidate>,
    ) -> AnalysisTransferHandle {
        let mut routing_tail = None;
        if let Some(previous) = previous
            && let Some(inherited) = self.analysis_transfers.remove(&previous)
        {
            candidates.extend(inherited.candidates);
            routing_tail = inherited.routing_tail;
            self.analysis_transfer_successors.insert(previous, identity);
        }
        candidates.sort_unstable_by(|left, right| left.uri.as_str().cmp(right.uri.as_str()));
        candidates.dedup();
        self.analysis_transfers.insert(
            identity,
            AnalysisTransferState {
                candidates,
                routing_tail,
            },
        );
        AnalysisTransferHandle { identity }
    }

    /// Collapse deposited pre-seal ledgers and the current routing capture into
    /// one pending transfer. The ordinary outer finalization remains the sole
    /// place that filters exact tokens, resolves per-URI reservation precedence,
    /// caps, and seals diagnostic work.
    fn install_library_routing_transfer(
        &mut self,
        identity: AnalysisTransferIdentity,
        previous: Option<AnalysisTransferIdentity>,
        current: Vec<AnalysisTransferCandidate>,
        mut deposit: LibraryRoutingPreSealDeposit,
    ) -> AnalysisTransferHandle {
        let mut inherited = Vec::new();
        let mut routing_tail = LibraryRoutingTail::from_deposit(&mut deposit);
        let mut consumed_excluded = HashSet::new();
        let mut missing = false;
        let mut terminals = Vec::new();
        if let Some(previous) = previous {
            terminals.push(previous);
        }
        terminals.extend(deposit.handles.iter().map(|handle| handle.identity));

        let mut unique_terminals = HashSet::new();
        for origin in terminals {
            let mut terminal = origin;
            let mut visited = HashSet::new();
            let mut cycle = false;
            while let Some(successor) = self.analysis_transfer_successors.get(&terminal).copied() {
                if !visited.insert(terminal) {
                    log::error!(
                        "analysis-transfer successor cycle detected during routing collapse"
                    );
                    debug_assert!(false, "analysis-transfer successor chain must be acyclic");
                    missing = true;
                    cycle = true;
                    break;
                }
                terminal = successor;
            }
            if cycle {
                continue;
            }
            if terminal == identity || !unique_terminals.insert(terminal) {
                continue;
            }
            if self.analysis_transfers_consumed.contains_key(&terminal) {
                consumed_excluded.extend(self.current_consumed_analysis_transfer_candidate_uris(
                    AnalysisTransferHandle { identity: terminal },
                ));
                continue;
            }
            if let Some(state) = self.analysis_transfers.remove(&terminal) {
                inherited.extend(state.candidates);
                if let Some(incoming_tail) = state.routing_tail {
                    if let Some(existing) = routing_tail.as_mut() {
                        existing.merge(incoming_tail);
                    } else {
                        routing_tail = Some(incoming_tail);
                    }
                }
                self.analysis_transfer_successors.insert(terminal, identity);
            } else {
                missing = true;
            }
        }
        if missing {
            inherited.extend(self.capture_analysis_transfer_candidates(deposit.fallback));
        }
        inherited.retain(|candidate| !consumed_excluded.contains(&candidate.uri));

        let mut merged = current;
        merged.extend(deposit.candidates);
        merged.extend(inherited);
        self.analysis_transfers.insert(
            identity,
            AnalysisTransferState {
                candidates: merged,
                routing_tail,
            },
        );
        AnalysisTransferHandle { identity }
    }

    /// Move a routing tail from the pending transfer into durable state-owned
    /// registries without sealing its diagnostic handle. A post-seed
    /// coordinator later consumes the retained handle after package state is
    /// current, preserving the single outer publication cap.
    pub(crate) fn claim_library_routing_tail(
        &mut self,
        handle: AnalysisTransferHandle,
    ) -> LibraryRoutingTailClaim {
        let mut identity = handle.identity;
        let mut visited = HashSet::new();
        while let Some(successor) = self.analysis_transfer_successors.get(&identity).copied() {
            if !visited.insert(identity) {
                log::error!("analysis-transfer successor cycle while claiming routing tail");
                debug_assert!(false, "analysis-transfer successor chain must be acyclic");
                return LibraryRoutingTailClaim::Blocked;
            }
            identity = successor;
        }
        if self.routing_shutdown.load(Ordering::Acquire) {
            if let Some(mut transfer) = self.analysis_transfers.remove(&identity) {
                if let Some(mut tail) = transfer.routing_tail.take() {
                    let mut retired = tail.retired_post_seed_owners;
                    if let Some(post_seed) = tail.post_seed.take() {
                        retired.push(post_seed.identity);
                        retired.extend(post_seed.deferred_system_file);
                    }
                    for owner in retired {
                        self.complete_post_seed_refresh_retry(owner);
                        self.complete_system_file_seed_retry(owner);
                    }
                }
                self.analysis_transfers_consumed
                    .insert(identity, Vec::new());
            }
            return LibraryRoutingTailClaim::Shutdown;
        }
        let Some(mut tail) = self
            .analysis_transfers
            .get_mut(&identity)
            .and_then(|state| state.routing_tail.take())
        else {
            return LibraryRoutingTailClaim::None;
        };

        if let Some(post_seed) = tail.post_seed.take() {
            let registration = self.begin_post_seed_refresh_retry_with_system_dependency(
                post_seed.identity,
                post_seed.deferred_system_file.is_some(),
            );
            if matches!(
                registration,
                PostSeedRefreshOwnerRegistration::ExistingDifferent(_)
            ) {
                tail.post_seed = Some(post_seed);
                self.analysis_transfers
                    .get_mut(&identity)
                    .expect("routing-tail transfer remained pending")
                    .routing_tail = Some(tail);
                self.request_library_routing_reconcile_current();
                return LibraryRoutingTailClaim::Blocked;
            }
            let retained = self.retain_post_seed_outer_finalization(
                post_seed.identity,
                [AnalysisTransferHandle { identity }],
                [],
            );
            debug_assert!(retained);
            for retired in tail.retired_post_seed_owners {
                self.complete_post_seed_refresh_retry(retired);
                self.complete_system_file_seed_retry(retired);
            }
            self.deferred_library_routing_build_notes
                .append(&mut tail.build_notes);
            return match registration {
                PostSeedRefreshOwnerRegistration::Added => {
                    LibraryRoutingTailClaim::PostSeedAdded(post_seed)
                }
                PostSeedRefreshOwnerRegistration::ExistingSame => {
                    LibraryRoutingTailClaim::PostSeedExisting
                }
                PostSeedRefreshOwnerRegistration::ExistingDifferent(_) => unreachable!(),
                PostSeedRefreshOwnerRegistration::Shutdown => LibraryRoutingTailClaim::Shutdown,
            };
        }

        for retired in tail.retired_post_seed_owners {
            self.complete_post_seed_refresh_retry(retired);
            self.complete_system_file_seed_retry(retired);
        }
        self.deferred_library_routing_build_notes
            .append(&mut tail.build_notes);
        LibraryRoutingTailClaim::NotesOnly
    }

    pub(crate) fn take_deferred_library_routing_build_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.deferred_library_routing_build_notes)
    }

    /// Atomically consume every supplied transfer and reserve the union of its
    /// still-current exact-record candidates.
    ///
    /// Every handle is prevalidated before any ledger entry, force marker, or
    /// activity reservation changes. Callers receiving `Superseded` may retry
    /// with the returned successor because that successor inherited the old
    /// candidates. `AlreadyConsumed` proves a different finalization owns the
    /// work and is terminal; only `MissingOrWrongOwner` permits an idempotent
    /// current-state fallback through
    /// [`Self::finalize_analysis_transfer_fallback`].
    #[cfg(test)]
    pub(crate) fn finalize_analysis_transfers(
        &mut self,
        finalization: AnalysisTransferFinalizationId,
        handles: &[AnalysisTransferHandle],
        additional_candidates: Vec<AnalysisTransferCandidate>,
    ) -> Result<AnalysisTransferFinalization, AnalysisTransferRejection> {
        self.finalize_analysis_transfers_excluding(
            finalization,
            handles,
            additional_candidates,
            &HashSet::new(),
            &HashSet::new(),
        )
    }

    /// Finalize transfers while atomically dropping transfer candidates already
    /// owned by exact, still-current reservations.
    ///
    /// `transfer_excluded` applies only to candidates inherited from handles;
    /// independent `additional_candidates` may represent later semantic work on
    /// the same open lifecycle. `all_excluded` is reserved for an exact current
    /// ticket (such as watched-batch pre-reservation) that already owns both
    /// categories. The caller validates ticket triggers under this same write
    /// lock. URI equality alone is insufficient because close/reopen can reuse
    /// the URI while changing lifecycle ownership.
    pub(crate) fn finalize_analysis_transfers_excluding(
        &mut self,
        finalization: AnalysisTransferFinalizationId,
        handles: &[AnalysisTransferHandle],
        mut additional_candidates: Vec<AnalysisTransferCandidate>,
        transfer_excluded: &HashSet<Url>,
        all_excluded: &HashSet<Url>,
    ) -> Result<AnalysisTransferFinalization, AnalysisTransferRejection> {
        if self.analysis_transfer_finalizations.contains(&finalization) {
            return Ok(AnalysisTransferFinalization::AlreadyFinalized);
        }
        let mut unique = HashSet::with_capacity(handles.len());
        for handle in handles {
            if !unique.insert(handle.identity) {
                return Err(AnalysisTransferRejection::MissingOrWrongOwner);
            }
            if self
                .analysis_transfers_consumed
                .contains_key(&handle.identity)
            {
                return Err(AnalysisTransferRejection::AlreadyConsumed { handle: *handle });
            }
            if let Some(successor) = self.analysis_transfer_successors.get(&handle.identity) {
                return Err(AnalysisTransferRejection::Superseded {
                    previous: *handle,
                    successor: AnalysisTransferHandle {
                        identity: *successor,
                    },
                });
            }
            if !self.analysis_transfers.contains_key(&handle.identity) {
                return Err(AnalysisTransferRejection::MissingOrWrongOwner);
            }
        }

        let consumed_identities: Vec<_> = handles.iter().map(|handle| handle.identity).collect();
        let mut transfer_candidates = Vec::new();
        for handle in handles {
            let state = self
                .analysis_transfers
                .remove(&handle.identity)
                .expect("all analysis transfer handles were prevalidated");
            transfer_candidates.extend(state.candidates);
        }
        self.analysis_transfer_finalizations.insert(finalization);
        transfer_candidates.retain(|candidate| {
            !transfer_excluded.contains(&candidate.uri) && !all_excluded.contains(&candidate.uri)
        });
        additional_candidates.retain(|candidate| !all_excluded.contains(&candidate.uri));
        additional_candidates.extend(transfer_candidates);
        let tickets = self
            .reserve_analysis_transfer_candidates(additional_candidates)
            .revalidations;
        let owned_candidates: Vec<_> = tickets
            .iter()
            .map(|ticket| (ticket.uri.clone(), ticket.trigger))
            .collect();
        for identity in consumed_identities {
            self.analysis_transfers_consumed
                .insert(identity, owned_candidates.clone());
        }
        Ok(AnalysisTransferFinalization::Committed(tickets))
    }

    /// Exact still-current triggers already owned by `handle`'s completed
    /// finalization. Metadata-only record replacement keeps the same trigger;
    /// close/reopen changes its epoch, so the new lifecycle is never suppressed
    /// by URI equality alone.
    pub(crate) fn current_consumed_analysis_transfer_candidate_uris(
        &self,
        handle: AnalysisTransferHandle,
    ) -> Vec<Url> {
        self.analysis_transfers_consumed
            .get(&handle.identity)
            .into_iter()
            .flatten()
            .filter(|(uri, trigger)| !trigger.is_stale(self, uri))
            .map(|(uri, _trigger)| uri.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn analysis_transfer_is_pending_for_test(
        &self,
        handle: AnalysisTransferHandle,
    ) -> bool {
        self.analysis_transfers.contains_key(&handle.identity)
    }

    #[cfg(test)]
    pub(crate) fn analysis_transfer_candidate_uris_for_test(
        &self,
        handle: AnalysisTransferHandle,
    ) -> Vec<Url> {
        self.analysis_transfers
            .get(&handle.identity)
            .map(|transfer| {
                transfer
                    .candidates
                    .iter()
                    .map(|candidate| candidate.uri.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn pending_post_seed_outer_is_empty_for_test(&self) -> bool {
        self.pending_post_seed_outer_handles.is_empty()
            && self.pending_post_seed_outer_candidates.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn install_routing_tail_for_test(
        &mut self,
        post_seed: LibraryRoutingPreSealPostSeed,
    ) -> AnalysisTransferHandle {
        let (_owner, mut deposit) = self.capture_current_library_routing_pre_seal();
        deposit.post_seed = Some(post_seed);
        deposit
            .build_notes
            .push("test routing tail build note".to_string());
        let identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: self.system_file_routing_owner_identity(),
            commit_generation: Self::mint_system_file_commit_generation(),
        });
        self.install_library_routing_transfer(identity, None, Vec::new(), deposit)
    }

    #[cfg(test)]
    pub(crate) fn install_system_file_transfer_for_test(
        &mut self,
        uris: impl IntoIterator<Item = Url>,
    ) -> SystemFileTransferredEffects {
        let candidates = self.capture_analysis_transfer_candidates(uris);
        let identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: self.system_file_routing_owner_identity(),
            commit_generation: Self::mint_system_file_commit_generation(),
        });
        let handle =
            self.install_analysis_transfer(identity, self.latest_system_file_transfer, candidates);
        self.latest_system_file_transfer = Some(identity);
        SystemFileTransferredEffects {
            handle,
            changed_uris: Vec::new(),
        }
    }

    /// Collapse an ownerless/degraded pre-seal ledger into one fresh pending
    /// SystemFile transfer against the current routing lineage.
    pub(crate) fn collapse_current_library_routing_pre_seal(
        &mut self,
        mut deposit: LibraryRoutingPreSealDeposit,
    ) -> Option<LibraryRoutingTransferredEffects> {
        if self.routing_shutdown.load(Ordering::Acquire) {
            for handle in deposit.handles {
                let mut identity = handle.identity;
                let mut visited = HashSet::new();
                while let Some(successor) =
                    self.analysis_transfer_successors.get(&identity).copied()
                {
                    if !visited.insert(identity) {
                        break;
                    }
                    identity = successor;
                }
                self.analysis_transfers.remove(&identity);
                self.analysis_transfers_consumed
                    .insert(identity, Vec::new());
            }
            if let Some(post_seed) = deposit.post_seed.take() {
                self.complete_post_seed_refresh_retry(post_seed.identity);
                self.complete_system_file_seed_retry(post_seed.identity);
                if let Some(system) = post_seed.deferred_system_file {
                    self.complete_post_seed_refresh_retry(system);
                    self.complete_system_file_seed_retry(system);
                }
            }
            for retired in deposit.retired_post_seed_owners {
                self.complete_post_seed_refresh_retry(retired);
                self.complete_system_file_seed_retry(retired);
            }
            return None;
        }
        let identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: self.system_file_routing_owner_identity(),
            commit_generation: Self::mint_system_file_commit_generation(),
        });
        let handle = self.install_library_routing_transfer(
            identity,
            self.latest_system_file_transfer,
            Vec::new(),
            deposit,
        );
        self.latest_system_file_transfer = Some(identity);
        Some(LibraryRoutingTransferredEffects {
            handle,
            changed_uris: Vec::new(),
            restart_owner: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn analysis_transfer_finalization_count_for_test(&self) -> usize {
        self.analysis_transfer_finalizations.len()
    }

    /// Complete a rejected handoff from current exact candidates exactly once.
    pub(crate) fn finalize_analysis_transfer_fallback(
        &mut self,
        finalization: AnalysisTransferFinalizationId,
        candidates: Vec<AnalysisTransferCandidate>,
    ) -> AnalysisTransferFinalization {
        if !self.analysis_transfer_finalizations.insert(finalization) {
            return AnalysisTransferFinalization::AlreadyFinalized;
        }
        AnalysisTransferFinalization::Committed(
            self.reserve_analysis_transfer_candidates(candidates)
                .revalidations,
        )
    }

    fn current_transfer_candidates(
        &self,
        candidates: Vec<AnalysisTransferCandidate>,
    ) -> Vec<AnalysisTransferCandidate> {
        candidates
            .into_iter()
            .filter(|candidate| {
                self.documents.record_token_is_current(&candidate.record)
                    && !candidate.trigger.is_stale(self, &candidate.uri)
                    && self.diagnostics_publish_allowed(&candidate.uri)
                    && self
                        .diagnostics_gate
                        .current_epoch(&candidate.uri)
                        .is_some()
            })
            .collect()
    }

    pub(crate) fn capture_analysis_transfer_candidates(
        &self,
        uris: impl IntoIterator<Item = Url>,
    ) -> Vec<AnalysisTransferCandidate> {
        uris.into_iter()
            .filter(|uri| self.documents.contains_key(uri))
            .map(|uri| {
                let record = self.documents.record_token(&uri);
                let trigger = DiagnosticsTrigger::capture(self, &uri);
                AnalysisTransferCandidate {
                    uri,
                    record,
                    trigger,
                    reservation: AnalysisTransferReservationPolicy::Dependent,
                }
            })
            .collect()
    }

    /// Begin one top-level scan driver.
    ///
    /// A newer arrival tombstones the older driver, but a committed unclaimed
    /// transfer survives until a successful newer commit inherits it.
    pub(crate) fn begin_workspace_scan_intent(&mut self) -> WorkspaceScanIntentToken {
        let generation = Self::mint_workspace_scan_intent_generation();
        self.workspace_scan_intent = Some(WorkspaceScanIntentState::Pending(generation));
        WorkspaceScanIntentToken { generation }
    }

    pub(crate) fn workspace_scan_intent_is_current(
        &self,
        intent: WorkspaceScanIntentToken,
    ) -> bool {
        self.workspace_scan_intent == Some(WorkspaceScanIntentState::Pending(intent.generation))
    }

    /// Capture the pre-I/O half of one full attempt.
    ///
    /// Attempt two calls this again and then repeats disk scan + derivation
    /// from scratch, but retains the same latest-arrival `intent`.
    pub(crate) fn capture_workspace_scan_input_basis(
        &self,
        intent: WorkspaceScanIntentToken,
    ) -> Option<WorkspaceScanInputBasis> {
        if !self.workspace_scan_intent_is_current(intent)
            || !self.cross_file_config.index_workspace
            || self.workspace_folders.is_empty()
        {
            return None;
        }
        Some(WorkspaceScanInputBasis {
            intent,
            scan_generation: self.workspace_scan_generation,
            tar_source_event_generation: self.tar_source_event_generation,
            analysis_config_generation: self.analysis_config_generation,
            chunk_override_generation: self.chunk_override_generation,
            workspace_folders: self.workspace_folders.clone(),
            max_chain_depth: self.cross_file_config.max_chain_depth,
            max_transitive_dependents_visited: self
                .cross_file_config
                .max_transitive_dependents_visited,
            exclusion_patterns: self.workspace_exclusions.patterns().to_vec(),
            index_workspace: self.cross_file_config.index_workspace,
        })
    }

    fn workspace_scan_input_basis_is_current(&self, basis: &WorkspaceScanInputBasis) -> bool {
        self.workspace_scan_intent_is_current(basis.intent)
            && self.workspace_scan_generation == basis.scan_generation
            && self.tar_source_event_generation == basis.tar_source_event_generation
            && self.analysis_config_generation == basis.analysis_config_generation
            && self.chunk_override_generation == basis.chunk_override_generation
            && self.workspace_folders == basis.workspace_folders
            && self.cross_file_config.max_chain_depth == basis.max_chain_depth
            && self.cross_file_config.max_transitive_dependents_visited
                == basis.max_transitive_dependents_visited
            && self.workspace_exclusions.patterns() == basis.exclusion_patterns.as_slice()
            && self.cross_file_config.index_workspace == basis.index_workspace
    }

    /// Capture the post-I/O authority half used for detached graph/open
    /// derivation. The caller holds one state read lock while taking
    /// `index_snapshot`, open overlays, and this basis.
    pub(crate) fn capture_workspace_scan_derivation_basis(
        &self,
        input: &WorkspaceScanInputBasis,
        index_snapshot: &crate::workspace_index::WorkspaceIndexSnapshot,
    ) -> Option<WorkspaceScanDerivationBasis> {
        if !self.workspace_scan_input_basis_is_current(input) {
            return None;
        }
        Some(WorkspaceScanDerivationBasis {
            input: input.clone(),
            graph_revision: self.cross_file_graph.edge_revision(),
            graph_authority_generation: self.workspace_graph_authority_generation,
            open_context_authority_generation: self.open_context_authority_generation,
            workspace_index_version: index_snapshot.version,
            workspace_index_max_files: self.workspace_index.config().max_files,
            workspace_index_max_file_size_bytes: self.workspace_index.config().max_file_size_bytes,
            workspace_index_artifact_capacity: index_snapshot.artifact_capacity_limit,
            workspace_index_pinned: index_snapshot.pinned.clone(),
            package_input_generation: self.package_input_generation(),
            package_config_generation: self.package_config_generation,
            system_file_routing: self.system_file_routing_stamp(),
            open_records: self
                .documents
                .keys()
                .map(|uri| (uri.clone(), self.documents.record_token(uri)))
                .collect(),
        })
    }

    fn workspace_scan_derivation_basis_is_current(
        &self,
        basis: &WorkspaceScanDerivationBasis,
    ) -> bool {
        let index = self.workspace_index.authority_snapshot();
        if !self.workspace_scan_input_basis_is_current(&basis.input)
            || self.cross_file_graph.edge_revision() != basis.graph_revision
            || self.workspace_graph_authority_generation != basis.graph_authority_generation
            || self.open_context_authority_generation != basis.open_context_authority_generation
            || index.version != basis.workspace_index_version
            || self.workspace_index.config().max_files != basis.workspace_index_max_files
            || self.workspace_index.config().max_file_size_bytes
                != basis.workspace_index_max_file_size_bytes
            || index.artifact_capacity_limit != basis.workspace_index_artifact_capacity
            || index.pinned != basis.workspace_index_pinned
            || self.package_input_generation() != basis.package_input_generation
            || self.package_config_generation != basis.package_config_generation
        {
            return false;
        }
        let current_open: std::collections::BTreeMap<_, _> = self
            .documents
            .keys()
            .map(|uri| (uri.clone(), self.documents.record_token(uri)))
            .collect();
        if current_open != basis.open_records {
            return false;
        }
        basis.system_file_routing == self.system_file_routing_stamp()
    }

    #[cfg(test)]
    pub(crate) fn workspace_scan_derivation_basis_is_current_for_test(
        &self,
        basis: &WorkspaceScanDerivationBasis,
    ) -> bool {
        self.workspace_scan_derivation_basis_is_current(basis)
    }

    pub(crate) fn workspace_scan_open_token(
        basis: &WorkspaceScanDerivationBasis,
        uri: &Url,
    ) -> Option<OpenRecordToken> {
        basis.open_records.get(uri).cloned()
    }

    pub(crate) fn workspace_graph_authority_generation(&self) -> u64 {
        self.workspace_graph_authority_generation
    }

    #[cfg(test)]
    pub(crate) fn workspace_scan_generation_for_test(&self) -> u64 {
        self.workspace_scan_generation
    }

    #[cfg(test)]
    pub(crate) fn open_context_authority_generation_for_test(&self) -> u64 {
        self.open_context_authority_generation.0
    }

    pub(crate) fn advance_workspace_graph_authority_generation(&mut self) {
        self.workspace_graph_authority_generation =
            self.workspace_graph_authority_generation.wrapping_add(1);
    }

    fn advance_open_context_authority_generation(&mut self) {
        self.open_context_authority_generation.0 =
            self.open_context_authority_generation.0.wrapping_add(1);
    }

    /// Replace the editor diagnostics policy and advance its compact authority
    /// stamp when the effective set changes.
    pub(crate) fn replace_editor_diagnostic_uris(&mut self, uris: Option<HashSet<Url>>) {
        if self.editor_diagnostic_uris == uris {
            return;
        }
        self.editor_diagnostic_uris = uris;
        self.editor_eligibility_generation.0 = self.editor_eligibility_generation.0.wrapping_add(1);
    }

    /// Retire every detached analysis basis captured before the latest parsed
    /// configuration recompute.
    pub(crate) fn advance_analysis_config_generation(&mut self) {
        self.analysis_config_generation.0 = self.analysis_config_generation.0.wrapping_add(1);
    }

    /// Retire detached scan candidates captured before a closed-file or
    /// scan-input write.
    ///
    /// A committed diagnostic transfer is not a candidate and deliberately
    /// survives this bump while post-scan package/config work converges. Only
    /// A successful later scan commit supersedes that transfer only after
    /// inheriting its unclaimed candidates.
    pub(crate) fn advance_workspace_scan_generation(&mut self) {
        self.workspace_scan_generation = self.workspace_scan_generation.wrapping_add(1);
    }

    /// Fence detached tar expansion before consulting the reverse registry,
    /// then return every finalized parent whose request may overlap an event.
    ///
    /// The unconditional generation bump closes the race where a CREATE
    /// arrives after a request begins walking the filesystem but before that
    /// parent has installed its first registry entry.
    pub(crate) fn record_tar_source_filesystem_events(
        &mut self,
        event_uris: impl IntoIterator<Item = Url>,
    ) -> Vec<Url> {
        let event_paths: Vec<PathBuf> = event_uris
            .into_iter()
            .filter_map(|uri| uri.to_file_path().ok())
            .collect();
        if event_paths.is_empty() {
            return Vec::new();
        }
        self.tar_source_event_generation = self
            .tar_source_event_generation
            .checked_add(1)
            .expect("tar_source filesystem-event generation exhausted");

        let mut overlapping_roots: Vec<PathBuf> = self
            .tar_source_parents_by_watch_path
            .keys()
            .filter(|watch_path| {
                event_paths.iter().any(|event_path| {
                    crate::cross_file::tar_source::paths_overlap(event_path, watch_path)
                })
            })
            .cloned()
            .collect();
        overlapping_roots.sort();
        overlapping_roots.dedup();
        for root in &overlapping_roots {
            self.bump_tar_source_watch_path_generation(root);
        }

        let mut parents = HashSet::new();
        for event_path in &event_paths {
            for (watch_path, owners) in &self.tar_source_parents_by_watch_path {
                if crate::cross_file::tar_source::paths_overlap(event_path, watch_path) {
                    parents.extend(
                        owners
                            .iter()
                            .filter(|parent| {
                                self.source_batch_filesystem_event_affects_parent(
                                    parent, event_path,
                                )
                            })
                            .cloned(),
                    );
                }
            }
        }
        let mut parents: Vec<_> = parents.into_iter().collect();
        parents.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        parents
    }

    /// Refine generic recursive watch-root overlap for implicit Shiny layouts.
    ///
    /// Explicit `tar_source()` and `list.files()` batches retain their existing
    /// recursive matching. A Shiny-only owner instead follows the convention's
    /// direct-child topology so unrelated application-root descendants do not
    /// force an open-parent refresh.
    fn source_batch_filesystem_event_affects_parent(
        &self,
        parent: &Url,
        event_path: &Path,
    ) -> bool {
        let metadata = if let Some(record) = self.documents.get_record(parent) {
            Some(Arc::clone(record.metadata()))
        } else if self.is_document_open_or_alias(parent) {
            None
        } else {
            self.workspace_index.get_metadata(parent)
        };
        let Some(metadata) = metadata else {
            return true;
        };
        if !metadata.tar_source_requests.is_empty()
            || !metadata.list_files_source_requests.is_empty()
        {
            return true;
        }
        metadata
            .shiny_application
            .as_ref()
            .is_none_or(|application| {
                crate::cross_file::shiny::filesystem_event_affects_application(
                    application,
                    event_path,
                )
            })
    }

    fn bump_tar_source_watch_path_generation(&mut self, path: &Path) {
        self.tar_source_watch_generation_counter = self
            .tar_source_watch_generation_counter
            .checked_add(1)
            .expect("tar_source watch-root generation exhausted");
        self.tar_source_watch_path_generations
            .insert(path.to_path_buf(), self.tar_source_watch_generation_counter);
    }

    /// Rebuild the bidirectional tar watch registry from authoritative
    /// finalized records. This is called only after a successful commit while
    /// holding the WorldState write lock, so rejected preparations cannot
    /// leak registry mutations.
    fn rebuild_tar_source_watch_registry(&mut self) {
        #[cfg(any(test, feature = "test-support"))]
        {
            self.tar_source_watch_full_rebuild_count += 1;
        }
        let previous_by_path = self.tar_source_parents_by_watch_path.clone();
        let (by_parent, by_path) = self.collect_authoritative_tar_source_watch_registry();
        let mut changed_paths: HashSet<PathBuf> = previous_by_path.keys().cloned().collect();
        changed_paths.extend(by_path.keys().cloned());
        let mut changed_paths: Vec<_> = changed_paths
            .into_iter()
            .filter(|path| previous_by_path.get(path) != by_path.get(path))
            .collect();
        changed_paths.sort();
        for path in changed_paths {
            self.bump_tar_source_watch_path_generation(&path);
        }
        self.tar_source_watch_paths_by_parent = by_parent;
        self.tar_source_parents_by_watch_path = by_path;
    }

    /// Compute the authoritative bidirectional registry without mutating
    /// generations. Tests use this as an explicit full-sweep oracle for the
    /// bounded post-commit gate.
    fn collect_authoritative_tar_source_watch_registry(
        &self,
    ) -> (HashMap<Url, Vec<PathBuf>>, HashMap<PathBuf, HashSet<Url>>) {
        let mut by_parent: HashMap<Url, Vec<PathBuf>> = HashMap::new();

        for uri in self.workspace_index.artifact_uris() {
            if self.is_document_open_or_alias(&uri) {
                continue;
            }
            if let Some(metadata) = self.workspace_index.get_metadata(&uri) {
                let mut paths = metadata.tar_source_expansion_watch_paths.clone();
                paths.sort();
                paths.dedup();
                if !paths.is_empty() {
                    by_parent.insert(uri, paths);
                }
            }
        }
        for uri in self.documents.keys() {
            let Some(record) = self.documents.get_record(uri) else {
                continue;
            };
            let mut paths = record.metadata().tar_source_expansion_watch_paths.clone();
            paths.sort();
            paths.dedup();
            if paths.is_empty() {
                by_parent.remove(uri);
            } else {
                by_parent.insert(uri.clone(), paths);
            }
        }

        let mut by_path: HashMap<PathBuf, HashSet<Url>> = HashMap::new();
        for (parent, paths) in &by_parent {
            for path in paths {
                by_path
                    .entry(path.clone())
                    .or_default()
                    .insert(parent.clone());
            }
        }
        (by_parent, by_path)
    }

    /// Read one parent's effective post-commit watch roots using the same
    /// authority precedence as [`Self::rebuild_tar_source_watch_registry`].
    fn authoritative_tar_source_watch_paths(&self, parent: &Url) -> Vec<PathBuf> {
        let metadata = if let Some(record) = self.documents.get_record(parent) {
            Some(Arc::clone(record.metadata()))
        } else if self.is_document_open_or_alias(parent) {
            None
        } else {
            self.workspace_index.get_metadata(parent)
        };
        let mut paths = metadata
            .map(|metadata| metadata.tar_source_expansion_watch_paths.clone())
            .unwrap_or_default();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Gate the rare full rebuild behind bounded authoritative parent checks.
    ///
    /// The common edit path names one parent. If its normalized roots still
    /// match the installed parent map, the check is O(1) in workspace size and
    /// performs no per-artifact index reads. A topology change deliberately
    /// falls back to the full rebuild so bidirectional ownership, net
    /// owner-set generation bumps, and tombstone retention keep one writer.
    fn refresh_tar_source_watch_registry(&mut self, refresh: TarSourceWatchRegistryRefresh) {
        let needs_rebuild = match refresh {
            TarSourceWatchRegistryRefresh::Full => true,
            TarSourceWatchRegistryRefresh::Parents(mut parents) => {
                parents.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
                parents.dedup();
                parents.into_iter().any(|parent| {
                    #[cfg(any(test, feature = "test-support"))]
                    {
                        self.tar_source_watch_parent_check_count += 1;
                    }
                    let authoritative = self.authoritative_tar_source_watch_paths(&parent);
                    self.tar_source_watch_paths_by_parent
                        .get(&parent)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        != authoritative.as_slice()
                })
            }
        };
        if needs_rebuild {
            self.rebuild_tar_source_watch_registry();
        }
    }

    /// Reconcile a bounded set of parents after an authoritative mutation that
    /// intentionally bypasses [`Self::try_commit_analysis`].
    pub(crate) fn refresh_tar_source_watch_parents(
        &mut self,
        parents: impl IntoIterator<Item = Url>,
    ) {
        self.refresh_tar_source_watch_registry(TarSourceWatchRegistryRefresh::Parents(
            parents.into_iter().collect(),
        ));
    }

    #[cfg(test)]
    pub(crate) fn source_batch_watch_snapshot_for_test(
        &self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> (u64, Vec<Option<u64>>, usize, usize, usize) {
        let generations = paths
            .into_iter()
            .map(|path| self.tar_source_watch_path_generations.get(&path).copied())
            .collect();
        (
            self.tar_source_event_generation,
            generations,
            self.tar_source_watch_path_generations.len(),
            self.tar_source_watch_paths_by_parent.len(),
            self.tar_source_parents_by_watch_path.len(),
        )
    }

    /// Current operational generation of raw package inputs.
    pub(crate) fn package_input_generation(&self) -> u64 {
        self.package_input_lifecycle.generation()
    }

    /// Record one raw package-input write or lifecycle/configuration transition.
    ///
    /// Call at the same lock-protected seam that mutates the input. Value-equal
    /// writes still advance: a detached seed must not infer freshness from
    /// semantic equality.
    pub(crate) fn record_package_input_mutation(&mut self) {
        self.package_input_lifecycle.advance();
    }

    /// Apply a `PackageInputDelta` produced by an event handler.
    /// Caller has already mutated `self.package_inputs` to reflect the event and
    /// recorded that mutation through `record_package_input_mutation`.
    /// Recomputes `package_state` as a pure function of inputs.
    pub fn apply_package_event(&mut self, delta: &crate::package_state::PackageInputDelta) {
        let _ = self
            .apply_package_event_with_routing_policy(delta, PackageRoutingOwnerPolicy::IfChanged);
    }

    /// Apply an event while returning the exact routing owner minted by a
    /// workspace/package-routing change.
    ///
    /// Configuration transitions use this instead of the compatibility
    /// adapter above so `system.file()` convergence cannot be discarded.
    pub(crate) fn apply_package_event_with_routing_owner(
        &mut self,
        delta: &crate::package_state::PackageInputDelta,
    ) -> Option<SystemFileRoutingOwnerIdentity> {
        self.apply_package_event_with_routing_policy(delta, PackageRoutingOwnerPolicy::IfChanged)
    }

    /// Compatibility tail for the single-owner CLI seed adapter.
    ///
    /// LSP seed/reseed callers install a complete
    /// [`PreparedPackageProjection`]; the CLI retains its synchronous in-place
    /// adapter and shares only this fresh-routing policy tail.
    pub(crate) fn apply_package_seed_event(
        &mut self,
        delta: &crate::package_state::PackageInputDelta,
    ) {
        let _ = self.apply_package_event_with_routing_policy(
            delta,
            PackageRoutingOwnerPolicy::FreshSeedOwner,
        );
    }

    fn apply_package_event_with_routing_policy(
        &mut self,
        delta: &crate::package_state::PackageInputDelta,
        routing_owner: PackageRoutingOwnerPolicy,
    ) -> Option<SystemFileRoutingOwnerIdentity> {
        let new_package_state = crate::package_state::derive_package_state(
            &self.package_state,
            &self.package_inputs,
            delta,
        );
        self.install_derived_package_state(new_package_state, routing_owner)
    }

    /// Install one complete raw+derived package projection.
    ///
    /// Detached callers prepare both values from the same immutable snapshot.
    /// Embedded open transactions call this only after their outer analysis
    /// basis has passed preflight, so the input replacement, derived record,
    /// routing owner, and local-dev overlay become visible as one in-memory
    /// commit.
    fn install_prepared_package_projection(
        &mut self,
        prepared: PreparedPackageProjection,
    ) -> Option<SystemFileRoutingOwnerIdentity> {
        self.package_inputs = prepared.inputs;
        self.record_package_input_mutation();
        self.install_derived_package_state(prepared.state, prepared.routing_owner)
    }

    pub(crate) fn capture_package_projection_basis(&self) -> PackageProjectionBasis {
        PackageProjectionBasis {
            package_input_generation: self.package_input_generation(),
            package_state_record_generation: self.package_state_record_generation,
            workspace_scan_generation: self.workspace_scan_generation,
            package_config_generation: self.package_config_generation,
            open_context_authority_generation: self.open_context_authority_generation,
            workspace_root: self.package_inputs.workspace_root.clone(),
            workspace_folders: self.workspace_folders.clone(),
            exclusion_patterns: self.workspace_exclusions.patterns().to_vec(),
            package_mode: self.package_inputs.package_mode,
            model_rprofile: self.package_inputs.model_rprofile,
            post_seed_ownership: PostSeedPackageProjectionOwnership::Unrestricted,
        }
    }

    pub(crate) fn current_sysdata_fallback_owner(&self) -> Option<SysdataFallbackOwner> {
        let workspace_root = self.package_inputs.workspace_root.clone()?;
        (self.package_seed_install_id != 0).then_some(SysdataFallbackOwner {
            seed_install_id: self.package_seed_install_id,
            workspace_root,
        })
    }

    pub(crate) fn sysdata_fallback_owner_is_current(&self, owner: &SysdataFallbackOwner) -> bool {
        self.package_seed_install_id == owner.seed_install_id
            && self.package_inputs.workspace_root.as_ref() == Some(&owner.workspace_root)
    }

    pub(crate) fn capture_sysdata_fallback_basis(
        &self,
        owner: &SysdataFallbackOwner,
    ) -> Option<(SysdataFallbackBasis, std::path::PathBuf)> {
        if !self.sysdata_fallback_owner_is_current(owner) {
            return None;
        }
        let runtime_r_path = self
            .package_library
            .r_subprocess()
            .map(|runtime| runtime.r_path().clone())?;
        let runtime_identity = self
            .package_library
            .r_subprocess()
            .map(|runtime| runtime.runtime_identity())?;
        Some((
            SysdataFallbackBasis {
                seed_install_id: self.package_seed_install_id,
                workspace_root: owner.workspace_root.clone(),
                package: self.capture_package_projection_basis(),
                package_library_install_id: self.package_library_install_id,
                package_library_content_generation: self.package_library_content_generation,
                configured_r_path: self.cross_file_config.packages_r_path.clone(),
                runtime_r_path: runtime_r_path.clone(),
                runtime_identity,
                analysis_config_generation: self.analysis_config_generation,
            },
            runtime_r_path,
        ))
    }

    pub(crate) fn capture_exact_foreground_post_seed_package_projection_basis(
        &self,
        identity: PackageSeedInstalledIdentity,
    ) -> PackageProjectionBasis {
        let mut basis = self.capture_package_projection_basis();
        basis.post_seed_ownership = PostSeedPackageProjectionOwnership::ForegroundExact(identity);
        basis
    }

    pub(crate) fn capture_current_foreground_post_seed_package_projection_basis(
        &self,
        identity: PackageSeedInstalledIdentity,
    ) -> PackageProjectionBasis {
        let mut basis = self.capture_package_projection_basis();
        basis.post_seed_ownership = PostSeedPackageProjectionOwnership::ForegroundCurrent(identity);
        basis
    }

    pub(crate) fn capture_coordinator_post_seed_package_projection_basis(
        &self,
        identity: PackageSeedInstalledIdentity,
    ) -> PackageProjectionBasis {
        let mut basis = self.capture_package_projection_basis();
        basis.post_seed_ownership = PostSeedPackageProjectionOwnership::Coordinator(identity);
        basis
    }

    pub(crate) fn try_install_prepared_package_projection(
        &mut self,
        basis: &PackageProjectionBasis,
        prepared: PreparedPackageProjection,
    ) -> Result<Option<SystemFileRoutingOwnerIdentity>, PackageProjectionInstallRejected> {
        if !self.package_projection_basis_is_current(basis) {
            return Err(PackageProjectionInstallRejected::StaleBasis);
        }
        Ok(self.install_prepared_package_projection(prepared))
    }

    pub(crate) fn try_install_sysdata_fallback_projection(
        &mut self,
        basis: &SysdataFallbackBasis,
        observation: &crate::package_state::sysdata::SysdataFileObservation,
        prepared: PreparedPackageProjection,
        affected_uris: impl IntoIterator<Item = Url>,
    ) -> Result<SysdataFallbackCommitEffects, PackageProjectionInstallRejected> {
        // This is the only sysdata byte read under the commit write lock. It is
        // intentionally inside the central commit so no off-lock derivation or
        // wait for that lock can separate the observation from the authority
        // CAS. Runtime-identity metadata is revalidated in the same narrow
        // section below.
        if !observation.is_current(&basis.workspace_root)
            || !self.sysdata_fallback_basis_is_current(basis)
        {
            return Err(PackageProjectionInstallRejected::StaleBasis);
        }
        let routing_owner = self.install_prepared_package_projection(prepared);
        let candidates = self.capture_analysis_transfer_candidates(affected_uris);
        Ok(SysdataFallbackCommitEffects {
            routing_owner,
            candidates,
        })
    }

    pub(crate) fn sysdata_fallback_basis_is_current(&self, basis: &SysdataFallbackBasis) -> bool {
        let runtime_r_path = self
            .package_library
            .r_subprocess()
            .map(|runtime| runtime.r_path());
        let runtime_identity = self
            .package_library
            .r_subprocess()
            .map(|runtime| runtime.runtime_identity());
        if self.package_seed_install_id != basis.seed_install_id
            || self.package_inputs.workspace_root.as_ref() != Some(&basis.workspace_root)
            || self.package_library_install_id != basis.package_library_install_id
            || self.package_library_content_generation != basis.package_library_content_generation
            || self.cross_file_config.packages_r_path != basis.configured_r_path
            || runtime_r_path != Some(&basis.runtime_r_path)
            || runtime_identity.as_ref() != Some(&basis.runtime_identity)
            || self.analysis_config_generation != basis.analysis_config_generation
            || !self.package_projection_basis_is_current(&basis.package)
        {
            return false;
        }
        true
    }

    fn package_projection_basis_is_current(&self, basis: &PackageProjectionBasis) -> bool {
        !self.routing_shutdown.load(Ordering::Acquire)
            && self.package_input_generation() == basis.package_input_generation
            && self.package_state_record_generation == basis.package_state_record_generation
            && self.workspace_scan_generation == basis.workspace_scan_generation
            && self.package_config_generation == basis.package_config_generation
            && self.open_context_authority_generation == basis.open_context_authority_generation
            && self.package_inputs.workspace_root == basis.workspace_root
            && self.workspace_folders == basis.workspace_folders
            && self.workspace_exclusions.patterns() == basis.exclusion_patterns
            && self.package_inputs.package_mode == basis.package_mode
            && self.package_inputs.model_rprofile == basis.model_rprofile
            && match basis.post_seed_ownership {
                PostSeedPackageProjectionOwnership::Unrestricted => true,
                PostSeedPackageProjectionOwnership::ForegroundExact(identity) => {
                    self.pending_post_seed_refresh_retry.is_none()
                        && self.pending_system_file_seed_retry.is_none()
                        && self.package_seed_installed_identity_is_current(identity)
                }
                PostSeedPackageProjectionOwnership::ForegroundCurrent(identity) => {
                    self.pending_post_seed_refresh_retry.is_none()
                        && self.pending_system_file_seed_retry.is_none()
                        && self.package_seed_tail_owner_is_current(identity)
                }
                PostSeedPackageProjectionOwnership::Coordinator(identity) => {
                    self.pending_post_seed_refresh_retry == Some(identity)
                        && self.package_seed_tail_owner_is_current(identity)
                        && self.pending_system_file_seed_retry.is_none()
                        && (!self.pending_post_seed_requires_system_transfer
                            || self
                                .pending_post_seed_system_transfer
                                .as_ref()
                                .is_some_and(|(owner, _)| *owner == identity))
                }
            }
    }

    /// Shared derived-record tail for both legacy in-place input adapters and
    /// complete prepared package projections.
    fn install_derived_package_state(
        &mut self,
        new_package_state: crate::package_state::PackageState,
        routing_owner: PackageRoutingOwnerPolicy,
    ) -> Option<SystemFileRoutingOwnerIdentity> {
        let old_routing = self
            .package_state
            .workspace()
            .map(|workspace| (workspace.name.as_str().to_owned(), workspace.root.clone()));
        self.package_state.set_from(new_package_state);
        self.package_state_record_generation = self.package_state_record_generation.wrapping_add(1);
        let new_routing = self
            .package_state
            .workspace()
            .map(|workspace| (workspace.name.as_str().to_owned(), workspace.root.clone()));
        let routing_changed = matches!(routing_owner, PackageRoutingOwnerPolicy::FreshSeedOwner)
            || old_routing != new_routing;
        if routing_changed {
            self.record_system_file_routing_owner_change();
        }

        // Refresh the package library's local-dev overlay from the freshly-set
        // contribution. Every in-place event and prepared projection converges
        // through this derived-record installer, so this is the one correct
        // refresh point.
        self.refresh_local_dev_overlay();
        routing_changed.then(|| self.system_file_routing_owner_identity())
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
    pub(crate) const MULTI_SEED_VISITED_CEILING: usize = 200_000;

    /// Creates a new WorldState initialized with default cross-file configuration and empty caches.
    ///
    /// The returned state is populated with:
    /// - default CrossFileConfig (logged at initialization),
    /// - one empty open-document store and one empty closed-file index,
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
            // Analysis authority
            documents: OpenDocumentStore::new(),
            workspace_index: WorkspaceIndex::new(Default::default()),

            open_document_aliases: OpenDocumentAliases::default(),

            // Workspace configuration
            workspace_folders: Vec::new(),

            // Package function awareness
            // Initialize with empty state - will be populated via initialize() or async initialization
            // Requirement 13.4: THE Package_Cache SHALL support concurrent read access
            package_library: Arc::new(PackageLibrary::new_empty()),
            package_library_install_id: Self::mint_package_library_install_id(),
            package_library_content_generation: 0,
            library_replacement_lifecycle: Arc::new(parking_lot::Mutex::new(
                LibraryReplacementLifecycle::default(),
            )),
            routing_shutdown: Arc::new(AtomicBool::new(false)),
            library_routing_reconcile_wake: Arc::new(tokio::sync::Notify::new()),
            library_routing_reconcile_wake_generation: Arc::new(AtomicU64::new(0)),
            library_routing_reconcile_eligibility_generation: Arc::new(AtomicU64::new(0)),
            package_seed_install_id: 0,
            pending_system_file_seed_retry: None,
            pending_post_seed_refresh_retry: None,
            pending_post_seed_system_transfer: None,
            pending_post_seed_requires_system_transfer: false,
            pending_post_seed_outer_handles: Vec::new(),
            pending_post_seed_outer_candidates: Vec::new(),
            deferred_library_routing_build_notes: Vec::new(),
            system_file_routing_owner_generation: Self::mint_system_file_routing_owner_generation(),

            // Caches
            help_cache: crate::help::HelpCache::new(),
            html_help_cache: crate::help::HtmlHelpCache::new(),
            signature_cache: Arc::new(SignatureCache::new(500)),
            cross_file_file_cache: CrossFileFileCache::new(),
            diagnostics_gate: CrossFileDiagnosticsGate::new(),
            diagnostics_coherence: Arc::new(DiagnosticsCoherenceGate::default()),
            diagnostics_publish_lock: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(any(test, feature = "test-support"))]
            diagnostics_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            did_open_reservation_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            did_open_reservation_snapshot_for_test: Vec::new(),
            #[cfg(test)]
            did_change_reservation_snapshot_for_test: Vec::new(),
            #[cfg(test)]
            did_open_commit_snapshot_for_test: None,
            #[cfg(test)]
            did_open_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            did_close_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            did_close_post_commit_pre_publish_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            close_resync_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            close_resync_post_attempt_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            open_lifecycle_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            open_lifecycle_post_commit_pre_clear_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            open_lifecycle_post_unlock_pre_spawn_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            open_lifecycle_added_effects_complete_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            workspace_scan_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            watched_package_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            watched_undecodable_retry_pre_delay_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            watched_batch_pre_finalize_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            watched_batch_fallback_after_updates_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            watched_final_handoff_test_capture: FinalHandoffCapture::default(),
            #[cfg(test)]
            config_reload_publish_test_capture: FinalHandoffCapture::default(),
            #[cfg(test)]
            analysis_revalidation_final_handoff_test_capture: FinalHandoffCapture::default(),
            #[cfg(test)]
            did_close_final_handoff_test_capture: FinalHandoffCapture::default(),
            #[cfg(test)]
            close_resync_final_handoff_test_capture: FinalHandoffCapture::default(),
            #[cfg(test)]
            config_system_file_post_send_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            diagnostics_post_publish_lock_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            diagnostics_post_consume_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            diagnostics_backstop_respawn_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            diagnostics_supersession_handoffs_for_test: Arc::new(std::sync::Mutex::new(
                HashMap::new(),
            )),
            #[cfg(test)]
            alias_reconcile_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            open_tar_source_refresh_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            open_tar_source_refresh_pre_release_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            package_init_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            package_init_post_claim_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            library_routing_reconcile_pre_drain_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            library_routing_reconcile_post_reload_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            degraded_reconcile_pre_park_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            system_file_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            system_file_pre_derivation_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            system_file_test_reject_remaining: 0,
            #[cfg(test)]
            system_file_test_commit_attempts: 0,
            #[cfg(any(test, feature = "test-support"))]
            library_routing_derivation_on_tokio_for_test: false,
            #[cfg(test)]
            library_routing_derivation_lane_for_test: Arc::new(
                crate::backend::LibraryRoutingDerivationLane::new(),
            ),
            #[cfg(test)]
            library_routing_test_reject_remaining: 0,
            #[cfg(test)]
            library_routing_test_commit_attempts: 0,
            #[cfg(test)]
            library_routing_deferred_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            library_routing_deferred_handles_for_test: Vec::new(),
            #[cfg(test)]
            library_routing_deferred_candidates_for_test: Vec::new(),
            #[cfg(test)]
            library_routing_deferred_post_seed_for_test: None,
            #[cfg(test)]
            open_edit_fallback_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            live_package_open_edit_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            post_seed_refresh_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            post_seed_refresh_test_reject_remaining: 0,
            #[cfg(test)]
            post_seed_refresh_test_commit_attempts: 0,
            #[cfg(test)]
            sysdata_fallback_pre_commit_test_pause:
                crate::cross_file::revalidation::DiagnosticsPublishPause::default(),
            #[cfg(test)]
            sysdata_fallback_test_reject_remaining: 0,
            #[cfg(test)]
            sysdata_fallback_test_commit_attempts: 0,
            #[cfg(test)]
            force_open_edit_overflow_for_test: false,
            #[cfg(test)]
            force_open_install_local_only_for_test: false,
            #[cfg(test)]
            force_package_library_not_ready_for_test: false,
            #[cfg(test)]
            package_library_build_outcome_for_test: None,
            #[cfg(test)]
            force_package_init_stale_for_test: false,

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
            effective_lint_config_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            workspace_exclusions: crate::config_file::CompiledWorkspaceExclusions::default(),
            per_document_indent_options: std::collections::HashMap::new(),
            cross_file_meta: MetadataCache::new(),
            cross_file_graph: DependencyGraph::new(),
            standalone_scope_cache: Arc::new(
                crate::cross_file::standalone_cache::StandaloneScopeCache::new(),
            ),
            package_config_generation: 0,
            cross_file_revalidation: CrossFileRevalidationState::new(),
            open_tar_source_refreshes: CrossFileRevalidationState::new(),
            cross_file_activity: CrossFileActivityState::new(),
            editor_diagnostic_uris: None,
            editor_eligibility_generation: EditorEligibilityGeneration(0),
            analysis_config_generation: AnalysisConfigGeneration(0),
            chunk_override_generation: ChunkOverrideGeneration(0),
            editor_chunk_kind_overrides: HashMap::new(),
            watched_file_resync_generation_counter: 0,
            watched_file_resync_generations: HashMap::new(),
            tar_source_event_generation: 0,
            tar_source_watch_generation_counter: 0,
            tar_source_watch_path_generations: HashMap::new(),
            tar_source_watch_paths_by_parent: HashMap::new(),
            tar_source_parents_by_watch_path: HashMap::new(),
            libpath_watcher: LibpathWatcherState::Disabled,
            libpath_watcher_owner_generation: Self::mint_libpath_watcher_owner_generation(),
            package_library_ready: false,
            workspace_scan_generation: 0,
            workspace_scan_intent: None,
            analysis_transfers: HashMap::new(),
            analysis_transfer_successors: HashMap::new(),
            analysis_transfers_consumed: HashMap::new(),
            analysis_transfer_finalizations: HashSet::new(),
            latest_workspace_scan_transfer: None,
            latest_system_file_transfer: None,
            workspace_graph_authority_generation: 0,
            open_context_authority_generation: OpenContextAuthorityGeneration(0),
            open_install_intents: HashMap::new(),
            open_close_intents: HashMap::new(),
            open_lifecycle_intent: None,
            #[cfg(any(test, feature = "test-support"))]
            open_pin_recompute_count: 0,
            #[cfg(any(test, feature = "test-support"))]
            watched_batch_test_reject_once: false,
            #[cfg(any(test, feature = "test-support"))]
            watched_batch_test_reject_remaining: 0,
            #[cfg(any(test, feature = "test-support"))]
            watched_package_test_compute_fail_remaining: 0,
            #[cfg(any(test, feature = "test-support"))]
            watched_batch_test_commit_attempts: 0,
            #[cfg(any(test, feature = "test-support"))]
            analysis_revalidation_reservation_count: 0,
            #[cfg(any(test, feature = "test-support"))]
            tar_source_watch_full_rebuild_count: 0,
            #[cfg(any(test, feature = "test-support"))]
            tar_source_watch_parent_check_count: 0,
            workspace_scan_complete: false,
            package_state: crate::package_state::PackageState::new(),
            package_state_record_generation: 0,
            package_inputs: crate::package_state::PackageInputs::default(),
            package_input_lifecycle: crate::package_state::PackageInputLifecycle::default(),
            package_seed_retry: crate::package_state::PackageSeedRetryLifecycle::default(),
            library_routing_retry: Arc::new(
                crate::package_state::PackageSeedRetryLifecycle::default(),
            ),
            watched_package_retry: Arc::new(
                crate::package_state::PackageSeedRetryLifecycle::default(),
            ),
            sysdata_fallback_retry: crate::package_state::PackageSeedRetryLifecycle::default(),
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

    fn mint_open_lifecycle_intent_generation() -> u64 {
        NEXT_OPEN_LIFECYCLE_INTENT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("open-lifecycle intent generation counter exhausted")
    }

    /// Claim latest-arrival ownership for one active-document notification.
    /// Both activity-only and diagnostic-policy payloads use this sequencer,
    /// so a newer activity hint can supersede an older paused lifecycle batch.
    pub(crate) fn begin_open_lifecycle_intent(&mut self) -> OpenLifecycleIntentToken {
        let generation = Self::mint_open_lifecycle_intent_generation();
        self.open_lifecycle_intent = Some(OpenLifecycleIntentState::Pending(generation));
        OpenLifecycleIntentToken { generation }
    }

    /// Commit one active-document payload atomically if it is still the most
    /// recently arrived notification.
    ///
    /// Lifecycle transitions are derived only after intent validation from
    /// the final open-document set and current editor policy. The returned
    /// tickets carry exact post-mint triggers; callers must not fresh-capture
    /// them later, because a remove/re-add may already have retired the epoch.
    pub(crate) fn commit_open_lifecycle_batch_if_current(
        &mut self,
        prepared: PreparedOpenLifecycleBatch,
    ) -> Option<OpenLifecycleBatchEffects> {
        if self.open_lifecycle_intent
            != Some(OpenLifecycleIntentState::Pending(
                prepared.intent.generation,
            ))
        {
            return None;
        }

        let Some(new_uris) = prepared.diagnostic_uris else {
            self.cross_file_activity.update(
                prepared.active_uri,
                prepared.visible_uris,
                prepared.timestamp_ms,
            );
            self.open_lifecycle_intent = Some(OpenLifecycleIntentState::Committed(
                prepared.intent.generation,
            ));
            return Some(OpenLifecycleBatchEffects::default());
        };

        let previously_eligible: HashSet<Url> = self
            .documents
            .keys()
            .filter(|uri| {
                self.diagnostics_publish_allowed(uri)
                    && self.diagnostics_gate.current_epoch(uri).is_some()
            })
            .cloned()
            .collect();
        let newly_eligible: HashSet<Url> = self
            .documents
            .keys()
            .filter(|uri| new_uris.contains(*uri))
            .cloned()
            .collect();
        let mut removed_clears: Vec<Url> = previously_eligible
            .difference(&newly_eligible)
            .cloned()
            .collect();
        let mut added: Vec<Url> = newly_eligible
            .difference(&previously_eligible)
            .cloned()
            .collect();
        removed_clears.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        added.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));

        self.cross_file_activity.update(
            prepared.active_uri,
            prepared.visible_uris,
            prepared.timestamp_ms,
        );
        self.replace_editor_diagnostic_uris(Some(new_uris));
        for uri in &removed_clears {
            self.retire_open_document_diagnostic_lifecycle(uri);
        }
        for uri in &added {
            self.begin_open_document_diagnostic_lifecycle(uri);
        }
        let debounce_ms = self.cross_file_config.revalidation_debounce_ms;
        let added_tickets = added
            .into_iter()
            .map(|uri| AnalysisRevalidationTicket {
                trigger: DiagnosticsTrigger::capture(self, &uri),
                debounce_ms,
                uri,
            })
            .collect();
        self.open_lifecycle_intent = Some(OpenLifecycleIntentState::Committed(
            prepared.intent.generation,
        ));
        Some(OpenLifecycleBatchEffects {
            removed_clears,
            added_tickets,
        })
    }

    #[cfg(test)]
    pub(crate) fn open_lifecycle_intent_for_test(&self) -> Option<(&'static str, u64)> {
        self.open_lifecycle_intent.map(|intent| match intent {
            OpenLifecycleIntentState::Pending(generation) => ("pending", generation),
            OpenLifecycleIntentState::Committed(generation) => ("committed", generation),
        })
    }

    #[cfg(test)]
    pub(crate) fn editor_eligibility_generation_for_test(&self) -> u64 {
        self.editor_eligibility_generation.0
    }

    fn mint_open_install_intent_generation() -> u64 {
        NEXT_OPEN_INSTALL_INTENT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("open-install intent generation counter exhausted")
    }

    /// Claim latest-arrival ownership for one `didOpen` request.
    pub(crate) fn begin_open_install_intent(&mut self, uri: &Url) -> OpenInstallIntentToken {
        self.cancel_open_close_intent(uri);
        let generation = Self::mint_open_install_intent_generation();
        self.open_install_intents
            .insert(uri.clone(), OpenInstallIntentState::Pending(generation));
        OpenInstallIntentToken {
            uri: uri.clone(),
            generation,
            target: self.documents.record_token(uri),
        }
    }

    pub(crate) fn open_install_intent_is_current(&self, token: &OpenInstallIntentToken) -> bool {
        self.open_install_intents.get(token.uri())
            == Some(&OpenInstallIntentState::Pending(token.generation))
    }

    /// Invalidate every pending install for `uri`, including while absent.
    pub(crate) fn cancel_open_install_intent(&mut self, uri: &Url) {
        let generation = Self::mint_open_install_intent_generation();
        self.open_install_intents
            .insert(uri.clone(), OpenInstallIntentState::Cancelled(generation));
    }

    /// Cancel only an exact still-pending request after terminal rejection.
    pub(crate) fn cancel_open_install_intent_if_current(
        &mut self,
        token: &OpenInstallIntentToken,
    ) -> bool {
        if !self.open_install_intent_is_current(token) {
            return false;
        }
        self.cancel_open_install_intent(token.uri());
        true
    }

    fn consume_open_install_intent(&mut self, token: &OpenInstallIntentToken) {
        debug_assert!(self.open_install_intent_is_current(token));
        let generation = Self::mint_open_install_intent_generation();
        self.open_install_intents.insert(
            token.uri().clone(),
            OpenInstallIntentState::Installed(generation),
        );
    }

    fn mint_open_close_intent_generation() -> u64 {
        NEXT_OPEN_CLOSE_INTENT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("open-close intent generation counter exhausted")
    }

    /// Claim latest-arrival ownership for one `didClose` request and
    /// immediately tombstone an invisible pending `didOpen`.
    pub(crate) fn begin_open_close_intent(&mut self, uri: &Url) -> OpenCloseIntentToken {
        self.cancel_open_install_intent(uri);
        let generation = Self::mint_open_close_intent_generation();
        self.open_close_intents
            .insert(uri.clone(), OpenCloseIntentState::Pending(generation));
        OpenCloseIntentToken {
            uri: uri.clone(),
            generation,
            target: self.documents.record_token(uri),
        }
    }

    pub(crate) fn open_close_intent_is_current(&self, token: &OpenCloseIntentToken) -> bool {
        self.open_close_intents.get(token.uri())
            == Some(&OpenCloseIntentState::Pending(token.generation))
            && self.documents.record_token_is_current(&token.target)
    }

    #[cfg(test)]
    pub(crate) fn open_close_intent_for_test(&self, uri: &Url) -> Option<(&'static str, u64)> {
        self.open_close_intents.get(uri).map(|intent| match intent {
            OpenCloseIntentState::Pending(generation) => ("pending", *generation),
            OpenCloseIntentState::Closed(generation) => ("closed", *generation),
            OpenCloseIntentState::Cancelled(generation) => ("cancelled", *generation),
        })
    }

    /// Invalidate every pending close for `uri`, including while absent.
    pub(crate) fn cancel_open_close_intent(&mut self, uri: &Url) {
        let generation = Self::mint_open_close_intent_generation();
        self.open_close_intents
            .insert(uri.clone(), OpenCloseIntentState::Cancelled(generation));
    }

    pub(crate) fn cancel_open_close_intent_if_current(
        &mut self,
        token: &OpenCloseIntentToken,
    ) -> bool {
        if self.open_close_intents.get(token.uri())
            != Some(&OpenCloseIntentState::Pending(token.generation))
        {
            return false;
        }
        self.cancel_open_close_intent(token.uri());
        true
    }

    fn consume_open_close_intent(&mut self, token: &OpenCloseIntentToken) {
        debug_assert!(
            self.open_close_intents.get(token.uri())
                == Some(&OpenCloseIntentState::Pending(token.generation))
        );
        let generation = Self::mint_open_close_intent_generation();
        self.open_close_intents.insert(
            token.uri().clone(),
            OpenCloseIntentState::Closed(generation),
        );
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

    /// Begin a lifecycle for an already-open document and project the epoch
    /// into its immutable analysis provenance.
    pub(crate) fn begin_open_document_diagnostic_lifecycle(
        &mut self,
        uri: &Url,
    ) -> Option<DiagnosticsEpoch> {
        let epoch = self.begin_diagnostic_lifecycle(uri)?;
        if let Some(generation) = self
            .documents
            .get_record(uri)
            .map(|record| record.generation())
        {
            self.documents
                .replace_lifecycle_epoch_if_current(uri, generation, Some(epoch))
                .expect("open lifecycle replacement remains current under the state write lock");
            self.advance_open_context_authority_generation();
        }
        Some(epoch)
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

    /// Retire diagnostics for a document that remains open.
    ///
    /// Tab removal clears the publish gate and also replaces the immutable
    /// record so its provenance cannot advertise the retired epoch.
    pub(crate) fn retire_open_document_diagnostic_lifecycle(&mut self, uri: &Url) {
        self.retire_diagnostic_lifecycle(uri);
        if let Some(generation) = self
            .documents
            .get_record(uri)
            .map(|record| record.generation())
        {
            self.documents
                .replace_lifecycle_epoch_if_current(uri, generation, None)
                .expect("open lifecycle retirement remains current under the state write lock");
            self.advance_open_context_authority_generation();
        }
    }

    /// Retire every diagnostics lifecycle and cancel all pending debounced
    /// work. Called on server shutdown so no in-flight diagnostic work can
    /// publish after the shutdown response.
    pub(crate) fn retire_all_diagnostic_lifecycles(&self) {
        self.cross_file_revalidation.cancel_all();
        self.open_tar_source_refreshes.cancel_all();
        self.diagnostics_gate.clear_all();
    }

    /// Cancel delayed package convergence tasks during server shutdown.
    pub(crate) fn cancel_package_seed_retry(&self) {
        self.package_seed_retry.cancel();
        self.library_routing_retry.cancel();
        self.watched_package_retry.cancel();
        self.sysdata_fallback_retry.cancel();
    }

    /// Retire every unpublished routing tail during shutdown without creating
    /// diagnostic tickets. Diagnostic epochs are already retired by the
    /// caller; this sweep deterministically releases retry identities and
    /// state-owned ledgers so detached coordinators fail their next owner
    /// check.
    pub(crate) fn drain_library_routing_tails_for_shutdown(
        &mut self,
    ) -> Option<Arc<crate::libpath_watcher::LibpathWatcherHandle>> {
        self.routing_shutdown.store(true, Ordering::Release);
        self.libpath_watcher_owner_generation = Self::mint_libpath_watcher_owner_generation();
        let retired_watcher = self.libpath_watcher.retire();
        let mut transfer_identities: Vec<_> = self
            .analysis_transfers
            .iter()
            .filter_map(|(identity, transfer)| transfer.routing_tail.as_ref().map(|_| *identity))
            .collect();
        // Shutdown retires every diagnostic lifecycle, so no pending
        // analysis handoff may remain publishable. This also covers a
        // config-system.file delivery already sent to its caller: it has no
        // routing tail, but the tracked fallback root must observe the drain
        // instead of finalizing it after shutdown.
        transfer_identities.extend(self.analysis_transfers.keys().copied());
        transfer_identities.extend(
            self.pending_post_seed_outer_handles
                .iter()
                .map(|handle| handle.identity),
        );
        transfer_identities.extend(
            self.pending_post_seed_system_transfer
                .as_ref()
                .map(|(_, handle)| handle.identity),
        );
        let mut terminal_identities = HashSet::new();
        for origin in transfer_identities {
            let mut terminal = origin;
            let mut visited = HashSet::new();
            while let Some(successor) = self.analysis_transfer_successors.get(&terminal).copied() {
                if !visited.insert(terminal) {
                    log::error!("analysis-transfer successor cycle during shutdown drain");
                    break;
                }
                terminal = successor;
            }
            terminal_identities.insert(terminal);
        }
        let mut tails = Vec::new();
        for identity in terminal_identities {
            if let Some(mut transfer) = self.analysis_transfers.remove(&identity)
                && let Some(tail) = transfer.routing_tail.take()
            {
                tails.push(tail);
            }
            self.analysis_transfers_consumed
                .insert(identity, Vec::new());
        }
        let deposited = {
            let mut lifecycle = self.library_replacement_lifecycle.lock();
            lifecycle.pending = None;
            lifecycle.reconcile_required = None;
            lifecycle.pre_seal.take()
        };
        if let Some(mut deposit) = deposited
            && let Some(tail) = LibraryRoutingTail::from_deposit(&mut deposit)
        {
            tails.push(tail);
        }
        let mut identities = Vec::new();
        for mut tail in tails {
            if let Some(post_seed) = tail.post_seed.take() {
                identities.push(post_seed.identity);
                identities.extend(post_seed.deferred_system_file);
            }
            identities.append(&mut tail.retired_post_seed_owners);
        }
        identities.extend(self.pending_post_seed_refresh_retry);
        identities.extend(self.pending_system_file_seed_retry);
        identities.sort_unstable_by_key(|identity| identity.seed_install_id);
        identities.dedup();
        for identity in identities {
            self.complete_post_seed_refresh_retry(identity);
            self.complete_system_file_seed_retry(identity);
        }
        self.pending_post_seed_outer_handles.clear();
        self.pending_post_seed_outer_candidates.clear();
        self.pending_post_seed_system_transfer = None;
        self.deferred_library_routing_build_notes.clear();
        retired_watcher
    }

    fn open_alias_candidates_for_uri(&self, uri: &Url) -> Vec<Url> {
        Self::resolve_open_alias_candidates(uri, &self.workspace_folders)
    }

    /// Resolve the bounded case/symlink alias set without borrowing state.
    ///
    /// Detached OpenInstall preparation calls this with a captured workspace
    /// root list so filesystem work never runs under the `WorldState` lock.
    pub(crate) fn resolve_open_alias_candidates(uri: &Url, workspace_folders: &[Url]) -> Vec<Url> {
        let Ok(path) = uri.to_file_path() else {
            return Vec::new();
        };

        let mut candidates = Vec::new();
        if let Some(case_path) = case_correct_open_path_for_workspaces(&path, workspace_folders)
            && let Ok(case_uri) = Url::from_file_path(case_path)
            && case_uri != *uri
        {
            candidates.push(case_uri);
        }
        if let Some(target_path) = symlink_target_open_path_for_workspaces(&path, workspace_folders)
            && let Ok(target_uri) = Url::from_file_path(target_path)
            && target_uri != *uri
        {
            candidates.push(target_uri);
        }

        let mut seen = HashSet::new();
        candidates.retain(|candidate| seen.insert(candidate.clone()));
        debug_assert!(candidates.len() <= Self::MAX_OPEN_ALIASES_PER_RECORD);
        candidates.truncate(Self::MAX_OPEN_ALIASES_PER_RECORD);
        candidates
    }

    fn register_open_document_aliases(&mut self, uri: &Url) -> Vec<Url> {
        let aliases = self.open_alias_candidates_for_uri(uri);
        self.register_prepared_open_document_aliases(uri, aliases.clone());
        aliases
    }

    fn register_prepared_open_document_aliases(&mut self, uri: &Url, aliases: Vec<Url>) {
        self.open_document_aliases.open(uri.clone(), aliases);
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
            .find(|open_uri| self.documents.contains_key(open_uri))
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
    /// The provider is a thin view over the open-document authority, the
    /// closed-file authority, and the non-authoritative raw-content memo.
    ///
    /// **Validates: Requirements 4.1, 13.1, 13.2**
    pub fn content_provider(&self) -> DefaultContentProvider<'_> {
        DefaultContentProvider::with_aliases(
            &self.documents,
            &self.workspace_index,
            &self.cross_file_file_cache,
            &self.open_document_aliases,
        )
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
        documents: &'a impl OpenDocumentsView,
    ) -> DefaultContentProvider<'a> {
        DefaultContentProvider::with_aliases(
            documents,
            &self.workspace_index,
            &self.cross_file_file_cache,
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
    /// protected from LRU eviction in `WorkspaceIndex` and
    /// `CrossFileWorkspaceIndex`, so closed-but-reachable documents survive
    /// across edits to other files and avoid the `compute_artifacts_with_metadata`
    /// recomputation fallback. Open documents live in the non-evictable
    /// open-document authority; closed neighbors live in the unified
    /// workspace index, whose two tiers share this pin set.
    ///
    /// Call after the open set changes (`did_open` / `did_close`) or after a
    /// dependency-graph edge change touches an open file.
    pub fn recompute_open_neighborhood_pins(&mut self) {
        #[cfg(any(test, feature = "test-support"))]
        {
            self.open_pin_recompute_count += 1;
        }
        let neighborhood = self.open_neighborhood_pins_for_graph(&self.cross_file_graph);
        self.workspace_index.set_pinned_uris(neighborhood);
    }

    /// Derive the exact pins that a prospective graph would require for the
    /// current open-document authority.
    fn open_neighborhood_pins_for_graph(&self, graph: &DependencyGraph) -> HashSet<Url> {
        let mut open_uris: Vec<Url> = self.documents.uris();
        let mut seen_open_roots: HashSet<Url> = open_uris.iter().cloned().collect();
        for open_uri in self.documents.uris() {
            for canonical_uri in self.canonical_uris_for_open_document(&open_uri) {
                if seen_open_roots.insert(canonical_uri.clone()) {
                    open_uris.push(canonical_uri);
                }
            }
        }
        if open_uris.is_empty() {
            return HashSet::new();
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

        graph.collect_neighborhood_multi(
            open_uris.iter().cloned(),
            max_depth,
            effective_max_visited,
        )
    }

    /// Resize all LRU caches based on configuration.
    /// Called after parsing initialization options.
    pub fn resize_caches(&mut self, config: &crate::cross_file::config::CrossFileConfig) {
        self.cross_file_meta
            .resize(config.cache_metadata_max_entries);
        self.cross_file_file_cache.resize(
            config.cache_file_content_max_entries,
            config.cache_existence_max_entries,
        );
        let evicted = self
            .workspace_index
            .resize_artifacts_with_evictions(config.cache_workspace_index_max_entries);
        if !evicted.is_empty() {
            self.refresh_tar_source_watch_parents(evicted);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open_document(&mut self, uri: Url, text: &str, version: Option<i32>) {
        let mut tar_watch_parents = vec![uri.clone()];
        tar_watch_parents.extend(self.canonical_uris_for_open_document(&uri));
        let aliases = self.register_open_document_aliases(&uri);
        tar_watch_parents.extend(aliases.iter().cloned());
        self.cross_file_file_cache.invalidate(&uri);
        for alias in aliases {
            self.cross_file_file_cache.invalidate(&alias);
        }
        self.documents
            .insert(uri.clone(), Document::new_with_uri(text, version, &uri));
        self.advance_open_context_authority_generation();
        self.refresh_tar_source_watch_registry(TarSourceWatchRegistryRefresh::Parents(
            tar_watch_parents,
        ));
    }

    /// Open one document authority with its already-enriched metadata.
    ///
    /// The mature [`Document`] constructor is the sole masking/parser/package
    /// derivation path; [`OpenDocumentStore`] derives metadata-dependent scope
    /// artifacts from that exact tree and analysis text. The CLI disk-fallback
    /// path uses this after completing the same metadata finalization order as
    /// workspace scanning.
    pub(crate) fn open_document_with_language_id_and_metadata(
        &mut self,
        uri: Url,
        text: &str,
        version: Option<i32>,
        language_id: Option<&str>,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
        lifecycle_epoch: Option<DiagnosticsEpoch>,
    ) -> Arc<OpenDocumentRecord> {
        let document = Document::new_with_language_id(text, version, &uri, language_id);
        self.install_open_document(uri, document, metadata, lifecycle_epoch)
    }

    fn install_open_document(
        &mut self,
        uri: Url,
        document: Document,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
        lifecycle_epoch: Option<DiagnosticsEpoch>,
    ) -> Arc<OpenDocumentRecord> {
        let mut tar_watch_parents = vec![uri.clone()];
        tar_watch_parents.extend(self.canonical_uris_for_open_document(&uri));
        let aliases = self.register_open_document_aliases(&uri);
        tar_watch_parents.extend(aliases.iter().cloned());
        self.cross_file_file_cache.invalidate(&uri);
        self.record_editor_chunk_kind_override(&uri, document.chunk_kind);
        for alias in aliases {
            self.cross_file_file_cache.invalidate(&alias);
        }
        let record = self
            .documents
            .open(uri, document, metadata, lifecycle_epoch);
        self.advance_open_context_authority_generation();
        self.refresh_tar_source_watch_registry(TarSourceWatchRegistryRefresh::Parents(
            tar_watch_parents,
        ));
        record
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
        let document = Document::new_with_language_id(text, version, &uri, language_id);
        let metadata = Arc::new(document.cross_file_metadata());
        let lifecycle_epoch = self.diagnostics_gate.current_epoch(&uri);
        self.install_open_document(uri, document, metadata, lifecycle_epoch);
    }

    pub fn close_document(&mut self, uri: &Url) -> Vec<Url> {
        self.cancel_open_install_intent(uri);
        let aliases = self.open_document_aliases.close(uri);
        let mut tar_watch_parents = vec![uri.clone()];
        tar_watch_parents.extend(aliases.iter().cloned());
        let removed_record = self.documents.close(uri).is_some();
        if removed_record || !aliases.is_empty() {
            self.advance_open_context_authority_generation();
        }
        if let Ok(mut cache) = self.effective_lint_config_cache.lock() {
            cache.remove(uri.as_str());
        }
        self.refresh_tar_source_watch_registry(TarSourceWatchRegistryRefresh::Parents(
            tar_watch_parents,
        ));
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
        let old = self.editor_chunk_kind_overrides.get(uri).copied();
        if uri.to_file_path().is_err() || chunk_kind == classify_chunk_document(uri.path()) {
            self.editor_chunk_kind_overrides.remove(uri);
        } else {
            self.editor_chunk_kind_overrides
                .insert(uri.clone(), chunk_kind);
        }
        if self.editor_chunk_kind_overrides.get(uri).copied() != old {
            self.chunk_override_generation.0 = self.chunk_override_generation.0.wrapping_add(1);
        }
    }

    /// Drop the persisted editor-derived chunk-kind override for `uri`.
    ///
    /// Call this from every path that removes a URI's closed cross-file state
    /// (deletion, final undecodable convergence, or workspace-index cleanup) so
    /// a later recreated file starts from path classification until the editor
    /// supplies a fresh language signal.
    pub fn prune_editor_chunk_kind_override(&mut self, uri: &Url) {
        if self.editor_chunk_kind_overrides.remove(uri).is_some() {
            self.chunk_override_generation.0 = self.chunk_override_generation.0.wrapping_add(1);
        }
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
        if file_type_from_uri(uri) == FileType::R {
            crate::cross_file::extract_metadata_for_kind(
                self.chunk_kind_for_closed_file(uri),
                content,
            )
        } else {
            crate::cross_file::CrossFileMetadata::default()
        }
    }

    /// Prepare one ordered LSP notification batch against the current record.
    ///
    /// The returned batch is not visible until [`Self::try_commit_analysis`]
    /// validates its exact basis. Preparation never invalidates the raw cache:
    /// rejected changes are strict no-ops.
    pub(crate) fn prepare_document_changes(
        &self,
        uri: &Url,
        changes: impl IntoIterator<Item = TextDocumentContentChangeEvent>,
        version: i32,
    ) -> Option<PreparedOpenEdit> {
        let basis = self.capture_open_analysis_basis(uri)?;
        let prepared = self.documents.prepare_changes(uri, changes, version)?;
        Some(PreparedOpenEdit {
            basis,
            uri: uri.clone(),
            prepared,
        })
    }

    fn capture_closed_analysis_basis(
        &self,
        subject: AnalysisSubjectBasis,
        uri: &Url,
    ) -> AnalysisBasis {
        let tar_source_watch_generations = self
            .tar_source_watch_paths_by_parent
            .get(uri)
            .into_iter()
            .flatten()
            .map(|path| {
                (
                    path.clone(),
                    self.tar_source_watch_path_generations
                        .get(path)
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect();
        AnalysisBasis {
            subject,
            watched_file_generation: self.watched_file_resync_generations.get(uri).copied(),
            tar_source_event_generation: self.tar_source_event_generation,
            tar_source_watch_generations,
            graph_revision: self.cross_file_graph.edge_revision(),
            graph_authority_generation: self.workspace_graph_authority_generation(),
            open_context_authority_generation: self.open_context_authority_generation,
            analysis_config_generation: self.analysis_config_generation,
            context_authorities: Vec::new(),
            batch_overlay_contexts: Vec::new(),
            open_transition: None,
            package_input_generation: self.package_input_generation(),
            package_config_generation: self.package_config_generation,
            system_file_routing: self.system_file_routing_stamp(),
            analysis_config: AnalysisConfigStamp {
                workspace_folders: self.workspace_folders.clone(),
                max_chain_depth: self.cross_file_config.max_chain_depth,
                max_forward_depth: self.cross_file_config.max_forward_depth,
                max_backward_depth: self.cross_file_config.max_backward_depth,
                on_demand_indexing_enabled: self.cross_file_config.on_demand_indexing_enabled,
                packages_enabled: self.cross_file_config.packages_enabled,
                revalidation_debounce_ms: self.cross_file_config.revalidation_debounce_ms,
                exclusion_patterns: self.workspace_exclusions.patterns().to_vec(),
                chunk_kind: self.chunk_kind_for_closed_file(uri),
            },
        }
    }

    /// Attach exact authority identities for closed metadata/content consumed
    /// while preparing inherited-WD and backward-parent projections.
    ///
    /// Open records are covered by the open-context generation. Closed index
    /// records use per-record ABA-safe tokens; raw-cache fallbacks use their
    /// exact snapshot. An over-budget preparation fails closed.
    pub(crate) fn attach_analysis_context_authorities(
        &self,
        basis: AnalysisBasis,
        uris: Vec<Url>,
    ) -> Option<AnalysisBasis> {
        // Use the same visited budget and absolute ceiling as cross-file
        // neighborhood traversal. Preparation must not carry more authority
        // identities than the traversal it supports could legally visit.
        let context_budget = self
            .cross_file_config
            .max_transitive_dependents_visited
            .min(Self::MULTI_SEED_VISITED_CEILING);
        self.attach_analysis_context_authorities_bounded(basis, uris, context_budget)
    }

    fn attach_analysis_context_authorities_bounded(
        &self,
        mut basis: AnalysisBasis,
        mut uris: Vec<Url>,
        limit: usize,
    ) -> Option<AnalysisBasis> {
        uris.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        uris.dedup();
        if uris.len() > limit {
            return None;
        }
        basis.context_authorities = uris
            .into_iter()
            .filter_map(|uri| {
                if self.open_document_uri_for_authoritative_uri(&uri).is_some() {
                    return None;
                }
                let token = self.workspace_index.closed_record_token(&uri);
                if self.workspace_index.closed_record_token_is_present(&token) {
                    Some(AnalysisContextAuthority::Closed(token))
                } else {
                    Some(AnalysisContextAuthority::Raw {
                        snapshot: self.cross_file_file_cache.get_snapshot(&uri),
                        uri,
                    })
                }
            })
            .collect();
        Some(basis)
    }

    pub(crate) fn capture_closed_pending_analysis_basis(
        &self,
        claim: EnrichmentClaim,
    ) -> AnalysisBasis {
        let uri = claim.uri().clone();
        self.capture_closed_analysis_basis(AnalysisSubjectBasis::Pending(claim), &uri)
    }

    pub(crate) fn capture_closed_refresh_analysis_basis(
        &self,
        token: CompleteRefreshToken,
    ) -> AnalysisBasis {
        let uri = token.uri().clone();
        self.capture_closed_analysis_basis(AnalysisSubjectBasis::Complete(token), &uri)
    }

    pub(crate) fn capture_closed_removal_analysis_basis(&self, uri: &Url) -> AnalysisBasis {
        self.capture_closed_analysis_basis(
            AnalysisSubjectBasis::Observed(self.workspace_index.closed_record_token(uri)),
            uri,
        )
    }

    pub(crate) fn capture_open_analysis_basis(&self, uri: &Url) -> Option<AnalysisBasis> {
        self.capture_open_transition_analysis_basis(
            AnalysisSubjectBasis::Open(self.documents.record_token(uri)),
            uri,
            self.open_alias_candidates_for_uri(uri),
        )
    }

    pub(crate) fn capture_open_alias_reconcile_basis(
        &self,
        uri: &Url,
        prospective_aliases: Vec<Url>,
    ) -> Option<AnalysisBasis> {
        let token = self.documents.record_token(uri);
        self.documents.record_token_is_current(&token).then(|| {
            self.capture_open_transition_analysis_basis(
                AnalysisSubjectBasis::Open(token),
                uri,
                prospective_aliases,
            )
        })?
    }

    /// Capture a detached install against one exact latest-arrival intent.
    ///
    /// `prospective_aliases` were resolved off-lock. This method binds their
    /// current in-state owners and raw-cache identities without performing
    /// filesystem work under the state lock.
    pub(crate) fn capture_open_install_analysis_basis(
        &self,
        intent: &OpenInstallIntentToken,
        prospective_aliases: Vec<Url>,
    ) -> Option<AnalysisBasis> {
        if !self.open_install_intent_is_current(intent) {
            return None;
        }
        self.capture_open_transition_analysis_basis(
            AnalysisSubjectBasis::OpenInstall(Box::new(OpenInstallSubjectBasis {
                intent: intent.clone(),
                target: intent.target.clone(),
            })),
            intent.uri(),
            prospective_aliases,
        )
    }

    pub(crate) fn capture_open_close_analysis_basis(
        &self,
        intent: &OpenCloseIntentToken,
    ) -> Option<AnalysisBasis> {
        if !self.open_close_intent_is_current(intent) {
            return None;
        }
        self.capture_open_transition_analysis_basis(
            AnalysisSubjectBasis::OpenClose(Box::new(OpenCloseSubjectBasis {
                intent: intent.clone(),
                target: intent.target.clone(),
            })),
            intent.uri(),
            self.canonical_uris_for_open_document(intent.uri()),
        )
    }

    fn capture_open_transition_analysis_basis(
        &self,
        subject: AnalysisSubjectBasis,
        uri: &Url,
        prospective_aliases: Vec<Url>,
    ) -> Option<AnalysisBasis> {
        let mut basis = self.capture_closed_analysis_basis(subject, uri);
        let mut raw_uris = vec![uri.clone()];
        raw_uris.extend(self.canonical_uris_for_open_document(uri));
        raw_uris.extend(prospective_aliases);
        raw_uris.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        raw_uris.dedup();
        // One target spelling plus the two registered and two prospective
        // aliases. Registered/prospective sets can overlap, hence `<=`.
        debug_assert!(raw_uris.len() <= 1 + 2 * Self::MAX_OPEN_ALIASES_PER_RECORD);
        let mut alias_owner_uris: Vec<Url> = raw_uris
            .iter()
            .filter_map(|candidate| self.open_document_uri_for_authoritative_uri(candidate))
            .collect();
        alias_owner_uris.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        alias_owner_uris.dedup();
        let alias_owner_tokens = alias_owner_uris
            .into_iter()
            .map(|owner| self.documents.record_token(&owner))
            .collect();
        let raw_authorities = raw_uris
            .into_iter()
            .map(|raw_uri| {
                let snapshot = self.cross_file_file_cache.get_snapshot(&raw_uri);
                (raw_uri, snapshot)
            })
            .collect();
        basis.open_transition = Some(OpenTransitionStamp {
            diagnostic_epoch: self.diagnostics_gate.current_epoch(uri),
            editor_eligibility_generation: self.editor_eligibility_generation,
            closed_index_version: self.workspace_index.version(),
            raw_cache_generation: self.cross_file_file_cache.content_generation(),
            alias_owner_tokens,
            raw_authorities,
        });
        Some(basis)
    }

    pub(crate) fn attach_open_edit_context_authorities(
        &self,
        edit: PreparedOpenEdit,
        uris: Vec<Url>,
    ) -> Result<PreparedOpenEdit, Box<PreparedOpenEdit>> {
        #[cfg(test)]
        if self.force_open_edit_overflow_for_test {
            return Err(Box::new(edit));
        }
        self.attach_open_edit_context_authorities_with_limit(
            edit,
            uris,
            Self::MULTI_SEED_VISITED_CEILING,
        )
    }

    fn attach_open_edit_context_authorities_with_limit(
        &self,
        mut edit: PreparedOpenEdit,
        mut uris: Vec<Url>,
        limit: usize,
    ) -> Result<PreparedOpenEdit, Box<PreparedOpenEdit>> {
        uris.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        uris.dedup();
        if uris.len() > limit {
            return Err(Box::new(edit));
        }
        edit.basis = self
            .attach_analysis_context_authorities_bounded(edit.basis, uris, limit)
            .expect("deduplicated open-edit context set was checked against its ceiling");
        Ok(edit)
    }

    pub(crate) fn open_edit_subject_is_current(&self, edit: &PreparedOpenEdit) -> bool {
        matches!(
            &edit.basis.subject,
            AnalysisSubjectBasis::Open(token)
                if token.uri() == &edit.uri && self.documents.record_token_is_current(token)
        )
    }

    pub(crate) fn open_edit_basis_is_current(&self, edit: &PreparedOpenEdit) -> bool {
        self.analysis_basis_is_current(&edit.basis, &edit.uri, &HashSet::new())
    }

    pub(crate) fn open_edit_analysis_basis_is_current(
        &self,
        edit: &PreparedOpenEditAnalysis,
    ) -> bool {
        self.analysis_basis_is_current(&edit.basis, &edit.uri, &HashSet::from([edit.uri.clone()]))
    }

    pub(crate) fn rebase_open_edit_if_subject_current(
        &self,
        mut edit: PreparedOpenEdit,
    ) -> Option<PreparedOpenEdit> {
        if !self.open_edit_subject_is_current(&edit) {
            return None;
        }
        edit.basis = self.capture_open_analysis_basis(&edit.uri)?;
        Some(edit)
    }

    #[cfg(test)]
    pub(crate) fn force_open_edit_context_overflow_for_test(
        &self,
        edit: PreparedOpenEdit,
        uris: Vec<Url>,
    ) -> PreparedOpenEdit {
        match self.attach_open_edit_context_authorities_with_limit(edit, uris, 0) {
            Ok(_) => panic!("non-empty test context must exceed the zero ceiling"),
            Err(edit) => *edit,
        }
    }

    pub(crate) fn capture_open_metadata_derivation(
        &self,
        uri: &Url,
        expected: AnalysisGeneration,
    ) -> Option<CapturedOpenMetadataAnalysis> {
        let record = self.documents.get_record(uri)?;
        if record.generation() != expected {
            return None;
        }
        let basis = self.capture_open_analysis_basis(uri)?;
        let graph_roots = self.authoritative_revalidation_roots_for_uri(uri);
        let mut metadata_map = HashMap::new();
        let mut content_map = HashMap::new();
        for open_uri in self.documents.uris() {
            if let Some(open_record) = self.documents.get_record(&open_uri) {
                metadata_map.insert(open_uri.clone(), open_record.metadata().clone());
                content_map.insert(open_uri.clone(), open_record.document().text());
                for canonical in self.canonical_uris_for_open_document(&open_uri) {
                    if self
                        .open_document_uri_for_authoritative_uri(&canonical)
                        .as_ref()
                        == Some(&open_uri)
                    {
                        metadata_map.insert(canonical.clone(), open_record.metadata().clone());
                        content_map.insert(canonical, open_record.document().text());
                    }
                }
            }
        }
        for closed_uri in self.workspace_index.artifact_uris() {
            if metadata_map.contains_key(&closed_uri) {
                continue;
            }
            if let Some(metadata) = self.workspace_index.get_metadata(&closed_uri) {
                metadata_map.insert(closed_uri.clone(), metadata);
            }
            if let Some(entry) = self.workspace_index.get(&closed_uri) {
                content_map.insert(closed_uri, entry.contents.to_string());
            }
        }
        let raw_entries = self.cross_file_file_cache.snapshot_entries();
        if metadata_map.len().saturating_add(raw_entries.len()) > Self::MULTI_SEED_VISITED_CEILING {
            return None;
        }
        let raw_content = raw_entries
            .into_iter()
            .map(|(raw_uri, _snapshot, content)| {
                let kind = self.chunk_kind_for_closed_file(&raw_uri);
                content_map
                    .entry(raw_uri.clone())
                    .or_insert_with(|| content.clone());
                (raw_uri, (content, kind))
            })
            .collect();
        let (workspace_name, package_workspace_root, library_paths) =
            self.snapshot_system_file_inputs();
        Some(CapturedOpenMetadataAnalysis {
            basis,
            uri: uri.clone(),
            expected,
            chunk_kind: record.document().chunk_kind,
            file_type: record.document().file_type,
            analysis_text: record.document().analysis_text(),
            old_metadata: record.metadata().clone(),
            workspace_root: self.workspace_folders.first().cloned(),
            max_chain_depth: self.cross_file_config.max_chain_depth,
            workspace_name,
            package_workspace_root,
            library_paths,
            exclusions: self.workspace_exclusions.clone(),
            graph_roots,
            _graph: self.cross_file_graph.clone(),
            metadata_map,
            content_map,
            raw_content,
        })
    }

    pub(crate) fn prepare_captured_open_metadata_analysis(
        &self,
        captured: CapturedOpenMetadataAnalysis,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
        plan: PreparedOpenCommitPlan,
        context_uris: Vec<Url>,
    ) -> Option<PreparedOpenMetadataAnalysis> {
        // Detached derivation may only validate the authority identities
        // captured with its snapshot. Do not resample per-URI authorities
        // here: the snapshot basis already owns global closed-index,
        // raw-cache, and open-context generations that cover every lookup.
        if context_uris.len() > Self::MULTI_SEED_VISITED_CEILING {
            return None;
        }
        Some(PreparedOpenMetadataAnalysis::new(
            captured.basis,
            captured.uri,
            captured.expected,
            metadata,
            plan,
        ))
    }

    fn analysis_basis_is_current(
        &self,
        basis: &AnalysisBasis,
        uri: &Url,
        batch_targets: &HashSet<Url>,
    ) -> bool {
        let subject_current = match &basis.subject {
            AnalysisSubjectBasis::Pending(claim) => {
                claim.uri() == uri && self.workspace_index.enrichment_claim_is_current(claim)
            }
            AnalysisSubjectBasis::Complete(token) => {
                token.uri() == uri && self.workspace_index.complete_refresh_is_current(token)
            }
            AnalysisSubjectBasis::Observed(token) => {
                token.uri() == uri && self.workspace_index.closed_record_token_is_current(token)
            }
            AnalysisSubjectBasis::Open(token) => {
                token.uri() == uri && self.documents.record_token_is_current(token)
            }
            AnalysisSubjectBasis::OpenInstall(install) => {
                install.intent.uri() == uri
                    && self.open_install_intent_is_current(&install.intent)
                    && self.documents.record_token_is_current(&install.target)
            }
            AnalysisSubjectBasis::OpenClose(close) => {
                close.intent.uri() == uri
                    && self.open_close_intent_is_current(&close.intent)
                    && self.documents.record_token_is_current(&close.target)
            }
        };
        let closed_subject = !matches!(
            &basis.subject,
            AnalysisSubjectBasis::Open(_)
                | AnalysisSubjectBasis::OpenInstall(_)
                | AnalysisSubjectBasis::OpenClose(_)
        );
        if !subject_current
            || (closed_subject && self.is_document_open_or_alias(uri))
            || self.watched_file_resync_generations.get(uri).copied()
                != basis.watched_file_generation
            || self.tar_source_event_generation != basis.tar_source_event_generation
            || basis
                .tar_source_watch_generations
                .iter()
                .any(|(path, generation)| {
                    self.tar_source_watch_path_generations.get(path).copied() != Some(*generation)
                })
            || self.cross_file_graph.edge_revision() != basis.graph_revision
            || self.workspace_graph_authority_generation() != basis.graph_authority_generation
            || self.open_context_authority_generation != basis.open_context_authority_generation
            || self.analysis_config_generation != basis.analysis_config_generation
            || self.package_input_generation() != basis.package_input_generation
            || self.package_config_generation != basis.package_config_generation
        {
            return false;
        }
        if let Some(open) = &basis.open_transition
            && (self.diagnostics_gate.current_epoch(uri) != open.diagnostic_epoch
                || self.editor_eligibility_generation != open.editor_eligibility_generation
                || self.workspace_index.version() != open.closed_index_version
                || self.cross_file_file_cache.content_generation() != open.raw_cache_generation
                || open
                    .alias_owner_tokens
                    .iter()
                    .any(|token| !self.documents.record_token_is_current(token))
                || open.raw_authorities.iter().any(|(raw_uri, snapshot)| {
                    self.cross_file_file_cache.get_snapshot(raw_uri) != *snapshot
                }))
        {
            return false;
        }
        if basis
            .batch_overlay_contexts
            .iter()
            .any(|context| !batch_targets.contains(context))
        {
            return false;
        }
        if basis
            .context_authorities
            .iter()
            .any(|authority| match authority {
                _ if basis
                    .batch_overlay_contexts
                    .iter()
                    .any(|context| context == authority.uri()) =>
                {
                    false
                }
                AnalysisContextAuthority::Closed(token) => {
                    !self.workspace_index.closed_record_token_is_current(token)
                }
                AnalysisContextAuthority::Raw { uri, snapshot } => {
                    self.cross_file_file_cache.get_snapshot(uri) != *snapshot
                }
            })
        {
            return false;
        }
        basis.system_file_routing == self.system_file_routing_stamp()
            && basis.analysis_config
                == (AnalysisConfigStamp {
                    workspace_folders: self.workspace_folders.clone(),
                    max_chain_depth: self.cross_file_config.max_chain_depth,
                    max_forward_depth: self.cross_file_config.max_forward_depth,
                    max_backward_depth: self.cross_file_config.max_backward_depth,
                    on_demand_indexing_enabled: self.cross_file_config.on_demand_indexing_enabled,
                    packages_enabled: self.cross_file_config.packages_enabled,
                    revalidation_debounce_ms: self.cross_file_config.revalidation_debounce_ms,
                    exclusion_patterns: self.workspace_exclusions.patterns().to_vec(),
                    chunk_kind: self.chunk_kind_for_closed_file(uri),
                })
    }

    fn reserve_analysis_revalidations(&mut self, mut affected: Vec<Url>) -> AnalysisCommitEffects {
        // One shared activity-aware cap is applied after every transfer and
        // collect-only candidate has been unioned. URI order breaks equal
        // activity scores deterministically; cap-dropped URIs are never marked.
        affected.sort_by(|left, right| {
            self.cross_file_activity
                .priority_score(left)
                .cmp(&self.cross_file_activity.priority_score(right))
                .then_with(|| left.as_str().cmp(right.as_str()))
        });
        affected.dedup();
        affected.truncate(self.cross_file_config.max_revalidations_per_trigger);
        #[cfg(any(test, feature = "test-support"))]
        {
            self.analysis_revalidation_reservation_count += affected.len();
        }
        self.diagnostics_gate
            .mark_force_republish_many(affected.iter());
        AnalysisCommitEffects {
            revalidations: affected
                .into_iter()
                .map(|uri| AnalysisRevalidationTicket {
                    trigger: DiagnosticsTrigger::capture(self, &uri),
                    debounce_ms: self.cross_file_config.revalidation_debounce_ms,
                    uri,
                })
                .collect(),
            affected_candidates: Vec::new(),
            open: None,
            close: None,
            workspace_scan: None,
            system_file: None,
            package_routing: None,
        }
    }

    fn reserve_analysis_transfer_candidates(
        &mut self,
        candidates: Vec<AnalysisTransferCandidate>,
    ) -> AnalysisCommitEffects {
        let candidates = self.current_transfer_candidates(candidates);
        let mut candidates_by_uri: HashMap<Url, AnalysisTransferCandidate> =
            HashMap::with_capacity(candidates.len());
        for candidate in candidates {
            match candidates_by_uri.entry(candidate.uri.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(candidate);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if matches!(
                        candidate.reservation,
                        AnalysisTransferReservationPolicy::Subject { .. }
                    ) && matches!(
                        entry.get().reservation,
                        AnalysisTransferReservationPolicy::Dependent
                    ) {
                        entry.insert(candidate);
                    }
                }
            }
        }
        let mut candidates: Vec<_> = candidates_by_uri.into_values().collect();
        candidates.sort_by(|left, right| {
            let left_priority = match left.reservation {
                AnalysisTransferReservationPolicy::Subject { .. } => 0,
                AnalysisTransferReservationPolicy::Dependent => self
                    .cross_file_activity
                    .priority_score(&left.uri)
                    .saturating_add(1),
            };
            let right_priority = match right.reservation {
                AnalysisTransferReservationPolicy::Subject { .. } => 0,
                AnalysisTransferReservationPolicy::Dependent => self
                    .cross_file_activity
                    .priority_score(&right.uri)
                    .saturating_add(1),
            };
            left_priority
                .cmp(&right_priority)
                .then_with(|| left.uri.as_str().cmp(right.uri.as_str()))
        });
        candidates.truncate(self.cross_file_config.max_revalidations_per_trigger);
        #[cfg(any(test, feature = "test-support"))]
        {
            self.analysis_revalidation_reservation_count += candidates.len();
        }
        self.diagnostics_gate.mark_force_republish_many(
            candidates
                .iter()
                .filter(|candidate| {
                    candidate.reservation == AnalysisTransferReservationPolicy::Dependent
                })
                .map(|candidate| &candidate.uri),
        );
        AnalysisCommitEffects {
            revalidations: candidates
                .into_iter()
                .map(|candidate| AnalysisRevalidationTicket {
                    uri: candidate.uri,
                    trigger: candidate.trigger,
                    debounce_ms: match candidate.reservation {
                        AnalysisTransferReservationPolicy::Subject { debounce_ms } => debounce_ms,
                        AnalysisTransferReservationPolicy::Dependent => {
                            self.cross_file_config.revalidation_debounce_ms
                        }
                    },
                })
                .collect(),
            affected_candidates: Vec::new(),
            open: None,
            close: None,
            workspace_scan: None,
            system_file: None,
            package_routing: None,
        }
    }

    fn extend_tar_watch_open_plan_parents(parents: &mut Vec<Url>, plan: &PreparedOpenCommitPlan) {
        parents.extend(plan.reset_closed_roots.iter().cloned());
        parents.extend(plan.retire_closed_roots.iter().cloned());
        parents.extend(
            plan.close_disk_installs
                .iter()
                .map(|install| install.uri.clone()),
        );
    }

    fn tar_watch_refresh_for_system_file(
        prepared: &PreparedSystemFileAnalysis,
    ) -> TarSourceWatchRegistryRefresh {
        let mut parents = prepared
            .index
            .as_ref()
            .into_iter()
            .flat_map(|index| index.changed_uris().iter().cloned())
            .collect::<Vec<_>>();
        parents.extend(
            prepared
                .open_metadata
                .iter()
                .map(|replacement| replacement.uri.clone()),
        );
        TarSourceWatchRegistryRefresh::Parents(parents)
    }

    /// Capture every parent whose authoritative open/closed record may change
    /// before a prepared commit consumes or rewrites its transition inputs.
    fn tar_watch_refresh_for_prepared(
        &self,
        prepared: &PreparedAnalysisCommit,
    ) -> TarSourceWatchRegistryRefresh {
        let mut parents = Vec::new();
        match prepared {
            PreparedAnalysisCommit::WorkspaceScan(_) => {
                return TarSourceWatchRegistryRefresh::Full;
            }
            PreparedAnalysisCommit::SystemFile(prepared) => {
                return Self::tar_watch_refresh_for_system_file(prepared);
            }
            PreparedAnalysisCommit::Upsert(prepared) => {
                parents.push(prepared.uri.clone());
            }
            PreparedAnalysisCommit::Remove { uri, .. } => {
                parents.push(uri.clone());
            }
            PreparedAnalysisCommit::WatchedBatch(prepared) => {
                parents.extend(prepared.mutations.iter().map(|mutation| match mutation {
                    PreparedClosedMutation::Upsert(prepared) => prepared.uri.clone(),
                    PreparedClosedMutation::Remove { uri, .. } => uri.clone(),
                }));
            }
            PreparedAnalysisCommit::OpenInstall(prepared) => {
                parents.push(prepared.uri.clone());
                parents.extend(self.canonical_uris_for_open_document(&prepared.uri));
                parents.extend(prepared.aliases.iter().cloned());
                Self::extend_tar_watch_open_plan_parents(&mut parents, &prepared.plan);
            }
            PreparedAnalysisCommit::OpenEdit(prepared) => {
                parents.push(prepared.uri.clone());
                Self::extend_tar_watch_open_plan_parents(&mut parents, &prepared.plan);
            }
            PreparedAnalysisCommit::OpenMetadata(prepared) => {
                parents.push(prepared.uri.clone());
                Self::extend_tar_watch_open_plan_parents(&mut parents, &prepared.plan);
            }
            PreparedAnalysisCommit::OpenAliasReconcile(prepared) => {
                parents.push(prepared.uri.clone());
                // Capture the old aliases before registration replaces them.
                parents.extend(self.canonical_uris_for_open_document(&prepared.uri));
                parents.extend(prepared.aliases.iter().cloned());
                Self::extend_tar_watch_open_plan_parents(&mut parents, &prepared.plan);
            }
            PreparedAnalysisCommit::OpenClose(prepared) => {
                parents.push(prepared.uri.clone());
                parents.extend(self.canonical_uris_for_open_document(&prepared.uri));
                parents.extend(prepared.expected_aliases.iter().cloned());
                Self::extend_tar_watch_open_plan_parents(&mut parents, &prepared.plan);
            }
        }
        TarSourceWatchRegistryRefresh::Parents(parents)
    }

    /// Validate and commit one prepared analysis transaction while the caller
    /// holds the sole `WorldState` write lock.
    ///
    /// Rejection leaves analysis authorities unchanged, except that an exact
    /// still-current Pending lease is released for retry. On success the
    /// record, graph projection, raw-content memo, authority generations,
    /// pins, and revalidation reservation are updated before immutable tickets
    /// are returned.
    pub(crate) fn try_commit_analysis(
        &mut self,
        prepared: PreparedAnalysisCommit,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        let mut tar_watch_refresh = self.tar_watch_refresh_for_prepared(&prepared);
        let result = match prepared {
            PreparedAnalysisCommit::WorkspaceScan(prepared) => {
                self.try_commit_workspace_scan(*prepared)
            }
            PreparedAnalysisCommit::SystemFile(prepared) => self.try_commit_system_file(*prepared),
            PreparedAnalysisCommit::OpenClose(prepared) => {
                self.try_commit_open_close(*prepared, &mut tar_watch_refresh)
            }
            PreparedAnalysisCommit::OpenInstall(prepared) => {
                self.try_commit_open_install(*prepared)
            }
            PreparedAnalysisCommit::OpenEdit(prepared) => self.try_commit_open_edit(*prepared),
            PreparedAnalysisCommit::OpenMetadata(prepared) => {
                self.try_commit_open_metadata(*prepared)
            }
            PreparedAnalysisCommit::OpenAliasReconcile(prepared) => {
                self.try_commit_open_alias_reconcile(*prepared)
            }
            PreparedAnalysisCommit::Upsert(prepared) => self.try_commit_closed_batch(
                vec![PreparedClosedMutation::Upsert(prepared)],
                None,
                true,
                false,
                &mut tar_watch_refresh,
            ),
            PreparedAnalysisCommit::Remove { basis, uri } => self.try_commit_closed_batch(
                vec![PreparedClosedMutation::Remove { basis, uri }],
                None,
                true,
                false,
                &mut tar_watch_refresh,
            ),
            PreparedAnalysisCommit::WatchedBatch(prepared) => {
                self.try_commit_watched_batch(*prepared, &mut tar_watch_refresh)
            }
        };
        if result.is_ok() {
            self.refresh_tar_source_watch_registry(tar_watch_refresh);
        }
        result
    }

    fn try_commit_watched_batch(
        &mut self,
        prepared: PreparedWatchedBatchAnalysis,
        tar_watch_refresh: &mut TarSourceWatchRegistryRefresh,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        if prepared
            .watched_generations
            .iter()
            .any(|(uri, generation)| {
                self.watched_file_resync_generations.get(uri).copied() != Some(*generation)
            })
            || prepared
                .package
                .as_ref()
                .is_some_and(|(basis, _)| !self.package_projection_basis_is_current(basis))
            || (prepared.package.is_some()
                && (self.documents.keys().count() != prepared.package_open_records.len()
                    || prepared
                        .package_open_records
                        .values()
                        .any(|token| !self.documents.record_token_is_current(token))))
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        self.try_commit_closed_batch(
            prepared.mutations,
            prepared.package,
            false,
            prepared.durable_package_handoff,
            tar_watch_refresh,
        )
    }

    /// Atomically install one complete workspace-scan projection.
    ///
    /// Every authority and every open target is preflighted before the index
    /// lock is acquired. After `replace_all_complete` succeeds, the remaining
    /// operations are infallible in-memory swaps under the same `WorldState`
    /// write lock. No diagnostics marker or worker is created here; exact
    /// post-commit open tokens are transferred for one later claim after
    /// package/config convergence.
    fn try_commit_workspace_scan(
        &mut self,
        mut prepared: PreparedWorkspaceScanAnalysis,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        if !self.workspace_scan_input_basis_is_current(&prepared.input)
            || !self.workspace_scan_derivation_basis_is_current(&prepared.basis)
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }

        prepared
            .open_metadata
            .sort_unstable_by(|left, right| left.uri.as_str().cmp(right.uri.as_str()));
        if prepared
            .open_metadata
            .windows(2)
            .any(|pair| pair[0].uri == pair[1].uri)
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        let expected_targets: Vec<_> = prepared.basis.open_records.keys().cloned().collect();
        let actual_targets: Vec<_> = prepared
            .open_metadata
            .iter()
            .map(|replacement| replacement.uri.clone())
            .collect();
        if actual_targets != expected_targets
            || prepared.open_metadata.iter().any(|replacement| {
                prepared.basis.open_records.get(&replacement.uri) != Some(&replacement.token)
                    || !self.documents.record_token_is_current(&replacement.token)
                    || !self.documents.generation_is_current(
                        &replacement.uri,
                        replacement.prepared.base_generation(),
                    )
            })
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }

        let replaced = self
            .workspace_index
            .replace_all_complete_if_current(
                prepared.basis.workspace_index_version,
                prepared.artifact_only,
                prepared.full_records,
                prepared.workspace_index_pins,
            )
            .map_err(|_| AnalysisCommitRejected::StaleBasis)?;
        if !replaced {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        // The input basis was validated under this same WorldState write lock.
        // Advance directly after the index CAS; unlike `claim_*`, this cannot
        // fail after the first mutation.
        debug_assert_eq!(
            self.workspace_scan_generation,
            prepared.input.scan_generation
        );
        self.advance_workspace_scan_generation();

        for replacement in prepared.open_metadata {
            self.documents
                .commit_prepared_metadata_if_current(&replacement.uri, replacement.prepared)
                .expect("all workspace-scan open targets were prevalidated");
        }
        self.cross_file_graph = prepared.graph;
        self.workspace_scan_complete = true;
        self.advance_workspace_graph_authority_generation();
        self.advance_open_context_authority_generation();
        self.workspace_scan_intent = Some(WorkspaceScanIntentState::Committed(
            prepared.input.intent.generation,
        ));

        let identity = WorkspaceScanTransferIdentity {
            intent_generation: prepared.input.intent.generation,
            commit_generation: Self::mint_workspace_scan_commit_generation(),
            committed_scan_generation: self.workspace_scan_generation,
        };
        let candidate_uris: Vec<_> = self
            .documents
            .keys()
            .filter(|uri| self.diagnostics_publish_allowed(uri))
            .cloned()
            .collect();
        let candidates = self.capture_analysis_transfer_candidates(candidate_uris);
        let identity = AnalysisTransferIdentity::WorkspaceScan(identity);
        let handle = self.install_analysis_transfer(
            identity,
            self.latest_workspace_scan_transfer,
            candidates,
        );
        self.latest_workspace_scan_transfer = Some(identity);

        Ok(AnalysisCommitEffects {
            revalidations: Vec::new(),
            affected_candidates: Vec::new(),
            open: None,
            close: None,
            workspace_scan: Some(WorkspaceScanTransferredEffects { handle }),
            system_file: None,
            package_routing: None,
        })
    }

    /// Claim a committed scan's unmarked fanout exactly once.
    ///
    /// Candidates whose exact open records were replaced, closed, or reopened
    /// after the scan are dropped; their owning transition is responsible for
    /// current diagnostics. Marking and trigger capture happen together under
    /// the caller's state write lock.
    #[cfg(test)]
    pub(crate) fn claim_workspace_scan_transfer(
        &mut self,
        transferred: &WorkspaceScanTransferredEffects,
    ) -> Vec<AnalysisRevalidationTicket> {
        let finalization = Self::begin_analysis_transfer_finalization();
        match self.finalize_analysis_transfers(finalization, &[transferred.handle], Vec::new()) {
            Ok(AnalysisTransferFinalization::Committed(tickets)) => tickets,
            Ok(AnalysisTransferFinalization::AlreadyFinalized) | Err(_) => Vec::new(),
        }
    }

    fn system_file_analysis_basis_is_current(&self, basis: &SystemFileAnalysisBasis) -> bool {
        basis.routing == self.system_file_routing_stamp()
            && self.system_file_analysis_non_routing_basis_is_current(basis)
    }

    fn system_file_analysis_non_routing_basis_is_current(
        &self,
        basis: &SystemFileAnalysisBasis,
    ) -> bool {
        if self.routing_shutdown.load(Ordering::Acquire) {
            return false;
        }
        let index = self.workspace_index.authority_snapshot();
        let open_records: std::collections::BTreeMap<_, _> = self
            .documents
            .keys()
            .map(|uri| (uri.clone(), self.documents.record_token(uri)))
            .collect();
        index.version == basis.workspace_index_version
            && self.tar_source_event_generation == basis.tar_source_event_generation
            && self.workspace_index.config().max_files == basis.workspace_index_max_files
            && self.workspace_index.config().max_file_size_bytes
                == basis.workspace_index_max_file_size_bytes
            && index.artifact_capacity_limit == basis.workspace_index_artifact_capacity
            && index.pinned == basis.workspace_index_pinned
            && self.cross_file_graph.edge_revision() == basis.graph_revision
            && self.workspace_graph_authority_generation == basis.graph_authority_generation
            && self.open_context_authority_generation == basis.open_context_authority_generation
            && self.analysis_config_generation == basis.analysis_config_generation
            && self.chunk_override_generation == basis.chunk_override_generation
            && self.workspace_folders == basis.workspace_folders
            && self.workspace_exclusions.patterns() == basis.exclusion_patterns.as_slice()
            && self.cross_file_config.max_chain_depth == basis.max_chain_depth
            && open_records == basis.open_records
    }

    fn try_commit_system_file(
        &mut self,
        prepared: PreparedSystemFileAnalysis,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        if !self.system_file_analysis_basis_is_current(&prepared.basis)
            || prepared.open_metadata.iter().any(|replacement| {
                prepared.basis.open_records.get(&replacement.uri) != Some(&replacement.token)
                    || !self.documents.record_token_is_current(&replacement.token)
                    || !self.documents.generation_is_current(
                        &replacement.uri,
                        replacement.prepared.base_generation(),
                    )
            })
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        if let Some(index) = prepared.index
            && !self
                .workspace_index
                .commit_prepared_targeted_batch(index)
                .map_err(|_| AnalysisCommitRejected::StaleBasis)?
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }

        let open_changed = !prepared.open_metadata.is_empty();
        for replacement in prepared.open_metadata {
            self.documents
                .commit_prepared_metadata_if_current(&replacement.uri, replacement.prepared)
                .expect("all system.file open targets were prevalidated");
        }
        if open_changed {
            self.advance_open_context_authority_generation();
        }
        let graph_changed = !prepared.changed_uris.is_empty();
        if graph_changed {
            self.cross_file_graph = prepared.graph;
            self.advance_workspace_graph_authority_generation();
            self.advance_workspace_scan_generation();
            self.recompute_open_neighborhood_pins();
        }

        let changed_uris = prepared.changed_uris;
        let candidate_uris = self
            .system_file_republish_set_with_content(&changed_uris, &prepared.content_changed_uris);
        let candidates = self.capture_analysis_transfer_candidates(
            candidate_uris
                .into_iter()
                .filter(|uri| self.diagnostics_publish_allowed(uri)),
        );
        let identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: prepared.basis.routing.owner,
            commit_generation: Self::mint_system_file_commit_generation(),
        });
        let handle =
            self.install_analysis_transfer(identity, self.latest_system_file_transfer, candidates);
        self.latest_system_file_transfer = Some(identity);

        Ok(AnalysisCommitEffects {
            revalidations: Vec::new(),
            affected_candidates: Vec::new(),
            open: None,
            close: None,
            workspace_scan: None,
            system_file: Some(SystemFileTransferredEffects {
                handle,
                changed_uris,
            }),
            package_routing: None,
        })
    }

    /// Atomically publish one package-library successor with the complete
    /// `system.file()` index/open/graph projection derived for its prospective
    /// routing identity.
    ///
    /// The caller holds the old library's exclusive routing lease. Production
    /// cache helpers never wait on that lease while holding `WorldState`, so
    /// acquiring this state write lock after the lease is the single fair,
    /// deadlock-free finalization order.
    ///
    /// `prepared.watcher` must be a clone of a driver-owned install: a rejected
    /// attempt may drop this per-attempt clone under the state lock, while the
    /// driver retains the last prospective OS-handle `Arc` until after the
    /// guard is released.
    pub(crate) fn try_commit_library_routing(
        &mut self,
        prepared: PreparedLibraryRoutingAnalysis,
        lease: &PackageLibraryRoutingLease<'_>,
        mut replacement_guard: Option<&mut PendingLibraryReplacementGuard>,
        pre_seal: &mut Option<LibraryRoutingPreSealDeposit>,
        mut delivery: Option<&mut crate::libpath_watcher::LibpathJournalDelivery>,
    ) -> Result<
        (
            LibraryRoutingTransferredEffects,
            Option<Arc<crate::libpath_watcher::LibpathWatcherHandle>>,
        ),
        AnalysisCommitRejected,
    > {
        let tar_watch_refresh = Self::tar_watch_refresh_for_system_file(&prepared.system_file);
        let PreparedLibraryRoutingAnalysis {
            basis,
            prospective,
            library,
            ready,
            warm_basis,
            system_file,
            watcher,
        } = prepared;
        let warm_basis_is_valid = warm_basis
            .as_ref()
            .is_some_and(|basis| self.open_package_warm_basis_is_current(basis, &library));
        let watcher_shape_is_valid = match basis.mutation {
            LibraryRoutingMutation::Changed
            | LibraryRoutingMutation::FullRescan
            | LibraryRoutingMutation::DegradedReconcile => {
                matches!(watcher, PreparedLibpathWatcherInstall::Keep)
            }
            LibraryRoutingMutation::Replacement | LibraryRoutingMutation::Dropped => {
                !matches!(watcher, PreparedLibpathWatcherInstall::Keep)
                    && watcher.is_buffering_active()
            }
        };
        let delivery_is_valid = match basis.mutation {
            LibraryRoutingMutation::Replacement => delivery.is_none(),
            LibraryRoutingMutation::Changed => delivery.as_deref().is_some_and(|delivery| {
                matches!(
                    delivery.event(),
                    crate::libpath_watcher::LibpathEvent::Changed { .. }
                ) && self
                    .libpath_watcher
                    .active_journal()
                    .is_some_and(|journal| Arc::ptr_eq(journal, delivery.journal()))
            }),
            LibraryRoutingMutation::FullRescan => delivery.as_deref().is_some_and(|delivery| {
                matches!(
                    delivery.event(),
                    crate::libpath_watcher::LibpathEvent::Rescan
                ) && self
                    .libpath_watcher
                    .active_journal()
                    .is_some_and(|journal| Arc::ptr_eq(journal, delivery.journal()))
            }),
            LibraryRoutingMutation::DegradedReconcile => {
                delivery.is_none()
                    && matches!(
                        self.libpath_watcher,
                        LibpathWatcherState::Degraded {
                            reconcile_pending: true
                        }
                    )
            }
            LibraryRoutingMutation::Dropped => delivery.as_deref().is_some_and(|delivery| {
                matches!(
                    delivery.event(),
                    crate::libpath_watcher::LibpathEvent::Dropped
                ) && self
                    .libpath_watcher
                    .active_journal()
                    .is_some_and(|journal| Arc::ptr_eq(journal, delivery.journal()))
            }),
        };
        let requires_full_warm = matches!(
            basis.mutation,
            LibraryRoutingMutation::Replacement
                | LibraryRoutingMutation::FullRescan
                | LibraryRoutingMutation::DegradedReconcile
                | LibraryRoutingMutation::Dropped
        );
        if !self.library_routing_basis_is_current(&basis, lease)
            || !watcher_shape_is_valid
            || !delivery_is_valid
            || (requires_full_warm && !warm_basis_is_valid)
            || (!requires_full_warm && warm_basis.as_ref().is_some_and(|_| !warm_basis_is_valid))
            || system_file.basis.routing != prospective.routing
            || !self.system_file_analysis_non_routing_basis_is_current(&system_file.basis)
            || !system_file.external_observations_are_current()
            || system_file.open_metadata.iter().any(|replacement| {
                system_file.basis.open_records.get(&replacement.uri) != Some(&replacement.token)
                    || !self.documents.record_token_is_current(&replacement.token)
                    || !self.documents.generation_is_current(
                        &replacement.uri,
                        replacement.prepared.base_generation(),
                    )
            })
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        #[cfg(test)]
        {
            self.library_routing_test_commit_attempts += 1;
            if self.library_routing_test_reject_remaining > 0 {
                self.library_routing_test_reject_remaining -= 1;
                return Err(AnalysisCommitRejected::StaleBasis);
            }
        }
        if let Some(index) = system_file.index
            && !self
                .workspace_index
                .commit_prepared_targeted_batch(index)
                .map_err(|_| AnalysisCommitRejected::StaleBasis)?
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        if let Some(delivery) = delivery.as_mut() {
            delivery.ack();
        }

        basis.library.retire(lease);
        self.package_library = library;
        self.package_library_install_id = prospective.install_id;
        self.package_library_content_generation = prospective.content_generation;
        self.system_file_routing_owner_generation = prospective.routing_owner.0;
        self.package_library_ready = ready;
        self.refresh_local_dev_overlay();
        let watcher_owner_changed = prospective.watcher_owner != self.libpath_watcher_owner();
        let mut restart_owner = None;
        let retired_handle = match watcher {
            PreparedLibpathWatcherInstall::Keep => {
                debug_assert!(matches!(
                    basis.mutation,
                    LibraryRoutingMutation::Changed
                        | LibraryRoutingMutation::FullRescan
                        | LibraryRoutingMutation::DegradedReconcile
                ));
                if basis.mutation == LibraryRoutingMutation::DegradedReconcile {
                    self.libpath_watcher = LibpathWatcherState::Degraded {
                        reconcile_pending: false,
                    };
                }
                None
            }
            PreparedLibpathWatcherInstall::Active {
                handle,
                journal,
                recovery,
            } => {
                debug_assert_ne!(basis.mutation, LibraryRoutingMutation::Changed);
                let retired = self.libpath_watcher.retire();
                assert!(
                    journal.try_activate(),
                    "library-routing CAS must activate an exact buffering journal"
                );
                self.libpath_watcher_owner_generation = prospective.watcher_owner.0;
                self.libpath_watcher = LibpathWatcherState::Active {
                    handle: Some(handle),
                    journal,
                    is_recovery: recovery,
                    applied: LibpathWatcherSpec {
                        paths: self.package_library.lib_paths().to_vec(),
                        debounce_ms: basis.packages_watch_debounce_ms,
                    },
                };
                retired
            }
            PreparedLibpathWatcherInstall::Disabled => {
                debug_assert_ne!(basis.mutation, LibraryRoutingMutation::Changed);
                let retired = self.libpath_watcher.retire();
                self.libpath_watcher_owner_generation = prospective.watcher_owner.0;
                retired
            }
            PreparedLibpathWatcherInstall::AttachFailed {
                recovery,
                can_recover,
            } => {
                debug_assert_ne!(basis.mutation, LibraryRoutingMutation::Changed);
                let retired = self.libpath_watcher.retire();
                self.libpath_watcher_owner_generation = prospective.watcher_owner.0;
                if recovery {
                    self.libpath_watcher = LibpathWatcherState::Degraded {
                        reconcile_pending: false,
                    };
                } else if can_recover {
                    self.libpath_watcher = LibpathWatcherState::AwaitingRecovery;
                    restart_owner = Some(prospective.watcher_owner);
                } else {
                    self.libpath_watcher = LibpathWatcherState::Degraded {
                        reconcile_pending: false,
                    };
                }
                retired
            }
        };
        let finalized_pre_seal = if basis.mutation == LibraryRoutingMutation::Replacement {
            let mut lifecycle = self.library_replacement_lifecycle.lock();
            debug_assert_eq!(lifecycle.pending, basis.replacement_intent);
            let guard = replacement_guard
                .as_mut()
                .expect("a replacement CAS must own its synchronous lifecycle guard");
            debug_assert!(Arc::ptr_eq(
                &guard.lifecycle,
                &self.library_replacement_lifecycle
            ));
            debug_assert_eq!(Some(guard.intent), basis.replacement_intent);
            let mut adopted = pre_seal
                .take()
                .expect("replacement CAS retains its adopted pre-seal bundle");
            if let Some(late) = lifecycle.pre_seal.take() {
                adopted.merge(late);
            }
            // Any reconcile request installed by a late depositor is
            // satisfied by this same atomic adoption.
            lifecycle.reconcile_required = None;
            lifecycle.pending = None;
            guard.armed = false;
            drop(lifecycle);
            self.bump_package_config_generation();
            Some(adopted)
        } else {
            debug_assert!(replacement_guard.is_none());
            None
        };

        let open_changed = !system_file.open_metadata.is_empty();
        for replacement in system_file.open_metadata {
            self.documents
                .commit_prepared_metadata_if_current(&replacement.uri, replacement.prepared)
                .expect("all library-routing open targets were prevalidated");
        }
        if open_changed {
            self.advance_open_context_authority_generation();
        }
        if !system_file.changed_uris.is_empty() {
            self.cross_file_graph = system_file.graph;
            self.advance_workspace_graph_authority_generation();
            self.advance_workspace_scan_generation();
            self.recompute_open_neighborhood_pins();
        }

        let changed_uris = system_file.changed_uris;
        let candidate_uris = match basis.mutation {
            LibraryRoutingMutation::Replacement
            | LibraryRoutingMutation::FullRescan
            | LibraryRoutingMutation::DegradedReconcile
            | LibraryRoutingMutation::Dropped => self.documents.keys().cloned().collect(),
            LibraryRoutingMutation::Changed => self.system_file_republish_set_with_content(
                &changed_uris,
                &system_file.content_changed_uris,
            ),
        };
        let candidates = self.capture_analysis_transfer_candidates(
            candidate_uris
                .into_iter()
                .filter(|uri| self.diagnostics_publish_allowed(uri)),
        );
        let identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: prospective.routing_owner,
            commit_generation: Self::mint_system_file_commit_generation(),
        });
        let handle = if let Some(deposit) = finalized_pre_seal {
            self.install_library_routing_transfer(
                identity,
                self.latest_system_file_transfer,
                candidates,
                deposit,
            )
        } else {
            self.install_analysis_transfer(identity, self.latest_system_file_transfer, candidates)
        };
        self.latest_system_file_transfer = Some(identity);
        if watcher_owner_changed {
            // Full replacement and Dropped commits publish watcher ownership
            // directly rather than through the watcher-only swap CAS. They
            // must still wake a terminal degraded root parked on the old
            // owner.
            notify_library_routing_reconcile_edge(
                &self.library_routing_reconcile_wake,
                &self.library_routing_reconcile_wake_generation,
            );
        }
        self.refresh_tar_source_watch_registry(tar_watch_refresh);

        Ok((
            LibraryRoutingTransferredEffects {
                handle,
                changed_uris,
                restart_owner,
            },
            retired_handle,
        ))
    }

    fn try_commit_open_close(
        &mut self,
        mut prepared: PreparedOpenCloseAnalysis,
        tar_watch_refresh: &mut TarSourceWatchRegistryRefresh,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        let targets = HashSet::from([prepared.uri.clone()]);
        if !self.analysis_basis_is_current(&prepared.basis, &prepared.uri, &targets)
            || !self.open_close_intent_is_current(&prepared.intent)
            || self.canonical_uris_for_open_document(&prepared.uri) != prepared.expected_aliases
            || prepared.watched_roots.iter().any(|(root, expected, _)| {
                self.watched_file_resync_generations.get(root).copied() != *expected
            })
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }

        let old_interface = self
            .documents
            .get_record(&prepared.uri)
            .map(|record| record.artifacts().interface_hash);
        self.advance_workspace_scan_generation();
        self.retire_diagnostic_lifecycle(&prepared.uri);
        self.cross_file_activity.remove(&prepared.uri);
        let closed_aliases = self.open_document_aliases.close(&prepared.uri);
        debug_assert_eq!(closed_aliases, prepared.expected_aliases);
        self.documents.close(&prepared.uri);
        if let Ok(mut cache) = self.effective_lint_config_cache.lock() {
            cache.remove(prepared.uri.as_str());
        }
        let package_routing_owner = prepared
            .package
            .and_then(|package| self.install_prepared_package_projection(package));

        for install in prepared.plan.close_disk_installs.drain(..) {
            let (_, evicted) = self
                .workspace_index
                .install_complete_preserving_provenance(install.uri.clone(), install.entry);
            tar_watch_refresh.push_parent(evicted);
            self.cross_file_file_cache
                .insert(install.uri, install.snapshot, install.content);
        }
        let plan_effects = self.apply_open_commit_plan(
            &prepared.uri,
            old_interface,
            prepared.plan,
            package_routing_owner.is_some(),
        );
        let package_routing_owner = plan_effects.package_routing_owner.or(package_routing_owner);
        // Claim disk-watcher ownership after the prepared graph/index plan:
        // Remove dispositions prune old claims and chunk overrides as part of
        // their cleanup, so claiming earlier would erase the close's own token.
        for (root, _, chunk_kind) in &prepared.watched_roots {
            self.record_editor_chunk_kind_override(root, *chunk_kind);
            self.watched_file_resync_generation_counter =
                self.watched_file_resync_generation_counter.wrapping_add(1);
            self.watched_file_resync_generations
                .insert(root.clone(), self.watched_file_resync_generation_counter);
        }
        for ticket in &mut prepared.resync {
            ticket.expected_watched_generation = self
                .watched_file_resync_generations
                .get(&ticket.uri)
                .copied();
        }
        self.consume_open_close_intent(&prepared.intent);
        self.advance_workspace_graph_authority_generation();
        self.advance_open_context_authority_generation();
        Ok(AnalysisCommitEffects {
            revalidations: plan_effects.revalidations,
            affected_candidates: Vec::new(),
            open: None,
            close: Some(OpenCloseCommitOutcome {
                resync: prepared.resync,
            }),
            workspace_scan: None,
            system_file: None,
            package_routing: package_routing_owner.map(|owner| PackageRoutingCommitEffects {
                owner,
                candidates: plan_effects.transfer_candidates,
                handoff: None,
            }),
        })
    }

    fn try_commit_open_install(
        &mut self,
        prepared: PreparedOpenInstallAnalysis,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        let targets = HashSet::from([prepared.uri.clone()]);
        if !self.analysis_basis_is_current(&prepared.basis, &prepared.uri, &targets)
            || !self.open_install_intent_is_current(&prepared.intent)
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }

        let raw_uris: Vec<Url> = prepared
            .basis
            .open_transition
            .as_ref()
            .expect("open install carries its transition authority")
            .raw_authorities
            .iter()
            .map(|(uri, _)| uri.clone())
            .collect();
        let old_interface = self
            .documents
            .get_record(&prepared.uri)
            .map(|record| record.artifacts().interface_hash)
            .or_else(|| {
                self.workspace_index
                    .get_artifacts(&prepared.uri)
                    .map(|artifacts| artifacts.interface_hash)
            });

        // Validation above precedes epoch creation. Everything below is an
        // infallible in-memory replacement while the caller owns the
        // diagnostics publish lock and WorldState write lock.
        let lifecycle_epoch = self.begin_diagnostic_lifecycle(&prepared.uri);
        self.register_prepared_open_document_aliases(&prepared.uri, prepared.aliases);
        for raw_uri in raw_uris {
            self.cross_file_file_cache.invalidate(&raw_uri);
        }
        self.record_editor_chunk_kind_override(&prepared.uri, prepared.document.chunk_kind);
        let committed = self.documents.open_prepared(
            prepared.uri.clone(),
            prepared.document,
            prepared.metadata,
            prepared.artifacts,
            lifecycle_epoch,
        );
        let package_routing_owner = prepared
            .package
            .and_then(|package| self.install_prepared_package_projection(package));
        let packages_to_prefetch = prepared.plan.packages_to_prefetch.clone();
        let plan_effects = self.apply_open_commit_plan(
            &prepared.uri,
            old_interface,
            prepared.plan,
            package_routing_owner.is_some(),
        );
        let package_routing_owner = plan_effects.package_routing_owner.or(package_routing_owner);
        self.consume_open_install_intent(&prepared.intent);
        self.advance_workspace_graph_authority_generation();
        self.advance_open_context_authority_generation();
        Ok(AnalysisCommitEffects {
            revalidations: plan_effects.revalidations,
            affected_candidates: Vec::new(),
            open: Some(OpenAnalysisCommitOutcome {
                generation: committed.generation(),
                provenance: committed.provenance(),
                packages_to_prefetch,
            }),
            close: None,
            workspace_scan: None,
            system_file: None,
            package_routing: package_routing_owner.map(|owner| PackageRoutingCommitEffects {
                owner,
                candidates: plan_effects.transfer_candidates,
                handoff: None,
            }),
        })
    }

    fn try_commit_open_edit(
        &mut self,
        prepared: PreparedOpenEditAnalysis,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        let targets = HashSet::from([prepared.uri.clone()]);
        if !self.analysis_basis_is_current(&prepared.basis, &prepared.uri, &targets) {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        let raw_uris: Vec<Url> = prepared
            .basis
            .open_transition
            .as_ref()
            .expect("open edit carries its transition authority")
            .raw_authorities
            .iter()
            .map(|(uri, _)| uri.clone())
            .collect();
        let old_interface = self
            .documents
            .get_record(&prepared.uri)
            .map(|record| record.artifacts().interface_hash);
        let committed = self
            .documents
            .commit_prepared_if_current(&prepared.uri, prepared.prepared, prepared.metadata)
            .map_err(|_| AnalysisCommitRejected::StaleBasis)?;
        for raw_uri in raw_uris {
            self.cross_file_file_cache.invalidate(&raw_uri);
        }
        let package_routing_owner = prepared
            .package
            .and_then(|package| self.install_prepared_package_projection(package));
        let packages_to_prefetch = prepared.plan.packages_to_prefetch.clone();
        let plan_effects = self.apply_open_commit_plan(
            &prepared.uri,
            old_interface,
            prepared.plan,
            package_routing_owner.is_some(),
        );
        let package_routing_owner = plan_effects.package_routing_owner.or(package_routing_owner);
        self.advance_workspace_graph_authority_generation();
        self.advance_open_context_authority_generation();
        Ok(AnalysisCommitEffects {
            revalidations: plan_effects.revalidations,
            affected_candidates: Vec::new(),
            open: Some(OpenAnalysisCommitOutcome {
                generation: committed.generation(),
                provenance: committed.provenance(),
                packages_to_prefetch,
            }),
            close: None,
            workspace_scan: None,
            system_file: None,
            package_routing: package_routing_owner.map(|owner| PackageRoutingCommitEffects {
                owner,
                candidates: plan_effects.transfer_candidates,
                handoff: None,
            }),
        })
    }

    fn try_commit_open_metadata(
        &mut self,
        prepared: PreparedOpenMetadataAnalysis,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        let targets = HashSet::from([prepared.uri.clone()]);
        if !self.analysis_basis_is_current(&prepared.basis, &prepared.uri, &targets) {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        if !self
            .documents
            .generation_is_current(&prepared.uri, prepared.expected)
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        let old_interface = self
            .documents
            .get_record(&prepared.uri)
            .map(|record| record.artifacts().interface_hash);
        let committed = self
            .documents
            .replace_metadata_if_current(&prepared.uri, prepared.expected, prepared.metadata)
            .map_err(|_| AnalysisCommitRejected::StaleBasis)?;
        let packages_to_prefetch = prepared.plan.packages_to_prefetch.clone();
        let revalidations = self
            .apply_open_commit_plan(&prepared.uri, old_interface, prepared.plan, false)
            .revalidations;
        self.advance_workspace_graph_authority_generation();
        self.advance_open_context_authority_generation();
        Ok(AnalysisCommitEffects {
            revalidations,
            affected_candidates: Vec::new(),
            open: Some(OpenAnalysisCommitOutcome {
                generation: committed.generation(),
                provenance: committed.provenance(),
                packages_to_prefetch,
            }),
            close: None,
            workspace_scan: None,
            system_file: None,
            package_routing: None,
        })
    }

    fn try_commit_open_alias_reconcile(
        &mut self,
        prepared: PreparedOpenAliasReconcileAnalysis,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        let targets = HashSet::from([prepared.uri.clone()]);
        if !self.analysis_basis_is_current(&prepared.basis, &prepared.uri, &targets)
            || !self
                .documents
                .generation_is_current(&prepared.uri, prepared.expected)
        {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        let raw_uris: Vec<Url> = prepared
            .basis
            .open_transition
            .as_ref()
            .expect("alias reconcile carries transition authority")
            .raw_authorities
            .iter()
            .map(|(uri, _)| uri.clone())
            .collect();
        let old_interface = self
            .documents
            .get_record(&prepared.uri)
            .map(|record| record.artifacts().interface_hash);
        self.register_prepared_open_document_aliases(&prepared.uri, prepared.aliases);
        for raw_uri in raw_uris {
            self.cross_file_file_cache.invalidate(&raw_uri);
        }
        let revalidations = self
            .apply_open_commit_plan(&prepared.uri, old_interface, prepared.plan, false)
            .revalidations;
        self.advance_workspace_graph_authority_generation();
        self.advance_open_context_authority_generation();
        let record = self
            .documents
            .get_record(&prepared.uri)
            .expect("validated open record remains installed");
        Ok(AnalysisCommitEffects {
            revalidations,
            affected_candidates: Vec::new(),
            open: Some(OpenAnalysisCommitOutcome {
                generation: record.generation(),
                provenance: record.provenance(),
                packages_to_prefetch: Vec::new(),
            }),
            close: None,
            workspace_scan: None,
            system_file: None,
            package_routing: None,
        })
    }

    fn apply_open_commit_plan(
        &mut self,
        uri: &Url,
        old_interface: Option<u64>,
        plan: PreparedOpenCommitPlan,
        defer_reservation: bool,
    ) -> OpenCommitPlanEffects {
        let closing_subject = plan.closing_subject;
        let replacement_interface_hash = plan.replacement_interface_hash;
        // A close can change several authoritative spellings at once: the
        // raw subject, canonical alias roots that are remirrored, and
        // canonical roots that are reset or retired. Collect their
        // dependents both before and after applying the graph plan. The
        // pre-walk retains removed/existing incoming edges; the post-walk
        // discovers edges introduced by a surviving alias (notably a new
        // backward directive). Both sets flow through the single cap below.
        let changed_authoritative_roots = if closing_subject {
            let mut roots: Vec<Url> = plan
                .graph
                .iter()
                .map(|projection| projection.uri.clone())
                .chain(plan.reset_closed_roots.iter().cloned())
                .chain(plan.retire_closed_roots.iter().cloned())
                .collect();
            roots.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
            roots.dedup();
            roots
        } else {
            Vec::new()
        };
        let close_pre_graph_neighbors: Vec<Url> = changed_authoritative_roots
            .iter()
            .flat_map(|root| self.affected_open_dependents_after_edit(root, true, true))
            .collect();
        // Preserve the pre-update neighborhood only when the prepared graph
        // inputs can change routing. This keeps private/body-only edits on the
        // selective fast path while still retaining a removed edge's endpoint,
        // which is no longer discoverable from the post-update graph.
        let capture_pre_graph = !plan.reset_closed_roots.is_empty()
            || !plan.retire_closed_roots.is_empty()
            || plan.graph.iter().any(|projection| {
                let Some(old) = projection.old_metadata.as_deref() else {
                    return true;
                };
                let new = projection.new_metadata.as_ref();
                old.sources != new.sources
                    || old.sourced_by != new.sourced_by
                    || old.tarchetypes_document_links != new.tarchetypes_document_links
                    || old.working_directory != new.working_directory
                    || old.inherited_working_directory != new.inherited_working_directory
            });
        let pre_graph_neighbors = if capture_pre_graph {
            self.affected_open_dependents_after_edit(uri, true, true)
        } else {
            Vec::new()
        };
        for root in &plan.reset_closed_roots {
            self.workspace_index.invalidate(root);
            self.cross_file_graph.remove_file(root);
            self.cross_file_file_cache.invalidate(root);
            self.cross_file_meta.remove(root);
            self.prune_editor_chunk_kind_override(root);
            self.watched_file_resync_generations.remove(root);
        }
        for root in &plan.retire_closed_roots {
            self.cross_file_file_cache.invalidate(root);
            self.cross_file_meta.remove(root);
            self.prune_editor_chunk_kind_override(root);
            self.watched_file_resync_generations.remove(root);
        }

        let mut edges_changed = false;
        let mut wd_affected = Vec::new();
        let workspace_root = self.workspace_folders.first().cloned();
        for projection in plan.graph {
            let result = self.cross_file_graph.update_file(
                &projection.uri,
                projection.graph_metadata.as_ref(),
                workspace_root.as_ref(),
                |parent_uri| projection.parent_content.get(parent_uri).cloned(),
            );
            edges_changed |= result.edges_changed;
            if projection.make_non_lending {
                edges_changed |= self
                    .cross_file_graph
                    .make_forward_edges_non_lending(&projection.uri);
            }
            wd_affected.extend(
                crate::cross_file::revalidation::invalidate_children_on_parent_wd_change(
                    &projection.uri,
                    projection.old_metadata.as_deref(),
                    projection.new_metadata.as_ref(),
                    &self.cross_file_graph,
                    &self.cross_file_meta,
                ),
            );
        }
        let close_post_graph_neighbors: Vec<Url> = changed_authoritative_roots
            .iter()
            .flat_map(|root| self.affected_open_dependents_after_edit(root, true, true))
            .collect();

        let mut package_visibility_changed = false;
        let mut package_routing_owner = None;
        if let Some((event_uri, text)) = plan.package_event {
            let old_namespace = self.package_state.namespace_model().cloned();
            let old_contribution = self.package_state.scope_contribution().clone();
            let event = crate::package_state::event::HandlerEvent::DidChange {
                uri: event_uri,
                text,
            };
            if let Some(delta) =
                crate::package_state::event::translate(&mut self.package_inputs, event)
            {
                self.record_package_input_mutation();
                package_routing_owner = self.apply_package_event_with_routing_policy(
                    &delta,
                    PackageRoutingOwnerPolicy::IfChanged,
                );
                package_visibility_changed = self.package_state.namespace_model()
                    != old_namespace.as_ref()
                    || self.package_state.scope_contribution() != &old_contribution;
            }
        }

        let new_interface = if closing_subject {
            replacement_interface_hash
        } else {
            self.documents
                .get_record(uri)
                .map(|record| record.artifacts().interface_hash)
        };
        let interface_changed = old_interface != new_interface;
        let mut affected: HashSet<Url> = plan.seed_revalidation_uris.into_iter().collect();
        affected.extend(close_pre_graph_neighbors);
        affected.extend(close_post_graph_neighbors);
        if plan.direct_subject_publish || closing_subject {
            affected.remove(uri);
        } else {
            affected.insert(uri.clone());
        }
        if interface_changed || edges_changed {
            affected.extend(pre_graph_neighbors);
            affected.extend(self.affected_open_dependents_after_edit(
                uri,
                interface_changed,
                edges_changed,
            ));
        }
        for child in wd_affected {
            if let Some(open_child) = self.open_document_uri_for_authoritative_uri(&child) {
                affected.insert(open_child);
            }
        }
        if package_visibility_changed || (interface_changed && plan.package_source_interface_fanout)
        {
            affected.extend(plan.package_fanout_uris);
        }

        if edges_changed || plan.refresh_pins {
            self.recompute_open_neighborhood_pins();
        }

        let mut affected: Vec<Url> = affected.into_iter().collect();
        if defer_reservation || package_routing_owner.is_some() {
            let subject_debounce_ms = plan
                .subject_debounce_ms
                .unwrap_or(self.cross_file_config.edited_file_debounce_ms);
            let mut transfer_candidates = self.capture_analysis_transfer_candidates(affected);
            if let Some(subject) = transfer_candidates
                .iter_mut()
                .find(|candidate| candidate.uri == *uri)
            {
                subject.reservation = AnalysisTransferReservationPolicy::Subject {
                    debounce_ms: subject_debounce_ms,
                };
            }
            return OpenCommitPlanEffects {
                transfer_candidates,
                package_routing_owner,
                ..OpenCommitPlanEffects::default()
            };
        }
        affected.sort_by_cached_key(|candidate| {
            if candidate == uri {
                0
            } else {
                self.cross_file_activity
                    .priority_score(candidate)
                    .saturating_add(1)
            }
        });
        affected.truncate(self.cross_file_config.max_revalidations_per_trigger);
        #[cfg(any(test, feature = "test-support"))]
        {
            self.analysis_revalidation_reservation_count += affected.len();
        }
        self.diagnostics_gate
            .mark_force_republish_many(affected.iter().filter(|candidate| *candidate != uri));
        let revalidations = affected
            .into_iter()
            .filter(|affected_uri| !closing_subject || affected_uri != uri)
            .map(|affected_uri| AnalysisRevalidationTicket {
                debounce_ms: if affected_uri == *uri {
                    plan.subject_debounce_ms
                        .unwrap_or(self.cross_file_config.edited_file_debounce_ms)
                } else {
                    self.cross_file_config.revalidation_debounce_ms
                },
                trigger: DiagnosticsTrigger::capture(self, &affected_uri),
                uri: affected_uri,
            })
            .collect();
        OpenCommitPlanEffects {
            revalidations,
            transfer_candidates: Vec::new(),
            package_routing_owner,
        }
    }

    fn try_commit_closed_batch(
        &mut self,
        mutations: Vec<PreparedClosedMutation>,
        package: Option<(PackageProjectionBasis, PreparedPackageProjection)>,
        reserve_closed_fanout: bool,
        durable_package_handoff: bool,
        tar_watch_refresh: &mut TarSourceWatchRegistryRefresh,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        let mut targets = HashSet::with_capacity(mutations.len());
        let no_duplicates = mutations.iter().all(|mutation| {
            let uri = match mutation {
                PreparedClosedMutation::Upsert(prepared) => &prepared.uri,
                PreparedClosedMutation::Remove { uri, .. } => uri,
            };
            targets.insert(uri.clone())
        });
        let all_current = no_duplicates
            && mutations.iter().all(|mutation| {
                let (basis, uri) = match mutation {
                    PreparedClosedMutation::Upsert(prepared) => (&prepared.basis, &prepared.uri),
                    PreparedClosedMutation::Remove { basis, uri } => (basis.as_ref(), uri),
                };
                self.analysis_basis_is_current(basis, uri, &targets)
                    && match mutation {
                        PreparedClosedMutation::Upsert(prepared) => self
                            .cross_file_file_cache
                            .get_snapshot(uri)
                            .is_none_or(|existing| existing.mtime <= prepared.snapshot.mtime),
                        PreparedClosedMutation::Remove { .. } => true,
                    }
            });
        if !all_current {
            // Pending is a lease over an otherwise absent slot. A rejected
            // detached computation releases only the exact still-current
            // leases in this rejected batch; replaced claims remain untouched.
            for mutation in &mutations {
                let PreparedClosedMutation::Upsert(prepared) = mutation else {
                    continue;
                };
                let AnalysisSubjectBasis::Pending(claim) = &prepared.basis.subject else {
                    continue;
                };
                if self.workspace_index.enrichment_claim_is_current(claim) {
                    self.workspace_index
                        .abort_enrichment(claim)
                        .expect("current Pending lease aborts under the state write lock");
                }
            }
            return Err(AnalysisCommitRejected::StaleBasis);
        }

        // Removal fanout must be derived from the old graph. Collect every
        // target before applying the first mutation so a multi-file batch is
        // all-or-none both for authority mutation and fanout ownership.
        let removal_affected: Vec<Url> = mutations
            .iter()
            .filter_map(|mutation| match mutation {
                PreparedClosedMutation::Remove { uri, .. } => Some(uri),
                PreparedClosedMutation::Upsert(_) => None,
            })
            .flat_map(|uri| self.affected_open_dependents_after_edit(uri, true, false))
            .collect();
        let mut affected = if reserve_closed_fanout {
            removal_affected.clone()
        } else {
            Vec::new()
        };
        let mut affected_candidates = if reserve_closed_fanout {
            Vec::new()
        } else {
            removal_affected
        };
        let mut post_fanout = Vec::new();
        let mut graph_changed = false;
        for mutation in mutations {
            match mutation {
                PreparedClosedMutation::Upsert(prepared) => {
                    let prepared = *prepared;
                    let old_interface = self
                        .workspace_index
                        .get_artifacts(&prepared.uri)
                        .map(|artifacts| artifacts.interface_hash);
                    let new_interface = Some(prepared.entry.artifacts.interface_hash);
                    let reserve_fanout = reserve_closed_fanout
                        && matches!(&prepared.basis.subject, AnalysisSubjectBasis::Observed(_));
                    let report_unmarked_fanout = !reserve_closed_fanout
                        || matches!(&prepared.basis.subject, AnalysisSubjectBasis::Pending(_));
                    let (committed, evicted) = match &prepared.basis.subject {
                        AnalysisSubjectBasis::Pending(claim) => self
                            .workspace_index
                            .commit_enrichment_with_eviction(claim, prepared.entry)
                            .map(|(_, evicted)| (true, evicted))
                            .unwrap_or((false, None)),
                        AnalysisSubjectBasis::Complete(token) => self
                            .workspace_index
                            .commit_complete_refresh_with_eviction(token, prepared.entry)
                            .map(|(_, evicted)| (true, evicted))
                            .unwrap_or((false, None)),
                        AnalysisSubjectBasis::Observed(_) => {
                            let (_, evicted) =
                                self.workspace_index.install_complete_preserving_provenance(
                                    prepared.uri.clone(),
                                    prepared.entry,
                                );
                            (true, evicted)
                        }
                        AnalysisSubjectBasis::Open(_)
                        | AnalysisSubjectBasis::OpenInstall(_)
                        | AnalysisSubjectBasis::OpenClose(_) => {
                            unreachable!("open subjects never enter the closed commit path")
                        }
                    };
                    tar_watch_refresh.push_parent(evicted);
                    debug_assert!(committed, "prevalidated closed CAS remains current");

                    let result = self.cross_file_graph.update_file(
                        &prepared.uri,
                        prepared.graph_metadata.as_ref(),
                        prepared.workspace_root.as_ref(),
                        |parent_uri| prepared.parent_content.get(parent_uri).cloned(),
                    );
                    let mut edges_changed = result.edges_changed;
                    for projection in prepared.additional_graph {
                        let result = self.cross_file_graph.update_file(
                            &projection.uri,
                            projection.metadata.as_ref(),
                            prepared.workspace_root.as_ref(),
                            |parent_uri| projection.parent_content.get(parent_uri).cloned(),
                        );
                        edges_changed |= result.edges_changed;
                        if projection.make_non_lending {
                            edges_changed |= self
                                .cross_file_graph
                                .make_forward_edges_non_lending(&projection.uri);
                        }
                    }
                    self.cross_file_file_cache.insert(
                        prepared.uri.clone(),
                        prepared.snapshot,
                        prepared.content,
                    );
                    self.cross_file_meta.invalidate_many(&prepared.wd_children);
                    if reserve_fanout || report_unmarked_fanout {
                        let wd_open = prepared.wd_children.iter().filter_map(|child| {
                            self.open_document_uri_for_authoritative_uri(child)
                        });
                        if reserve_fanout {
                            affected.extend(wd_open);
                        } else {
                            affected_candidates.extend(wd_open);
                        }
                        post_fanout.push((
                            prepared.uri,
                            old_interface != new_interface,
                            edges_changed,
                            reserve_fanout,
                        ));
                    }
                    graph_changed |= edges_changed;
                }
                PreparedClosedMutation::Remove { basis: _, uri } => {
                    self.workspace_index.invalidate(&uri);
                    self.cross_file_graph.remove_file(&uri);
                    self.cross_file_file_cache.invalidate(&uri);
                    self.cross_file_meta.remove(&uri);
                    self.prune_editor_chunk_kind_override(&uri);
                    self.watched_file_resync_generations.remove(&uri);
                    graph_changed = true;
                }
            }
        }
        for (uri, interface_changed, edges_changed, reserve_fanout) in post_fanout {
            if interface_changed || edges_changed {
                let fanout = self.affected_open_dependents_after_edit(
                    &uri,
                    interface_changed,
                    edges_changed,
                );
                if reserve_fanout {
                    affected.extend(fanout);
                } else {
                    affected_candidates.extend(fanout);
                }
            }
        }
        if !targets.is_empty() {
            self.advance_workspace_scan_generation();
        }
        if graph_changed {
            self.advance_workspace_graph_authority_generation();
            self.recompute_open_neighborhood_pins();
        }
        affected_candidates.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        affected_candidates.dedup();
        let package_routing_owner =
            package.and_then(|(_, prepared)| self.install_prepared_package_projection(prepared));
        let mut effects = self.reserve_analysis_revalidations(affected);
        effects.affected_candidates = affected_candidates;
        effects.package_routing = package_routing_owner.map(|owner| {
            let (candidates, handoff) = if durable_package_handoff {
                let candidates =
                    self.capture_analysis_transfer_candidates(self.documents.keys().cloned());
                let identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
                    routing_owner: owner,
                    commit_generation: Self::mint_system_file_commit_generation(),
                });
                let handoff = self.install_analysis_transfer(
                    identity,
                    self.latest_system_file_transfer,
                    candidates.clone(),
                );
                self.latest_system_file_transfer = Some(identity);
                (candidates, Some(handoff))
            } else {
                (Vec::new(), None)
            };
            PackageRoutingCommitEffects {
                owner,
                candidates,
                handoff,
            }
        });
        Ok(effects)
    }

    /// Install a prepared document plus its enriched metadata if still current.
    #[cfg(test)]
    pub(crate) fn commit_document_changes(
        &mut self,
        uri: &Url,
        prepared: PreparedOpenEdit,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
    ) -> Result<Arc<OpenDocumentRecord>, AnalysisCommitRejected> {
        if prepared.uri != *uri {
            return Err(AnalysisCommitRejected::StaleBasis);
        }
        self.try_commit_analysis(PreparedAnalysisCommit::OpenEdit(Box::new(
            PreparedOpenEditAnalysis::new(prepared, metadata, PreparedOpenCommitPlan::default()),
        )))?;
        self.documents
            .get_record(uri)
            .cloned()
            .ok_or(AnalysisCommitRejected::StaleBasis)
    }

    /// Guarded replacement for metadata/artifacts derived off the current text.
    pub(crate) fn replace_open_document_metadata_if_current(
        &mut self,
        uri: &Url,
        generation: AnalysisGeneration,
        metadata: Arc<crate::cross_file::CrossFileMetadata>,
    ) -> Result<Arc<OpenDocumentRecord>, AnalysisCommitRejected> {
        let basis = self
            .capture_open_analysis_basis(uri)
            .ok_or(AnalysisCommitRejected::StaleBasis)?;
        self.try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(
            PreparedOpenMetadataAnalysis::new(
                basis,
                uri.clone(),
                generation,
                metadata,
                PreparedOpenCommitPlan::default(),
            ),
        )))?;
        self.documents
            .get_record(uri)
            .cloned()
            .ok_or(AnalysisCommitRejected::StaleBasis)
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
            // real directive); Stan uses its own geometry-preserving directive
            // mask and contributes no R metadata.
            return Some(Arc::new(doc.cross_file_metadata()));
        }
        if let Some(meta) = self.workspace_index.get_metadata(uri) {
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
    /// 1. open-document authority (open documents with enriched metadata)
    /// 2. WorkspaceIndex (closed-document authority)
    /// 3. File cache (re-extract metadata)
    ///
    /// Rmd/Quarto note (issue #343): every tier here is masked-correct for
    /// open R Markdown / Quarto documents. The open-document authority arm (tier 1)
    /// stores metadata extracted from the masked analysis text at
    /// `did_open`/`did_change` time (the open-document authority derives
    /// artifacts from the same masked text); the file-cache arm likewise masks
    /// via `extract_metadata_for_uri`. The state-aware file
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
                self.documents
                    .get_record(&open_uri)
                    .map(|record| record.metadata().clone())
                    .or_else(|| {
                        self.documents
                            .get(&open_uri)
                            .map(|doc| Arc::new(doc.cross_file_metadata()))
                    })
            })
            .or_else(|| self.workspace_index.get_metadata(uri))
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
    /// Package-mode state is not set from index parameters. The existing
    /// semantic package state remains live until the caller atomically installs
    /// a fresh package-input seed and derives its replacement. This matters when
    /// a detached seed is invalidated by a concurrent watcher update: index
    /// application must not leave package semantics temporarily reset or empty
    /// while the seed recomputes off-lock.
    ///
    /// Package-input freshness is owned separately by
    /// `package_input_lifecycle`, so applying an index neither advances nor
    /// resets the generation captured by a seed computed for this application.
    ///
    /// **Validates: Requirements 11.1, 13.1**
    pub fn apply_workspace_index(
        &mut self,
        entries: HashMap<Url, crate::workspace_index::IndexEntry>,
    ) {
        let generation = self.workspace_scan_generation();
        let records = entries
            .into_iter()
            .map(|(uri, entry)| {
                (
                    uri,
                    entry,
                    crate::workspace_index::ClosedProvenance::WorkspaceScan { generation },
                )
            })
            .collect();
        self.workspace_index
            .replace_all_complete(Vec::new(), records, HashSet::new())
            .expect("workspace index lock poisoned");

        log::info!("Applied {} workspace files", self.workspace_index.len());

        // Build the dependency graph from all workspace entries so that
        // forward-created backward edges are available for auto-detect mode.
        self.build_dependency_graph_from_workspace();
        self.rebuild_tar_source_watch_registry();
        self.workspace_scan_complete = true;
        log::info!(
            "[Background] Dependency graph built from workspace entries, workspace_scan_complete = true"
        );

        // Now that the graph reflects the workspace, refresh the closed-index
        // pin set so any file opened before the scan completes picks up its
        // neighborhood.
        self.recompute_open_neighborhood_pins();
    }

    /// Test-fixture seam for a closed document in the unified index.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_workspace_document_for_test(&mut self, uri: Url, document: Document) {
        let snapshot = crate::cross_file::file_cache::FileSnapshot {
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size: document.contents.len_bytes() as u64,
            content_hash: None,
        };
        let processed = processed_workspace_document(uri.clone(), document, snapshot);
        let (_, evicted) = self.workspace_index.install_complete_with_eviction(
            uri.clone(),
            processed.entry,
            crate::workspace_index::ClosedProvenance::Dynamic,
        );
        let mut parents = vec![uri];
        parents.extend(evicted);
        self.refresh_tar_source_watch_registry(TarSourceWatchRegistryRefresh::Parents(parents));
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
        for (uri, entry) in self.workspace_index.artifact_iter() {
            entries.push((uri, entry.metadata));
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
        // workspace_index (shared) can coexist without pre-cloning all contents.
        let Self {
            cross_file_graph,
            workspace_index,
            cross_file_file_cache,
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
                workspace_index
                    .get(parent_uri)
                    .map(|e| e.contents.to_string())
                    .or_else(|| cross_file_file_cache.get(parent_uri))
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

    /// Capture every authority and immutable payload consumed by detached
    /// `system.file()` convergence.
    pub(crate) fn capture_system_file_analysis(
        &self,
        only_packages: Option<HashSet<String>>,
    ) -> CapturedSystemFileAnalysis {
        self.capture_system_file_analysis_with_routing(
            only_packages,
            self.system_file_routing_stamp(),
        )
    }

    pub(crate) fn capture_library_routing_system_file_analysis(
        &self,
        basis: &LibraryRoutingBasis,
        prospective: &ProspectiveLibraryRouting,
        only_packages: Option<HashSet<String>>,
    ) -> Option<CapturedSystemFileAnalysis> {
        if !Arc::ptr_eq(&self.package_library, &basis.library)
            || !self.library_replacement_basis_is_current(basis)
            || self.system_file_routing_stamp() != basis.routing
            || self.package_input_generation() != basis.package_input_generation
            || self.package_config_generation != basis.package_config_generation
            || self.package_state_record_generation != basis.package_state_record_generation
            || self.cross_file_config.packages_enabled != basis.packages_enabled
            || self.cross_file_config.packages_r_path != basis.packages_r_path
            || self.cross_file_config.packages_additional_library_paths
                != basis.packages_additional_library_paths
            || self.workspace_folders != basis.workspace_folders
            || basis
                .watcher_owner
                .is_some_and(|owner| !self.libpath_watcher_owner_is_current(owner))
        {
            return None;
        }
        Some(
            self.capture_system_file_analysis_with_routing(
                only_packages,
                prospective.routing.clone(),
            ),
        )
    }

    fn capture_system_file_analysis_with_routing(
        &self,
        only_packages: Option<HashSet<String>>,
        routing: SystemFileRoutingStamp,
    ) -> CapturedSystemFileAnalysis {
        let index = self.workspace_index.authority_snapshot();
        let source_selected = |source: &crate::cross_file::ForwardSource| {
            source.system_file.as_ref().is_some_and(|call| {
                only_packages
                    .as_ref()
                    .is_none_or(|packages| packages.contains(&call.package))
            })
        };
        let raw_content = index
            .artifacts
            .iter()
            .filter(|(_, entry)| entry.metadata.sources.iter().any(source_selected))
            .filter_map(|(uri, _)| {
                self.cross_file_file_cache
                    .get(uri)
                    .map(|content| (uri.clone(), content))
            })
            .collect();
        let open: Vec<_> = self
            .documents
            .keys()
            .filter_map(|uri| {
                let record = self.documents.get_record(uri)?.clone();
                Some(CapturedSystemFileOpen {
                    uri: uri.clone(),
                    token: self.documents.record_token(uri),
                    metadata: record.metadata().clone(),
                    document: record,
                    graph_roots: self.authoritative_revalidation_roots_for_uri(uri),
                })
            })
            .collect();
        let open_records = open
            .iter()
            .map(|open| (open.uri.clone(), open.token.clone()))
            .collect();
        CapturedSystemFileAnalysis {
            basis: SystemFileAnalysisBasis {
                routing,
                tar_source_event_generation: self.tar_source_event_generation,
                workspace_index_version: index.version,
                workspace_index_max_files: self.workspace_index.config().max_files,
                workspace_index_max_file_size_bytes: self
                    .workspace_index
                    .config()
                    .max_file_size_bytes,
                workspace_index_artifact_capacity: index.artifact_capacity_limit,
                workspace_index_pinned: index.pinned.clone(),
                graph_revision: self.cross_file_graph.edge_revision(),
                graph_authority_generation: self.workspace_graph_authority_generation,
                open_context_authority_generation: self.open_context_authority_generation,
                analysis_config_generation: self.analysis_config_generation,
                chunk_override_generation: self.chunk_override_generation,
                workspace_folders: self.workspace_folders.clone(),
                exclusion_patterns: self.workspace_exclusions.patterns().to_vec(),
                max_chain_depth: self.cross_file_config.max_chain_depth,
                open_records,
            },
            only_packages,
            full_content: index
                .full
                .iter()
                .map(|(uri, entry)| (uri.clone(), entry.contents.to_string()))
                .collect(),
            raw_content,
            artifacts: index.artifacts,
            open,
            graph: self.cross_file_graph.clone(),
            exclusions: self.workspace_exclusions.clone(),
        }
    }

    /// Attach the exact index CAS state after detached filesystem/graph work.
    pub(crate) fn finish_system_file_analysis(
        &self,
        mut draft: PreparedSystemFileDraft,
    ) -> Option<PreparedSystemFileAnalysis> {
        // Admission/eviction must see the pins required by the prospective
        // graph, not the pre-transaction graph. Otherwise a newly reachable
        // external install can evict itself before the graph swap commits.
        draft.index_changes.pins = self.open_neighborhood_pins_for_graph(&draft.graph);
        let has_index_changes = !draft.index_changes.metadata.is_empty()
            || !draft.index_changes.installs.is_empty()
            || !draft.index_changes.removals.is_empty()
            || draft.index_changes.pins != draft.basis.workspace_index_pinned;
        let index = if has_index_changes {
            self.workspace_index
                .prepare_targeted_batch_if_current(
                    draft.basis.workspace_index_version,
                    draft.index_changes,
                )
                .ok()??
                .into()
        } else {
            None
        };
        Some(PreparedSystemFileAnalysis {
            basis: draft.basis,
            index,
            open_metadata: draft.open_metadata,
            graph: draft.graph,
            changed_uris: draft.changed_uris,
            content_changed_uris: draft.content_changed_uris,
            external_observations: draft.external_observations,
        })
    }

    /// Deferred synchronous compatibility writer for `raven check`.
    ///
    /// The LSP and its behavior tests must use the detached two-attempt
    /// transaction in `backend`; this method is the intentionally isolated
    /// final legacy writer until CLI index installation is migrated as its own
    /// ownership family.
    ///
    /// With `Some(packages)`, only entries containing a `system.file()` source
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
    pub(crate) fn resolve_system_file_in_workspace_cli_compat(
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
            .workspace_index
            .artifact_entries_matching(|entry| entry.metadata.sources.iter().any(source_selected));

        // Open buffers with a selected system.file source (authoritative
        // metadata lives in the document store, not the index).
        let open_affected: Vec<Url> = self
            .documents
            .uris()
            .into_iter()
            .filter(|uri| {
                self.documents
                    .get_record(uri)
                    .is_some_and(|record| record.metadata().sources.iter().any(source_selected))
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
        for (uri, entry) in affected {
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
                let mut metadata = entry.metadata.clone();
                Arc::make_mut(&mut metadata).sources = new_sources;
                changed_uris.push(uri.clone());
                self.workspace_index
                    .replace_complete_metadata(&uri, metadata);
            }
        }

        // Rebuild graph edges for changed index entries
        let workspace_root = self.workspace_folders.first().cloned();
        for uri in &changed_uris {
            if let Some(meta) = self.workspace_index.get_metadata(uri) {
                let get_content = |parent_uri: &Url| -> Option<String> {
                    self.workspace_index
                        .get(parent_uri)
                        .map(|e| e.contents.to_string())
                        .or_else(|| self.cross_file_file_cache.get(parent_uri))
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
            let Some((doc_generation, doc_meta)) = self
                .documents
                .get_record(&uri)
                .map(|record| (record.generation(), record.metadata().clone()))
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
                self.replace_open_document_metadata_if_current(
                    &uri,
                    doc_generation,
                    new_meta.clone(),
                )
                .expect("source convergence basis remains current under the state write lock");
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
                    self.workspace_index
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

        if !changed_uris.is_empty() {
            self.advance_workspace_graph_authority_generation();
        }
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
            if !self.workspace_index.is_complete(&uri) {
                continue;
            }
            if self.documents.contains_key(&uri) || self.is_document_open_or_alias(&uri) {
                continue;
            }
            if let Ok(path) = uri.to_file_path()
                && workspace_dirs.iter().any(|dir| path.starts_with(dir))
            {
                continue;
            }
            let referenced_from_index = self.workspace_index.any_artifact(|entry| {
                entry
                    .metadata
                    .sources
                    .iter()
                    .any(|s| s.resolved_uri.as_ref() == Some(&uri))
            });
            let referenced_from_open_doc = || {
                self.documents.uris().into_iter().any(|doc_uri| {
                    self.documents.get_record(&doc_uri).is_some_and(|record| {
                        record
                            .metadata()
                            .sources
                            .iter()
                            .any(|s| s.resolved_uri.as_ref() == Some(&uri))
                    })
                })
            };
            if referenced_from_index || referenced_from_open_doc() {
                continue;
            }
            self.workspace_index.invalidate(&uri);
            self.cross_file_graph.remove_file(&uri);
            self.prune_editor_chunk_kind_override(&uri);
        }
    }

    /// Expand the changed-URI output of `system.file()` convergence into the
    /// open documents whose diagnostics may be affected: changed files plus
    /// their open transitive dependents and sibling subtrees. A parent's
    /// cross-file scope traverses forward source edges transitively, so an
    /// edge formed or dropped on a child changes the parent's diagnostics
    /// even though the parent's own text and edges are untouched — the same
    /// fan-out `did_change` performs via
    /// `compute_affected_dependents_after_edit`.
    pub fn system_file_republish_set(&self, changed: &[Url]) -> Vec<Url> {
        self.system_file_republish_set_with_content(changed, &HashSet::new())
    }

    fn system_file_republish_set_with_content(
        &self,
        changed: &[Url],
        content_changed: &HashSet<Url>,
    ) -> Vec<Url> {
        let mut seen: std::collections::HashSet<Url> = std::collections::HashSet::new();
        let mut out: Vec<Url> = Vec::new();
        for uri in changed {
            if let Some(open_uri) = self.open_document_uri_for_authoritative_uri(uri)
                && seen.insert(open_uri.clone())
            {
                out.push(open_uri);
            }
            let dependents = self.affected_open_dependents_after_edit(
                uri,
                content_changed.contains(uri),
                true, // its dependency edges may have changed
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
        for (_, entry) in self.workspace_index.artifact_iter() {
            for source in &entry.metadata.sources {
                if let Some(ref uri) = source.resolved_uri
                    && self.workspace_index.enrichment_status(uri).is_none()
                {
                    external_uris.push(uri.clone());
                }
            }
        }
        for doc_uri in self.documents.uris() {
            if let Some(record) = self.documents.get_record(&doc_uri) {
                for source in &record.metadata().sources {
                    if let Some(ref uri) = source.resolved_uri
                        && self.workspace_index.enrichment_status(uri).is_none()
                    {
                        external_uris.push(uri.clone());
                    }
                }
            }
        }
        external_uris.sort();
        external_uris.dedup();

        let mut tar_watch_parents = Vec::new();

        for uri in external_uris {
            if self.workspace_index.enrichment_status(&uri).is_some() {
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

            let chunk_kind = self.chunk_kind_for_closed_file(&uri);
            let document =
                Document::new_with_kind(&content, None, file_type_from_uri(&uri), chunk_kind);
            let metadata = Arc::new(document.cross_file_metadata());
            let artifacts = Arc::new(document.cross_file_artifacts(&uri, &metadata));
            let snapshot =
                crate::cross_file::file_cache::FileSnapshot::with_content_hash(&fs_meta, &content);
            let entry = crate::workspace_index::IndexEntry {
                contents: Rope::from_str(&content),
                tree: document.tree,
                loaded_packages: document.loaded_packages,
                data_packages: document.data_packages,
                snapshot,
                metadata,
                artifacts,
                indexed_at_version: 0,
            };
            let (_, evicted) = self.workspace_index.install_complete_with_eviction(
                uri.clone(),
                entry,
                crate::workspace_index::ClosedProvenance::Dynamic,
            );
            tar_watch_parents.push(uri);
            tar_watch_parents.extend(evicted);
        }
        if !tar_watch_parents.is_empty() {
            self.refresh_tar_source_watch_parents(tar_watch_parents);
        }
    }
}

/// Resolve selected `system.file()` calls, read newly referenced external
/// files, and derive graph/open/index replacements without a shared state lock.
pub(crate) fn prepare_system_file_analysis(
    captured: CapturedSystemFileAnalysis,
) -> PreparedSystemFileDraft {
    let CapturedSystemFileAnalysis {
        basis,
        only_packages,
        artifacts,
        mut full_content,
        raw_content,
        open,
        mut graph,
        exclusions,
    } = captured;
    let source_selected = |source: &crate::cross_file::ForwardSource| {
        source.system_file.as_ref().is_some_and(|call| {
            only_packages
                .as_ref()
                .is_none_or(|packages| packages.contains(&call.package))
        })
    };
    let mut considered_external_paths = HashSet::new();
    {
        let mut record_considered_path = |source: &crate::cross_file::ForwardSource| {
            if !source_selected(source) {
                return;
            }
            if let Some(path) = source
                .resolved_uri
                .as_ref()
                .and_then(|uri| uri.to_file_path().ok())
            {
                considered_external_paths.insert(path);
            }
            let Some(call) = source.system_file.as_ref() else {
                return;
            };
            if basis.routing.workspace_name.as_deref() == Some(call.package.as_str())
                && let Some(root) = basis.routing.workspace_root.as_ref()
            {
                considered_external_paths.insert(
                    call.parts
                        .iter()
                        .fold(root.clone(), |path, part| path.join(part)),
                );
            }
            for library in &basis.routing.library_paths {
                let package_root = library.join(&call.package);
                considered_external_paths.insert(
                    call.parts
                        .iter()
                        .fold(package_root, |path, part| path.join(part)),
                );
            }
        };
        for (_, entry) in &artifacts {
            for source in &entry.metadata.sources {
                record_considered_path(source);
            }
        }
        for input in &open {
            for source in &input.metadata.sources {
                record_considered_path(source);
            }
        }
    }
    let mut considered_external_paths: Vec<_> = considered_external_paths.into_iter().collect();
    considered_external_paths.sort_unstable();
    let observed_external: HashMap<_, _> = considered_external_paths
        .into_iter()
        .map(|path| {
            let observed = observe_system_file_external(&path);
            (path, observed)
        })
        .collect();
    let mut external_observations: Vec<_> = observed_external
        .values()
        .map(|(observation, _)| observation.clone())
        .collect();
    external_observations.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    let workspace_root = basis.workspace_folders.first();
    let mut closed_metadata: HashMap<_, _> = artifacts
        .iter()
        .map(|(uri, entry)| (uri.clone(), entry.metadata.clone()))
        .collect();
    let mut provenance: HashMap<_, _> = artifacts
        .iter()
        .map(|(uri, entry)| (uri.clone(), entry.provenance))
        .collect();
    for (uri, content) in raw_content {
        full_content.entry(uri).or_insert(content);
    }
    let mut changed_uris = Vec::new();
    let mut old_targets = HashSet::new();
    let mut metadata_changes = Vec::new();
    for (uri, entry) in &artifacts {
        if !entry.metadata.sources.iter().any(source_selected) {
            continue;
        }
        let mut sources = entry.metadata.sources.clone();
        crate::cross_file::resolve_system_file_source_entries(
            &mut sources,
            basis.routing.workspace_name.as_deref(),
            basis.routing.workspace_root.as_deref(),
            &basis.routing.library_paths,
        );
        if sources == entry.metadata.sources {
            continue;
        }
        old_targets.extend(
            entry
                .metadata
                .sources
                .iter()
                .filter_map(|source| source.resolved_uri.clone()),
        );
        let mut metadata = (*entry.metadata).clone();
        metadata.sources = sources;
        let metadata = Arc::new(metadata);
        closed_metadata.insert(uri.clone(), metadata.clone());
        metadata_changes.push((uri.clone(), metadata));
        changed_uris.push(uri.clone());
    }

    let mut open_metadata: HashMap<_, _> = open
        .iter()
        .map(|input| (input.uri.clone(), input.metadata.clone()))
        .collect();
    let mut changed_open = HashSet::new();
    for input in &open {
        if !input.metadata.sources.iter().any(source_selected) {
            continue;
        }
        let mut sources = input.metadata.sources.clone();
        crate::cross_file::resolve_system_file_source_entries(
            &mut sources,
            basis.routing.workspace_name.as_deref(),
            basis.routing.workspace_root.as_deref(),
            &basis.routing.library_paths,
        );
        if sources == input.metadata.sources {
            continue;
        }
        old_targets.extend(
            input
                .metadata
                .sources
                .iter()
                .filter_map(|source| source.resolved_uri.clone()),
        );
        let mut metadata = (*input.metadata).clone();
        metadata.sources = sources;
        open_metadata.insert(input.uri.clone(), Arc::new(metadata));
        changed_open.insert(input.uri.clone());
        if !changed_uris.contains(&input.uri) {
            changed_uris.push(input.uri.clone());
        }
    }

    let referenced_targets: HashSet<_> = closed_metadata
        .values()
        .chain(open_metadata.values())
        .flat_map(|metadata| {
            metadata
                .sources
                .iter()
                .filter_map(|source| source.resolved_uri.clone())
        })
        .collect();
    let existing: HashSet<_> = artifacts.iter().map(|(uri, _)| uri.clone()).collect();
    let protected_open: HashSet<_> = open
        .iter()
        .flat_map(|input| input.graph_roots.iter().cloned())
        .collect();
    let workspace_dirs: Vec<_> = basis
        .workspace_folders
        .iter()
        .filter_map(|uri| uri.to_file_path().ok())
        .collect();
    let mut removals: Vec<_> = old_targets
        .into_iter()
        .filter(|uri| !referenced_targets.contains(uri))
        .filter(|uri| existing.contains(uri))
        .filter(|uri| !protected_open.contains(uri))
        .filter(|uri| {
            uri.to_file_path()
                .ok()
                .is_none_or(|path| !workspace_dirs.iter().any(|root| path.starts_with(root)))
        })
        .collect();
    removals.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));

    let mut installs = Vec::new();
    let mut content_changed_uris = HashSet::new();
    let mut missing_targets: Vec<_> = referenced_targets
        .iter()
        .filter(|uri| !existing.contains(*uri))
        .cloned()
        .collect();
    missing_targets.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    let mut refresh_targets: Vec<_> = referenced_targets
        .iter()
        .filter(|uri| !protected_open.contains(*uri))
        .filter(|uri| {
            uri.to_file_path()
                .ok()
                .is_none_or(|path| !workspace_dirs.iter().any(|root| path.starts_with(root)))
        })
        .filter_map(|uri| {
            let entry = artifacts
                .iter()
                .find_map(|(candidate, entry)| (candidate == uri).then_some(entry))?;
            if entry.provenance != ClosedProvenance::Dynamic {
                return None;
            }
            let path = uri.to_file_path().ok()?;
            let (observation, _) = observed_external.get(&path)?;
            match &observation.identity {
                SystemFileExternalIdentity::Valid(snapshot) if snapshot != &entry.snapshot => {
                    Some(uri.clone())
                }
                SystemFileExternalIdentity::Valid(_)
                | SystemFileExternalIdentity::Missing
                | SystemFileExternalIdentity::InvalidBytes => None,
            }
        })
        .collect();
    refresh_targets.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    let refresh_set: HashSet<_> = refresh_targets.iter().cloned().collect();
    // A refreshed full record owns its metadata replacement. Supplying the
    // same URI to both targeted `metadata` and `installs` would reject the
    // whole atomic batch.
    metadata_changes.retain(|(uri, _)| !refresh_set.contains(uri));

    let mut install_targets: Vec<_> = missing_targets
        .into_iter()
        .map(|uri| (uri, false))
        .chain(refresh_targets.into_iter().map(|uri| (uri, true)))
        .collect();
    install_targets.sort_unstable_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
    for (uri, refresh) in install_targets {
        let Some(path) = uri.to_file_path().ok() else {
            continue;
        };
        let Some((observation, Some(content))) = observed_external.get(&path) else {
            continue;
        };
        let SystemFileExternalIdentity::Valid(snapshot) = &observation.identity else {
            continue;
        };
        let content = content.clone();
        let document = Document::new_with_uri(&content, None, &uri);
        let mut metadata = document.cross_file_metadata();
        if document.file_type == FileType::R {
            crate::cross_file::resolve_system_file_source_entries(
                &mut metadata.sources,
                basis.routing.workspace_name.as_deref(),
                basis.routing.workspace_root.as_deref(),
                &basis.routing.library_paths,
            );
        }
        let metadata = Arc::new(metadata);
        let artifacts = Arc::new(document.cross_file_artifacts(&uri, &metadata));
        let entry = IndexEntry {
            contents: Rope::from_str(&content),
            loaded_packages: document.loaded_packages,
            data_packages: document.data_packages,
            tree: document.tree,
            snapshot: snapshot.clone(),
            metadata: metadata.clone(),
            artifacts,
            indexed_at_version: 0,
        };
        full_content.insert(uri.clone(), content);
        closed_metadata.insert(uri.clone(), metadata);
        provenance.insert(uri.clone(), ClosedProvenance::Dynamic);
        if refresh {
            content_changed_uris.insert(uri.clone());
            changed_uris.push(uri.clone());
        }
        installs.push((uri, entry, ClosedProvenance::Dynamic));
    }

    let changed_closed: HashSet<_> = metadata_changes
        .iter()
        .map(|(uri, _)| uri.clone())
        .collect();
    for (uri, metadata) in &metadata_changes {
        let graph_metadata = WorldState::metadata_for_dependency_graph_with_exclusions(
            &exclusions,
            uri,
            metadata,
            workspace_root,
        );
        graph.update_file(uri, graph_metadata.as_ref(), workspace_root, |parent| {
            full_content.get(parent).cloned()
        });
    }
    for uri in &content_changed_uris {
        let metadata = closed_metadata
            .get(uri)
            .expect("every refreshed target has parsed metadata");
        let graph_metadata = WorldState::metadata_for_dependency_graph_with_exclusions(
            &exclusions,
            uri,
            metadata,
            workspace_root,
        );
        graph.update_file(uri, graph_metadata.as_ref(), workspace_root, |parent| {
            full_content.get(parent).cloned()
        });
        if exclusions.is_excluded_uri(uri) {
            graph.make_forward_edges_non_lending(uri);
        }
    }
    for input in &open {
        if !changed_open.contains(&input.uri) && !changed_closed.contains(&input.uri) {
            continue;
        }
        let metadata = open_metadata
            .get(&input.uri)
            .expect("every captured open record has metadata");
        for root in &input.graph_roots {
            let root_metadata = if root == &input.uri {
                metadata.clone()
            } else {
                let mut root_metadata = (**metadata).clone();
                root_metadata.inherited_working_directory = None;
                crate::cross_file::enrich_metadata_with_inherited_wd(
                    &mut root_metadata,
                    root,
                    workspace_root,
                    |parent| {
                        open_metadata
                            .get(parent)
                            .cloned()
                            .or_else(|| closed_metadata.get(parent).cloned())
                    },
                    basis.max_chain_depth,
                );
                Arc::new(root_metadata)
            };
            let graph_metadata = WorldState::metadata_for_dependency_graph_with_exclusions(
                &exclusions,
                root,
                root_metadata.as_ref(),
                workspace_root,
            );
            graph.update_file(root, graph_metadata.as_ref(), workspace_root, |parent| {
                full_content.get(parent).cloned()
            });
            if exclusions.is_excluded_uri(&input.uri) || exclusions.is_excluded_uri(root) {
                graph.make_forward_edges_non_lending(root);
            }
        }
    }
    for uri in &removals {
        graph.remove_file(uri);
    }

    let open_metadata = open
        .into_iter()
        .filter(|input| changed_open.contains(&input.uri))
        .map(|input| {
            let metadata = open_metadata
                .remove(&input.uri)
                .expect("changed open metadata was derived above");
            let prepared = OpenDocumentStore::prepare_metadata_replacement(
                &input.uri,
                &input.document,
                metadata,
            );
            PreparedWorkspaceOpenMetadata::new(input.uri, input.token, prepared)
        })
        .collect();
    let mut pins = basis.workspace_index_pinned.clone();
    for uri in &removals {
        pins.remove(uri);
    }
    changed_uris.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    changed_uris.dedup();
    PreparedSystemFileDraft {
        basis,
        index_changes: WorkspaceIndexTargetedChanges {
            metadata: metadata_changes,
            installs,
            removals,
            pins,
        },
        open_metadata,
        graph,
        changed_uris,
        content_changed_uris,
        external_observations,
    }
}

/// Authoritative open-buffer input layered over a workspace-scan candidate.
#[derive(Clone)]
pub(crate) struct WorkspaceGraphOverlay {
    pub(crate) uri: Url,
    pub(crate) content: Rope,
    pub(crate) chunk_kind: ChunkKind,
    /// Normally supplied by `open-document authority`; tests and compatibility callers
    /// may omit it, in which case derivation reparses the Rope off-lock.
    pub(crate) metadata: Option<Arc<crate::cross_file::CrossFileMetadata>>,
    pub(crate) graph_roots: Vec<Url>,
    pub(crate) excluded: bool,
}

/// Owned inputs required to derive a complete graph off the `WorldState` lock.
pub(crate) struct WorkspaceGraphDerivationContext {
    pub(crate) workspace_root: Option<Url>,
    pub(crate) max_depth: usize,
    pub(crate) exclusions: crate::config_file::CompiledWorkspaceExclusions,
    pub(crate) system_file_workspace_name: Option<String>,
    pub(crate) system_file_workspace_root: Option<PathBuf>,
    pub(crate) system_file_library_paths: Vec<PathBuf>,
}

/// Recompute closed-file inherited working directories from immutable metadata.
///
/// Each round starts from `base_closed_metadata`, so a disk-derived inherited
/// directory cannot survive merely because an authoritative open parent removed
/// or rerouted the source edge that used to reach it. Forward proposals use the
/// previous mixed candidate and converge through the outer bounded fixpoint.
fn derive_closed_descendant_metadata(
    base_closed_metadata: &HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>>,
    entries: &mut HashMap<Url, crate::workspace_index::IndexEntry>,
    open_metadata: &HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>>,
    workspace_root: Option<&Url>,
    max_depth: usize,
) -> bool {
    let mut closed_uris: Vec<_> = base_closed_metadata.keys().cloned().collect();
    closed_uris.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut next = HashMap::with_capacity(base_closed_metadata.len());
    for uri in closed_uris {
        let mut metadata = (**base_closed_metadata
            .get(&uri)
            .expect("URI came from base closed metadata"))
        .clone();
        metadata.inherited_working_directory = None;
        crate::cross_file::enrich_metadata_with_inherited_wd(
            &mut metadata,
            &uri,
            workspace_root,
            |parent_uri| {
                open_metadata
                    .get(parent_uri)
                    .cloned()
                    .or_else(|| entries.get(parent_uri).map(|entry| entry.metadata.clone()))
            },
            max_depth,
        );
        next.insert(uri, Arc::new(metadata));
    }

    let mut parents: Vec<_> = entries
        .iter()
        .filter(|(uri, _)| !open_metadata.contains_key(*uri))
        .map(|(uri, entry)| (uri.clone(), entry.metadata.clone()))
        .chain(
            open_metadata
                .iter()
                .map(|(uri, metadata)| (uri.clone(), metadata.clone())),
        )
        .collect();
    parents.sort_unstable_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    let mut forward_proposals = HashMap::new();
    for (parent_uri, parent_metadata) in parents {
        let Some(path_context) = crate::cross_file::path_resolve::PathContext::from_metadata(
            &parent_uri,
            &parent_metadata,
            workspace_root,
        ) else {
            continue;
        };
        for source in &parent_metadata.sources {
            let resolved = source
                .resolved_uri
                .as_ref()
                .and_then(|uri| uri.to_file_path().ok())
                .or_else(|| {
                    crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                        &source.path,
                        &path_context,
                    )
                });
            let Some(resolved) = resolved else { continue };
            let Ok(child_uri) = Url::from_file_path(&resolved) else {
                continue;
            };
            let Some(child) = next.get(&child_uri) else {
                continue;
            };
            if child.working_directory.is_some()
                || child.inherited_working_directory.is_some()
                || forward_proposals.contains_key(&child_uri)
            {
                continue;
            }
            if let Some(inherited) =
                path_context.forward_child_inherited_wd(&resolved, source.chdir)
            {
                forward_proposals.insert(child_uri, inherited.to_string_lossy().to_string());
            }
        }
    }
    for (uri, inherited) in forward_proposals {
        if let Some(metadata) = next.get_mut(&uri) {
            Arc::make_mut(metadata).inherited_working_directory = Some(inherited);
        }
    }

    let changed = next.iter().any(|(uri, metadata)| {
        entries.get(uri).is_none_or(|entry| {
            entry.metadata.inherited_working_directory.as_deref()
                != metadata.inherited_working_directory.as_deref()
        })
    });
    for (uri, metadata) in next {
        if let Some(entry) = entries.get_mut(&uri) {
            entry.metadata = metadata;
        }
    }
    changed
}

/// Recompute open-buffer inherited working directories against the current
/// mixed open/closed candidate.
///
/// Backward directives win when they produce a context. Forward proposals are
/// deterministic (URI-sorted parent order) and cover open children that have no
/// backward directive. Re-running this together with
/// [`derive_closed_descendant_metadata`] reaches alternating
/// open → closed → open chains without consulting live `WorldState`.
fn derive_open_candidate_metadata(
    base_open_metadata: &HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>>,
    current_open_metadata: &HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>>,
    entries: &HashMap<Url, crate::workspace_index::IndexEntry>,
    workspace_root: Option<&Url>,
    max_depth: usize,
) -> (
    HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>>,
    bool,
) {
    let mut open_uris: Vec<_> = base_open_metadata.keys().cloned().collect();
    open_uris.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut next = HashMap::with_capacity(base_open_metadata.len());
    for uri in open_uris {
        let mut metadata = (**base_open_metadata
            .get(&uri)
            .expect("URI came from base open metadata"))
        .clone();
        metadata.inherited_working_directory = None;
        crate::cross_file::enrich_metadata_with_inherited_wd(
            &mut metadata,
            &uri,
            workspace_root,
            |parent_uri| {
                current_open_metadata
                    .get(parent_uri)
                    .cloned()
                    .or_else(|| entries.get(parent_uri).map(|entry| entry.metadata.clone()))
            },
            max_depth,
        );
        next.insert(uri, Arc::new(metadata));
    }

    let mut parents: Vec<_> = entries
        .iter()
        .filter(|(uri, _)| !current_open_metadata.contains_key(*uri))
        .map(|(uri, entry)| (uri.clone(), entry.metadata.clone()))
        .chain(
            next.iter()
                .map(|(uri, metadata)| (uri.clone(), metadata.clone())),
        )
        .collect();
    parents.sort_unstable_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    let mut forward_proposals = HashMap::new();
    for (parent_uri, parent_metadata) in parents {
        let Some(path_context) = crate::cross_file::path_resolve::PathContext::from_metadata(
            &parent_uri,
            &parent_metadata,
            workspace_root,
        ) else {
            continue;
        };
        for source in &parent_metadata.sources {
            let resolved = source
                .resolved_uri
                .as_ref()
                .and_then(|uri| uri.to_file_path().ok())
                .or_else(|| {
                    crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                        &source.path,
                        &path_context,
                    )
                });
            let Some(resolved) = resolved else { continue };
            let Ok(child_uri) = Url::from_file_path(&resolved) else {
                continue;
            };
            let Some(child) = next.get(&child_uri) else {
                continue;
            };
            if child.working_directory.is_some()
                || child.inherited_working_directory.is_some()
                || forward_proposals.contains_key(&child_uri)
            {
                continue;
            }
            if let Some(inherited) =
                path_context.forward_child_inherited_wd(&resolved, source.chdir)
            {
                forward_proposals.insert(child_uri, inherited.to_string_lossy().to_string());
            }
        }
    }
    for (uri, inherited) in forward_proposals {
        if let Some(metadata) = next.get_mut(&uri) {
            Arc::make_mut(metadata).inherited_working_directory = Some(inherited);
        }
    }

    let changed = next.iter().any(|(uri, metadata)| {
        current_open_metadata.get(uri).is_none_or(|previous| {
            previous.inherited_working_directory != metadata.inherited_working_directory
        })
    });
    (next, changed)
}

/// Derive a deterministic complete graph from closed entries plus open overlays.
///
/// This function may resolve filesystem paths and therefore must run off any
/// shared `WorldState` lock.
pub(crate) fn derive_workspace_dependency_graph(
    entries: &mut HashMap<Url, crate::workspace_index::IndexEntry>,
    graph_roots: Option<&HashSet<Url>>,
    open_overlays: &[WorkspaceGraphOverlay],
    context: &WorkspaceGraphDerivationContext,
    refresh_open_source_batches: bool,
) -> (
    DependencyGraph,
    HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>>,
) {
    let workspace_root = context.workspace_root.as_ref();
    let base_open_metadata: HashMap<_, _> = open_overlays
        .iter()
        .map(|open| {
            let metadata = open.metadata.clone().unwrap_or_else(|| {
                let raw = open.content.to_string();
                Arc::new(crate::cross_file::extract_metadata_for_kind(
                    open.chunk_kind,
                    &raw,
                ))
            });
            (open.uri.clone(), metadata)
        })
        .collect();
    let mut base_open_metadata = base_open_metadata;
    for metadata in base_open_metadata.values_mut() {
        if metadata
            .sources
            .iter()
            .any(|source| source.system_file.is_some())
        {
            crate::cross_file::resolve_system_file_sources(
                Arc::make_mut(metadata),
                context.system_file_workspace_name.as_deref(),
                context.system_file_workspace_root.as_deref(),
                &context.system_file_library_paths,
            );
        }
    }
    let previous_open_metadata = base_open_metadata.clone();

    for entry in entries.values_mut() {
        if entry
            .metadata
            .sources
            .iter()
            .any(|source| source.system_file.is_some())
        {
            crate::cross_file::resolve_system_file_sources(
                Arc::make_mut(&mut entry.metadata),
                context.system_file_workspace_name.as_deref(),
                context.system_file_workspace_root.as_deref(),
                &context.system_file_library_paths,
            );
        }
    }
    let previous_closed_metadata: HashMap<_, _> = entries
        .iter()
        .map(|(uri, entry)| (uri.clone(), entry.metadata.clone()))
        .collect();
    let mut base_closed_metadata: HashMap<_, _> = entries
        .iter()
        .map(|(uri, entry)| (uri.clone(), entry.metadata.clone()))
        .collect();
    for metadata in base_closed_metadata.values_mut() {
        Arc::make_mut(metadata).inherited_working_directory = None;
    }
    for (uri, metadata) in &base_closed_metadata {
        if let Some(entry) = entries.get_mut(uri) {
            entry.metadata = metadata.clone();
        }
    }
    let mut open_metadata = base_open_metadata.clone();
    for metadata in open_metadata.values_mut() {
        Arc::make_mut(metadata).inherited_working_directory = None;
    }
    for _ in 0..context.max_depth.max(1) {
        let closed_changed = derive_closed_descendant_metadata(
            &base_closed_metadata,
            entries,
            &open_metadata,
            workspace_root,
            context.max_depth,
        );
        let (next_open_metadata, open_changed) = derive_open_candidate_metadata(
            &base_open_metadata,
            &open_metadata,
            entries,
            workspace_root,
            context.max_depth,
        );
        open_metadata = next_open_metadata;
        if !closed_changed && !open_changed {
            break;
        }
    }

    // Source-batch expansion is the final metadata derivation stage: inherited
    // WD and system.file resolution are already stable. A scan entry or open
    // overlay normally arrives with an expansion from the same candidate.
    // Reuse it when the effective path context is unchanged so one candidate
    // observes each directory only once.
    for (uri, entry) in entries.iter_mut() {
        let metadata = Arc::make_mut(&mut entry.metadata);
        let reused = previous_closed_metadata.get(uri).is_some_and(|previous| {
            crate::cross_file::tar_source::reuse_tar_source_expansion(
                metadata,
                previous,
                uri,
                workspace_root,
                &context.exclusions,
            )
        });
        if !reused {
            let _ = crate::cross_file::tar_source::finalize_tar_source_requests_with_exclusions(
                metadata,
                uri,
                workspace_root,
                &context.exclusions,
            );
        }
        let analysis = entry.contents.to_string();
        entry.artifacts = Arc::new(if file_type_from_uri(uri) == FileType::R {
            match entry.tree.as_ref() {
                Some(tree) => crate::cross_file::scope::compute_artifacts_with_metadata(
                    uri,
                    tree,
                    &analysis,
                    Some(entry.metadata.as_ref()),
                ),
                None => crate::cross_file::scope::ScopeArtifacts::default(),
            }
        } else {
            crate::cross_file::scope::ScopeArtifacts::default()
        });
    }
    for (uri, metadata) in &mut open_metadata {
        let metadata = Arc::make_mut(metadata);
        let reused = !refresh_open_source_batches
            && previous_open_metadata.get(uri).is_some_and(|previous| {
                crate::cross_file::tar_source::reuse_tar_source_expansion(
                    metadata,
                    previous,
                    uri,
                    workspace_root,
                    &context.exclusions,
                )
            });
        if !reused {
            let _ = crate::cross_file::tar_source::finalize_tar_source_requests_with_exclusions(
                metadata,
                uri,
                workspace_root,
                &context.exclusions,
            );
        }
    }

    let mut content: HashMap<Url, String> = entries
        .iter()
        .map(|(uri, entry)| (uri.clone(), entry.contents.to_string()))
        .collect();
    for open in open_overlays {
        let open_content = open.content.to_string();
        content.insert(open.uri.clone(), open_content.clone());
        for root in &open.graph_roots {
            content.insert(root.clone(), open_content.clone());
        }
    }

    let mut graph = DependencyGraph::new();
    let mut closed_uris: Vec<_> = entries.keys().cloned().collect();
    closed_uris.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    for uri in closed_uris {
        if graph_roots.is_some_and(|roots| !roots.contains(&uri)) {
            continue;
        }
        let Some(entry) = entries.get(&uri) else {
            continue;
        };
        let graph_metadata = WorldState::metadata_for_dependency_graph_with_exclusions(
            &context.exclusions,
            &uri,
            entry.metadata.as_ref(),
            workspace_root,
        );
        graph.update_file(
            &uri,
            graph_metadata.as_ref(),
            workspace_root,
            |parent_uri| content.get(parent_uri).cloned(),
        );
    }

    let metadata_lookup = |uri: &Url| {
        open_metadata
            .get(uri)
            .cloned()
            .or_else(|| entries.get(uri).map(|entry| entry.metadata.clone()))
    };
    let mut open_roots = Vec::new();
    for open in open_overlays {
        for root in &open.graph_roots {
            open_roots.push((root.clone(), open));
        }
    }
    open_roots.sort_unstable_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
    for (root, open) in open_roots {
        let mut metadata = open_metadata
            .get(&open.uri)
            .map(|value| (**value).clone())
            .expect("open metadata is derived above")
            .clone();
        if root != open.uri {
            metadata.inherited_working_directory = None;
            crate::cross_file::enrich_metadata_with_inherited_wd(
                &mut metadata,
                &root,
                workspace_root,
                metadata_lookup,
                context.max_depth,
            );
        }
        if metadata
            .sources
            .iter()
            .any(|source| source.system_file.is_some())
        {
            crate::cross_file::resolve_system_file_sources(
                &mut metadata,
                context.system_file_workspace_name.as_deref(),
                context.system_file_workspace_root.as_deref(),
                &context.system_file_library_paths,
            );
        }
        let graph_metadata = WorldState::metadata_for_dependency_graph_with_exclusions(
            &context.exclusions,
            &root,
            &metadata,
            workspace_root,
        );
        graph.update_file(
            &root,
            graph_metadata.as_ref(),
            workspace_root,
            |parent_uri| content.get(parent_uri).cloned(),
        );
        if open.excluded {
            graph.make_forward_edges_non_lending(&root);
        }
    }
    (graph, open_metadata)
}

/// Scan workspace folders for R files without holding any locks (Requirement 13a)
///
/// Returns the artifact-only compatibility entries and the authoritative full
/// closed-file entries. Phase B merges those two payload tiers.
///
/// Package-mode state (workspace/namespace model, roxygen cache, NAMESPACE
/// imports) is intentionally **not** produced here. The canonical derivation
/// is `derive_package_state`; event adapters and prepared package projections
/// install its result through the shared `WorldState` derived-record seam.
///
/// **Validates: Requirements 11.1, 11.2, 11.3, 11.4, 11.5**
pub type WorkspaceScanResult = HashMap<Url, crate::workspace_index::IndexEntry>;

/// Result of processing a single workspace file (used by parallel scan).
struct ProcessedFile {
    uri: Url,
    entry: crate::workspace_index::IndexEntry,
}

/// Recursively collect file paths under `dir` whose leaf matches `accept`
/// (serial walk, fast). Symlinked directories ARE followed, with canonical-path
/// cycle detection to terminate on loops and avoid double-counting; the
/// non-source directories in [`should_skip_directory`] (`.git`, `node_modules`,
/// `renv`, `target`, …) are pruned. Results are unsorted; callers that need
/// deterministic order sort afterwards.
///
/// This is the single directory walk shared by the workspace indexer (which
/// passes [`is_stat_model_extension`] to collect
/// `.r`/`.jags`/`.bugs`/`.bug`/`.stan`)
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
pub(crate) fn decode_source(bytes: Vec<u8>) -> Result<String, SourceReadError> {
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

fn processed_workspace_document(
    uri: Url,
    doc: Document,
    snapshot: crate::cross_file::file_cache::FileSnapshot,
) -> ProcessedFile {
    // Pair `doc.tree` with the analysis text it was parsed from for both
    // metadata extraction and artifact computation, so byte offsets align
    // (#343). The scan excludes chunk documents but includes Stan; the shared
    // document methods keep Stan inert and preserve its optional directive
    // mask. The pairing must remain analysis-consistent if chunk scanning is
    // ever added — feeding raw text against a masked tree can mis-slice.
    let cross_file_meta = doc.cross_file_metadata();
    let artifacts = std::sync::Arc::new(doc.cross_file_artifacts(&uri, &cross_file_meta));

    let cross_file_meta = Arc::new(cross_file_meta);

    let entry = crate::workspace_index::IndexEntry {
        contents: doc.contents.clone(),
        tree: doc.tree.clone(),
        loaded_packages: doc.loaded_packages.clone(),
        data_packages: doc.data_packages.clone(),
        snapshot,
        metadata: cross_file_meta,
        artifacts,
        indexed_at_version: 0,
    };

    ProcessedFile { uri, entry }
}

/// Process a single file: read, parse, compute metadata and artifacts.
/// Returns `None` if the file can't be read or converted to a URI.
fn process_workspace_file(path: &Path) -> Option<ProcessedFile> {
    let text = read_source(path).ok()?;
    let uri = Url::from_file_path(path).ok()?;
    let metadata_result = fs::metadata(path).ok()?;

    log::trace!("Scanning file: {}", uri);
    let doc = Document::new_with_uri(&text, None, &uri);
    let snapshot =
        crate::cross_file::file_cache::FileSnapshot::with_content_hash(&metadata_result, &text);
    Some(processed_workspace_document(uri, doc, snapshot))
}

/// Reparse scanned files whose last editor language supplied a chunk
/// classification different from their path.
///
/// A detached scan reads disk before it can snapshot live `WorldState`.
/// Applying these captured overrides to the caller-owned result keeps parsing
/// and metadata derivation off-lock while preserving the closed-file language
/// signal. Final apply validates the workspace-index version and open-document
/// identities that protect this override snapshot from concurrent changes.
pub(crate) fn apply_workspace_scan_chunk_overrides(
    result: &mut WorkspaceScanResult,
    overrides: &HashMap<Url, ChunkKind>,
) {
    for (uri, chunk_kind) in overrides {
        let Some(entry) = result.get(uri) else {
            continue;
        };
        let snapshot = entry.snapshot.clone();
        let raw = entry.contents.to_string();
        let document = Document::new_with_kind(&raw, None, file_type_from_uri(uri), *chunk_kind);
        let processed = processed_workspace_document(uri.clone(), document, snapshot);
        result.insert(uri.clone(), processed.entry);
    }
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
    type IndexMap = HashMap<Url, crate::workspace_index::IndexEntry>;

    // Phase 2+3: Process files in parallel and accumulate directly into
    // thread-local HashMaps via fold, then merge with reduce. This avoids
    // an intermediate Vec<ProcessedFile> that would transiently hold all
    // file contents + ASTs and require two extra Url clones per file for
    // the serial insert loop.
    let mut entries: IndexMap = file_paths
        .par_iter()
        .fold(IndexMap::new, |mut entries, path| {
            if let Some(item) = process_workspace_file(path) {
                entries.insert(item.uri, item.entry);
            }
            entries
        })
        .reduce(IndexMap::new, |mut left, right| {
            left.extend(right);
            left
        });

    // Second pass: iteratively enrich metadata with inherited_working_directory
    // Track only files that need enrichment to avoid O(n²) behavior
    let mut files_needing_enrichment: HashSet<Url> = entries
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
        let metadata_map: HashMap<Url, Arc<crate::cross_file::CrossFileMetadata>> = entries
            .iter()
            .map(|(uri, entry)| (uri.clone(), entry.metadata.clone()))
            .collect();

        let mut newly_enriched = Vec::new();

        // Only process files that need enrichment
        for uri in &files_needing_enrichment {
            if let Some(entry) = entries.get_mut(uri) {
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

    for (uri, entry) in &mut entries {
        let metadata = Arc::make_mut(&mut entry.metadata);
        let _ = crate::cross_file::tar_source::finalize_tar_source_requests_with_exclusions(
            metadata,
            uri,
            workspace_root.as_ref(),
            exclusions,
        );
        crate::cross_file::enrich_selective_import_resolutions(
            metadata,
            uri,
            workspace_root.as_ref(),
        );
        let analysis = entry.contents.to_string();
        entry.artifacts = Arc::new(if file_type_from_uri(uri) == FileType::R {
            match entry.tree.as_ref() {
                Some(tree) => crate::cross_file::scope::compute_artifacts_with_metadata(
                    uri,
                    tree,
                    &analysis,
                    Some(entry.metadata.as_ref()),
                ),
                None => crate::cross_file::scope::ScopeArtifacts::default(),
            }
        } else {
            crate::cross_file::scope::ScopeArtifacts::default()
        });
    }

    log::info!("Scanned {} workspace files", entries.len());

    // Package-mode detection is *not* done here. `scan_workspace` used to
    // construct `PackageWorkspace` and a `PackageNamespaceModel` inline —
    // detecting roxygen, parsing NAMESPACE, aggregating roxygen tags per
    // file, and caching per-file roxygen tags — but
    // that logic duplicated `derive_package_state` and the result was
    // unconditionally overwritten by the `apply_package_event(Initial)`
    // call that follows `apply_workspace_index` in `backend.rs`.
    // The canonical derivation is now single-sourced through the event path
    // (`PackageInputs` → `derive_package_state`).

    entries
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
                || ext.eq_ignore_ascii_case("bug")
                || ext.eq_ignore_ascii_case("stan")
        })
}

// `scan_directory` was replaced by `collect_files_matching` + `process_workspace_file`
// for parallel scanning via rayon. See `scan_workspace`.

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};

    #[tokio::test]
    async fn final_handoff_completion_is_durable_before_waiter_arrives() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm_for("durable");
        let claim = capture.claim().unwrap();
        let recorder = tokio::spawn(async move {
            claim.record_and_pause(7_u8).await.unwrap().finish();
        });
        assert_eq!(handle.wait_payload().await, 7);
        handle.release();
        recorder.await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            handle.wait_completed(),
        )
        .await
        .expect("completion recorded before the waiter must remain observable");
        assert!(handle.status().completed);
    }

    #[tokio::test]
    async fn final_handoff_claim_lineage_retires_only_after_last_owner_drops() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm_for("lineage");
        let lineage =
            FinalHandoffClaimLineage::<WatchedFinalHandoffForTest>::new(capture.claim().unwrap());
        let successor = lineage.clone();

        drop(lineage);
        assert!(!handle.status().recorded);

        drop(successor);
        assert_eq!(
            handle.wait_payload().await.outcome,
            WatchedFinalHandoffOutcome::RetiredBeforeFinalHandoff
        );
        handle.wait_completed().await;
        assert!(handle.status().abnormal_exits.is_empty());
    }

    #[tokio::test]
    async fn final_handoff_claim_lineage_normal_record_outranks_terminal_drop() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm_for("lineage");
        let lineage =
            FinalHandoffClaimLineage::<WatchedFinalHandoffForTest>::new(capture.claim().unwrap());
        let recorder = lineage.clone();
        let task = tokio::spawn(async move {
            recorder
                .claim()
                .record_and_pause(WatchedFinalHandoffForTest {
                    outcome: WatchedFinalHandoffOutcome::Finalized,
                    reserved: Vec::new(),
                    transferred: Vec::new(),
                })
                .await
                .unwrap()
                .finish();
        });

        assert_eq!(
            handle.wait_payload().await.outcome,
            WatchedFinalHandoffOutcome::Finalized
        );
        handle.release();
        task.await.unwrap();
        drop(lineage);
        handle.wait_completed().await;
        assert!(handle.status().abnormal_exits.is_empty());
    }

    #[tokio::test]
    async fn final_handoff_claim_lineage_panic_is_abnormal() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm_for("lineage");
        let claim = capture.claim().unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _lineage = FinalHandoffClaimLineage::<WatchedFinalHandoffForTest>::new(claim);
            panic!("test panic");
        }));
        assert!(panic.is_err());

        assert_eq!(
            handle.wait_payload().await.outcome,
            WatchedFinalHandoffOutcome::RetiredBeforeFinalHandoff
        );
        handle.wait_completed().await;
        assert_eq!(handle.status().abnormal_exits, vec![(1, "root")]);
    }

    #[tokio::test]
    async fn final_handoff_child_keeps_receipt_open_after_root_finishes() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm();
        let claim = capture.claim().unwrap();
        let recorder = tokio::spawn(async move { claim.record_and_pause(()).await.unwrap() });
        handle.wait_payload().await;
        handle.release();
        let root = recorder.await.unwrap();
        let child = root.child("child");
        root.finish();
        assert!(!handle.status().completed);
        assert_eq!(handle.status().outstanding.len(), 1);
        child.finish();
        handle.wait_completed().await;
        assert!(handle.status().abnormal_exits.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn final_handoff_concurrent_last_children_complete_once() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm();
        let claim = capture.claim().unwrap();
        let recorder = tokio::spawn(async move { claim.record_and_pause(()).await.unwrap() });
        handle.wait_payload().await;
        handle.release();
        let root = recorder.await.unwrap();
        let left = root.child("left");
        let right = root.child("right");
        root.finish();
        let (left, right) = tokio::join!(
            tokio::spawn(async move { left.finish() }),
            tokio::spawn(async move { right.finish() }),
        );
        left.unwrap();
        right.unwrap();
        handle.wait_completed().await;
        assert!(handle.status().outstanding.is_empty());
    }

    #[tokio::test]
    async fn final_handoff_abnormal_drop_is_reported() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm();
        let claim = capture.claim().unwrap();
        let recorder = tokio::spawn(async move { claim.record_and_pause(()).await.unwrap() });
        handle.wait_payload().await;
        handle.release();
        drop(recorder.await.unwrap());
        handle.wait_completed().await;
        assert_eq!(handle.status().abnormal_exits, vec![(1, "root")]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn final_handoff_spawn_abort_and_panic_are_reported() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm();
        let claim = capture.claim().unwrap();
        let recorder = tokio::spawn(async move { claim.record_and_pause(()).await.unwrap() });
        handle.wait_payload().await;
        handle.release();
        let root = recorder.await.unwrap();
        let aborted = root.child("aborted-child");
        root.finish();
        let task = tokio::spawn(async move {
            let _completion = aborted;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        handle.wait_completed().await;
        assert_eq!(handle.status().abnormal_exits, vec![(2, "aborted-child")]);

        let capture = FinalHandoffCapture::default();
        let handle = capture.arm();
        let claim = capture.claim().unwrap();
        let recorder = tokio::spawn(async move { claim.record_and_pause(()).await.unwrap() });
        handle.wait_payload().await;
        handle.release();
        let root = recorder.await.unwrap();
        let panicked = root.child("panicked-child");
        root.finish();
        let task = tokio::spawn(async move {
            let _completion = panicked;
            panic!("intentional descendant panic");
        });
        assert!(task.await.unwrap_err().is_panic());
        handle.wait_completed().await;
        assert_eq!(handle.status().abnormal_exits, vec![(2, "panicked-child")]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn final_handoff_cloned_finalizers_have_one_completion_owner() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm();
        let claim = capture.claim().unwrap();
        let other = claim.clone();
        let first = tokio::spawn(async move { claim.record_and_pause(1_u8).await });
        let second = tokio::spawn(async move { other.record_and_pause(2_u8).await });
        let payload = handle.wait_payload().await;
        assert!(payload == 1 || payload == 2);
        handle.release();
        let first = first.await.unwrap();
        let second = second.await.unwrap();
        assert_eq!(
            usize::from(first.is_some()) + usize::from(second.is_some()),
            1
        );
        first.into_iter().chain(second).next().unwrap().finish();
        handle.wait_completed().await;
    }

    #[tokio::test]
    async fn final_handoff_empty_payload_is_still_a_completed_handoff() {
        let capture = FinalHandoffCapture::default();
        let handle = capture.arm();
        let claim = capture.claim().unwrap();
        claim.record_completed(Vec::<u8>::new());
        assert!(handle.wait_payload().await.is_empty());
        handle.wait_completed().await;
        assert!(handle.status().completed);
    }

    fn full_change(text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_owned(),
        }
    }

    fn cache_snapshot(size: u64, content_hash: u64) -> crate::cross_file::file_cache::FileSnapshot {
        crate::cross_file::file_cache::FileSnapshot {
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size,
            content_hash: Some(content_hash),
        }
    }

    fn assert_tar_watch_registry_matches_full_oracle(state: &WorldState) {
        let (by_parent, by_path) = state.collect_authoritative_tar_source_watch_registry();
        assert_eq!(state.tar_source_watch_paths_by_parent, by_parent);
        assert_eq!(state.tar_source_parents_by_watch_path, by_path);
    }

    fn tar_watch_test_entry(
        metadata: crate::cross_file::CrossFileMetadata,
    ) -> crate::workspace_index::IndexEntry {
        crate::workspace_index::IndexEntry {
            contents: Rope::from_str("x <- 1\n"),
            tree: None,
            loaded_packages: Vec::new(),
            data_packages: Vec::new(),
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: None,
            },
            metadata: Arc::new(metadata),
            artifacts: Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 0,
        }
    }

    fn commit_test_edit(
        state: &mut WorldState,
        uri: &Url,
        text: &str,
        metadata: crate::cross_file::CrossFileMetadata,
        plan: PreparedOpenCommitPlan,
    ) -> Result<AnalysisCommitEffects, AnalysisCommitRejected> {
        let edit = state
            .prepare_document_changes(uri, [full_change(text)], 2)
            .unwrap();
        state.try_commit_analysis(PreparedAnalysisCommit::OpenEdit(Box::new(
            PreparedOpenEditAnalysis::new(edit, Arc::new(metadata), plan),
        )))
    }

    fn open_projection(
        uri: Url,
        graph_metadata: crate::cross_file::CrossFileMetadata,
        old_metadata: Option<crate::cross_file::CrossFileMetadata>,
        new_metadata: crate::cross_file::CrossFileMetadata,
        make_non_lending: bool,
    ) -> PreparedOpenGraphProjection {
        PreparedOpenGraphProjection {
            uri,
            graph_metadata: Arc::new(graph_metadata),
            old_metadata: old_metadata.map(Arc::new),
            new_metadata: Arc::new(new_metadata),
            parent_content: HashMap::new(),
            make_non_lending,
        }
    }

    fn transfer_candidate(state: &mut WorldState, uri: &Url) -> AnalysisTransferCandidate {
        state.open_document(uri.clone(), "value <- 1\n", Some(1));
        state.begin_open_document_diagnostic_lifecycle(uri).unwrap();
        state
            .capture_analysis_transfer_candidates([uri.clone()])
            .pop()
            .unwrap()
    }

    fn test_transfer(
        state: &mut WorldState,
        identity: AnalysisTransferIdentity,
        candidate: AnalysisTransferCandidate,
    ) -> AnalysisTransferHandle {
        state.install_analysis_transfer(identity, None, vec![candidate])
    }

    #[test]
    fn analysis_transfer_subject_policy_wins_global_uri_dedup_before_cap() {
        for cap in [1, 2] {
            let subject_uri = Url::parse("file:///workspace/DESCRIPTION").unwrap();
            let dependent_uri = Url::parse("file:///workspace/R/consumer.R").unwrap();
            let mut state = WorldState::new();
            state.cross_file_config.max_revalidations_per_trigger = cap;
            let mut subject = transfer_candidate(&mut state, &subject_uri);
            let dependent_duplicate = subject.clone();
            subject.reservation = AnalysisTransferReservationPolicy::Subject { debounce_ms: 17 };
            let dependent = transfer_candidate(&mut state, &dependent_uri);

            let effects = state.reserve_analysis_transfer_candidates(vec![
                subject,
                dependent,
                dependent_duplicate,
            ]);

            assert_eq!(effects.revalidations[0].uri, subject_uri);
            assert_eq!(effects.revalidations[0].debounce_ms, 17);
            assert_eq!(
                effects
                    .revalidations
                    .iter()
                    .filter(|ticket| ticket.uri == subject_uri)
                    .count(),
                1,
                "Subject(A), Dependent(B), Dependent(A) must globally merge A"
            );
            assert_eq!(
                state
                    .diagnostics_gate
                    .force_republish_count_for_test(&subject_uri),
                0,
                "the merged subject must keep its no-force policy"
            );
            assert_eq!(effects.revalidations.len(), cap);
            assert_eq!(
                state
                    .diagnostics_gate
                    .force_republish_count_for_test(&dependent_uri),
                u32::from(cap == 2),
                "the dependent is force-marked only when it survives the cap"
            );
        }
    }

    #[test]
    fn analysis_transfer_multi_consume_is_all_or_none_in_both_orders() {
        for invalid_first in [false, true] {
            let uri = Url::parse("file:///workspace/live.R").unwrap();
            let mut state = WorldState::new();
            let candidate = transfer_candidate(&mut state, &uri);
            let valid = test_transfer(
                &mut state,
                AnalysisTransferIdentity::WorkspaceScan(WorkspaceScanTransferIdentity {
                    intent_generation: 1,
                    commit_generation: 2,
                    committed_scan_generation: 3,
                }),
                candidate,
            );
            let invalid = AnalysisTransferHandle {
                identity: AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
                    routing_owner: SystemFileRoutingOwnerIdentity(4),
                    commit_generation: 5,
                }),
            };
            let handles = if invalid_first {
                vec![invalid, valid]
            } else {
                vec![valid, invalid]
            };
            let rejected = state.finalize_analysis_transfers(
                WorldState::begin_analysis_transfer_finalization(),
                &handles,
                Vec::new(),
            );
            assert_eq!(
                rejected,
                Err(AnalysisTransferRejection::MissingOrWrongOwner)
            );
            assert_eq!(state.analysis_revalidation_reservation_count, 0);
            assert_eq!(
                state.diagnostics_gate.force_republish_count_for_test(&uri),
                0
            );

            let committed = state
                .finalize_analysis_transfers(
                    WorldState::begin_analysis_transfer_finalization(),
                    &[valid],
                    Vec::new(),
                )
                .unwrap();
            assert!(matches!(
                committed,
                AnalysisTransferFinalization::Committed(ref tickets)
                    if tickets.len() == 1 && tickets[0].uri == uri
            ));
        }
    }

    #[test]
    fn analysis_transfer_invalid_workspace_and_valid_system_is_atomic_in_both_orders() {
        for invalid_first in [false, true] {
            let uri = Url::parse("file:///workspace/live.R").unwrap();
            let mut state = WorldState::new();
            let candidate = transfer_candidate(&mut state, &uri);
            let valid = test_transfer(
                &mut state,
                AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
                    routing_owner: SystemFileRoutingOwnerIdentity(1),
                    commit_generation: 2,
                }),
                candidate,
            );
            let invalid = AnalysisTransferHandle {
                identity: AnalysisTransferIdentity::WorkspaceScan(WorkspaceScanTransferIdentity {
                    intent_generation: 3,
                    commit_generation: 4,
                    committed_scan_generation: 5,
                }),
            };
            let handles = if invalid_first {
                vec![invalid, valid]
            } else {
                vec![valid, invalid]
            };
            assert_eq!(
                state.finalize_analysis_transfers(
                    WorldState::begin_analysis_transfer_finalization(),
                    &handles,
                    Vec::new(),
                ),
                Err(AnalysisTransferRejection::MissingOrWrongOwner)
            );
            assert_eq!(state.analysis_revalidation_reservation_count, 0);
            assert_eq!(
                state.diagnostics_gate.force_republish_count_for_test(&uri),
                0
            );
            assert!(matches!(
                state
                    .finalize_analysis_transfers(
                        WorldState::begin_analysis_transfer_finalization(),
                        &[valid],
                        Vec::new(),
                    )
                    .unwrap(),
                AnalysisTransferFinalization::Committed(ref tickets)
                    if tickets.len() == 1 && tickets[0].uri == uri
            ));
        }
    }

    #[test]
    fn analysis_transfer_overlap_reserves_once_and_replay_is_idempotent() {
        let uri = Url::parse("file:///workspace/live.R").unwrap();
        let mut state = WorldState::new();
        let candidate = transfer_candidate(&mut state, &uri);
        let first = test_transfer(
            &mut state,
            AnalysisTransferIdentity::WorkspaceScan(WorkspaceScanTransferIdentity {
                intent_generation: 1,
                commit_generation: 2,
                committed_scan_generation: 3,
            }),
            candidate.clone(),
        );
        let second = test_transfer(
            &mut state,
            AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
                routing_owner: SystemFileRoutingOwnerIdentity(4),
                commit_generation: 5,
            }),
            candidate,
        );
        let finalization = WorldState::begin_analysis_transfer_finalization();
        let committed = state
            .finalize_analysis_transfers(finalization, &[first, second], Vec::new())
            .unwrap();
        assert!(matches!(
            committed,
            AnalysisTransferFinalization::Committed(ref tickets) if tickets.len() == 1
        ));
        assert_eq!(state.analysis_revalidation_reservation_count, 1);
        assert_eq!(
            state.diagnostics_gate.force_republish_count_for_test(&uri),
            1
        );
        assert_eq!(
            state
                .finalize_analysis_transfers(finalization, &[first, second], Vec::new())
                .unwrap(),
            AnalysisTransferFinalization::AlreadyFinalized
        );
        assert_eq!(state.analysis_revalidation_reservation_count, 1);
    }

    #[test]
    fn analysis_transfer_successor_proves_inheritance_before_supersession() {
        let uri = Url::parse("file:///workspace/live.R").unwrap();
        let mut state = WorldState::new();
        let candidate = transfer_candidate(&mut state, &uri);
        let old_identity = AnalysisTransferIdentity::WorkspaceScan(WorkspaceScanTransferIdentity {
            intent_generation: 1,
            commit_generation: 2,
            committed_scan_generation: 3,
        });
        let old = state.install_analysis_transfer(old_identity, None, vec![candidate.clone()]);
        let new_identity = AnalysisTransferIdentity::WorkspaceScan(WorkspaceScanTransferIdentity {
            intent_generation: 4,
            commit_generation: 5,
            committed_scan_generation: 6,
        });
        let new = state.install_analysis_transfer(new_identity, Some(old_identity), Vec::new());
        assert_eq!(
            state.finalize_analysis_transfers(
                WorldState::begin_analysis_transfer_finalization(),
                &[old],
                Vec::new(),
            ),
            Err(AnalysisTransferRejection::Superseded {
                previous: old,
                successor: new,
            })
        );
        let committed = state
            .finalize_analysis_transfers(
                WorldState::begin_analysis_transfer_finalization(),
                &[new],
                Vec::new(),
            )
            .unwrap();
        assert!(matches!(
            committed,
            AnalysisTransferFinalization::Committed(ref tickets)
                if tickets.len() == 1 && tickets[0].uri == uri
        ));
    }

    #[test]
    fn analysis_transfer_fallback_and_cap_are_marker_exact() {
        let first = Url::parse("file:///workspace/a.R").unwrap();
        let second = Url::parse("file:///workspace/b.R").unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.max_revalidations_per_trigger = 1;
        state.cross_file_activity.active_uri = Some(second.clone());
        let candidates = vec![
            transfer_candidate(&mut state, &second),
            transfer_candidate(&mut state, &first),
        ];
        let finalization = WorldState::begin_analysis_transfer_finalization();
        let committed = state.finalize_analysis_transfer_fallback(finalization, candidates.clone());
        assert!(matches!(
            committed,
            AnalysisTransferFinalization::Committed(ref tickets)
                if tickets.len() == 1 && tickets[0].uri == second
        ));
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&first),
            0,
            "lower-priority cap-dropped candidates must never receive force markers"
        );
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&second),
            1
        );
        assert_eq!(
            state.finalize_analysis_transfer_fallback(finalization, candidates),
            AnalysisTransferFinalization::AlreadyFinalized
        );
        assert_eq!(state.analysis_revalidation_reservation_count, 1);
    }

    #[test]
    fn system_file_last_open_target_rejection_is_atomic() {
        let library = tempfile::tempdir().unwrap();
        let package = library.path().join("otherpkg");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("helper.R"), "helper <- 1\n").unwrap();
        let source = "source(system.file(\"helper.R\", package = \"otherpkg\"))\n";
        let first = Url::parse("file:///workspace/a.R").unwrap();
        let last = Url::parse("file:///workspace/z.R").unwrap();
        let mut state = WorldState::new();
        for uri in [&first, &last] {
            state.open_document(uri.clone(), source, Some(1));
            let generation = state.documents.get_record(uri).unwrap().generation();
            state
                .replace_open_document_metadata_if_current(
                    uri,
                    generation,
                    Arc::new(crate::cross_file::extract_metadata(source)),
                )
                .unwrap();
        }
        let mut package_library = crate::package_library::PackageLibrary::new_empty();
        package_library.set_lib_paths(vec![library.path().to_path_buf()]);
        state.install_package_library(Arc::new(package_library), true);

        let captured = state.capture_system_file_analysis(None);
        let draft = prepare_system_file_analysis(captured);
        let mut prepared = state.finish_system_file_analysis(draft).unwrap();
        prepared.corrupt_last_open_token_for_test();
        let index_version = state.workspace_index.version();
        let graph_revision = state.cross_file_graph.edge_revision();
        let graph_authority = state.workspace_graph_authority_generation;
        let open_context = state.open_context_authority_generation;
        let first_generation = state.documents.get_record(&first).unwrap().generation();
        let last_generation = state.documents.get_record(&last).unwrap().generation();
        let reservations = state.analysis_revalidation_reservation_count;

        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::SystemFile(Box::new(prepared))),
            Err(AnalysisCommitRejected::StaleBasis)
        );
        assert_eq!(state.workspace_index.version(), index_version);
        assert_eq!(state.cross_file_graph.edge_revision(), graph_revision);
        assert_eq!(state.workspace_graph_authority_generation, graph_authority);
        assert_eq!(state.open_context_authority_generation, open_context);
        assert_eq!(
            state.documents.get_record(&first).unwrap().generation(),
            first_generation
        );
        assert_eq!(
            state.documents.get_record(&last).unwrap().generation(),
            last_generation
        );
        assert_eq!(state.analysis_revalidation_reservation_count, reservations);
    }

    #[test]
    fn open_edit_private_only_change_does_not_fan_out() {
        let parent = Url::parse("file:///workspace/parent.R").unwrap();
        let child = Url::parse("file:///workspace/child.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(parent.clone(), "source(\"child.R\")\n", Some(1));
        state.open_document(
            child.clone(),
            "f <- function() {\n  private <- 1\n}\n",
            Some(1),
        );
        state.cross_file_graph.update_file(
            &parent,
            &crate::cross_file::CrossFileMetadata {
                sources: vec![crate::cross_file::ForwardSource {
                    resolved_uri: Some(child.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            None,
            |_| None,
        );

        let effects = commit_test_edit(
            &mut state,
            &child,
            "f <- function() {\n  private <- 2\n}\n",
            crate::cross_file::CrossFileMetadata::default(),
            PreparedOpenCommitPlan::default(),
        )
        .unwrap();
        assert_eq!(
            effects
                .revalidations
                .iter()
                .map(|ticket| &ticket.uri)
                .collect::<Vec<_>>(),
            vec![&child]
        );
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&parent),
            0
        );
    }

    #[test]
    fn open_edit_interface_only_change_fans_out_to_parent() {
        let parent = Url::parse("file:///workspace/parent.R").unwrap();
        let child = Url::parse("file:///workspace/child.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(parent.clone(), "source(\"child.R\")\n", Some(1));
        state.open_document(child.clone(), "exported <- 1\n", Some(1));
        state.cross_file_graph.update_file(
            &parent,
            &crate::cross_file::CrossFileMetadata {
                sources: vec![crate::cross_file::ForwardSource {
                    resolved_uri: Some(child.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            None,
            |_| None,
        );

        let effects = commit_test_edit(
            &mut state,
            &child,
            "renamed_export <- 1\n",
            crate::cross_file::CrossFileMetadata::default(),
            PreparedOpenCommitPlan::default(),
        )
        .unwrap();
        assert!(
            effects
                .revalidations
                .iter()
                .any(|ticket| ticket.uri == parent)
        );
    }

    #[test]
    fn open_edit_edge_only_removal_fans_out_to_removed_child_and_refreshes_pins() {
        let parent = Url::parse("file:///workspace/parent.R").unwrap();
        let child = Url::parse("file:///workspace/child.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(parent.clone(), "source(\"child.R\")\n", Some(1));
        state.open_document(child.clone(), "value <- 1\n", Some(1));
        state.cross_file_graph.update_file(
            &parent,
            &crate::cross_file::CrossFileMetadata {
                sources: vec![crate::cross_file::ForwardSource {
                    resolved_uri: Some(child.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            None,
            |_| None,
        );
        let pin_count = state.open_pin_recompute_count;
        let meta = crate::cross_file::CrossFileMetadata::default();
        let effects = commit_test_edit(
            &mut state,
            &parent,
            "# source removed\n",
            meta.clone(),
            PreparedOpenCommitPlan {
                graph: vec![open_projection(
                    parent.clone(),
                    meta.clone(),
                    None,
                    meta,
                    false,
                )],
                ..PreparedOpenCommitPlan::default()
            },
        )
        .unwrap();
        let uris: HashSet<_> = effects
            .revalidations
            .iter()
            .map(|ticket| ticket.uri.clone())
            .collect();
        assert_eq!(uris, HashSet::from([parent, child.clone()]));
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&child),
            1
        );
        assert_eq!(state.open_pin_recompute_count, pin_count + 1);
    }

    #[test]
    fn open_edit_wd_only_change_fans_out_to_backward_directive_child() {
        let parent = Url::parse("file:///workspace/parent.R").unwrap();
        let child = Url::parse("file:///workspace/child.R").unwrap();
        let root = Url::parse("file:///workspace").unwrap();
        let mut state = WorldState::new();
        state.workspace_folders.push(root.clone());
        state.open_document(parent.clone(), "f <- function() 1\n", Some(1));
        state.open_document(child.clone(), "f()\n", Some(1));
        state.cross_file_graph.update_file(
            &child,
            &crate::cross_file::CrossFileMetadata {
                sourced_by: vec![crate::cross_file::BackwardDirective {
                    path: "parent.R".to_owned(),
                    call_site: crate::cross_file::CallSiteSpec::Default,
                    directive_line: 0,
                }],
                ..Default::default()
            },
            Some(&root),
            |_| None,
        );
        let old_meta = crate::cross_file::CrossFileMetadata {
            working_directory: Some("/old".to_owned()),
            ..Default::default()
        };
        let new_meta = crate::cross_file::CrossFileMetadata {
            working_directory: Some("/new".to_owned()),
            ..Default::default()
        };
        let effects = commit_test_edit(
            &mut state,
            &parent,
            "f <- function() 2\n",
            new_meta.clone(),
            PreparedOpenCommitPlan {
                graph: vec![open_projection(
                    parent.clone(),
                    new_meta.clone(),
                    Some(old_meta),
                    new_meta,
                    false,
                )],
                ..PreparedOpenCommitPlan::default()
            },
        )
        .unwrap();
        assert!(
            effects
                .revalidations
                .iter()
                .any(|ticket| ticket.uri == child)
        );
    }

    #[test]
    fn open_edit_excluded_projection_is_non_lending() {
        let excluded = Url::parse("file:///workspace/excluded.R").unwrap();
        let child = Url::parse("file:///workspace/child.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(excluded.clone(), "# excluded\n", Some(1));
        state.open_document(child.clone(), "value <- 1\n", Some(1));
        let meta = crate::cross_file::CrossFileMetadata {
            sources: vec![crate::cross_file::ForwardSource {
                resolved_uri: Some(child),
                ..Default::default()
            }],
            ..Default::default()
        };
        let pin_count = state.open_pin_recompute_count;
        commit_test_edit(
            &mut state,
            &excluded,
            "# excluded edit\n",
            meta.clone(),
            PreparedOpenCommitPlan {
                graph: vec![open_projection(
                    excluded.clone(),
                    meta.clone(),
                    None,
                    meta,
                    true,
                )],
                refresh_pins: true,
                ..PreparedOpenCommitPlan::default()
            },
        )
        .unwrap();
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&excluded)
                .iter()
                .all(|edge| edge.non_lending)
        );
        assert_eq!(state.open_pin_recompute_count, pin_count + 1);
    }

    #[test]
    fn open_edit_package_only_visibility_change_fans_out() {
        let root = std::path::PathBuf::from("/work/pkg");
        let edited = Url::from_file_path(root.join("R/a.R")).unwrap();
        let sibling = Url::from_file_path(root.join("R/b.R")).unwrap();
        let old_text = "#' @importFrom stats median\nNULL\n";
        let new_text = "#' @importFrom stats sd\nNULL\n";
        let mut state = WorldState::new();
        state.package_inputs.workspace_root = Some(root);
        state.package_inputs.package_mode = crate::cross_file::config::PackageMode::Auto;
        state.package_inputs.description = Some(crate::package_state::DescriptionInput {
            text: Arc::from("Package: pkg\n"),
        });
        let initial = crate::package_state::derive_package_state(
            &state.package_state,
            &state.package_inputs,
            &crate::package_state::PackageInputDelta::Initial,
        );
        state.package_state.set_from(initial);
        let old_delta = crate::package_state::event::translate(
            &mut state.package_inputs,
            crate::package_state::event::HandlerEvent::DidChange {
                uri: edited.clone(),
                text: Arc::from(old_text),
            },
        )
        .unwrap();
        state.record_package_input_mutation();
        state.apply_package_event(&old_delta);
        state.open_document(edited.clone(), old_text, Some(1));
        state.open_document(sibling.clone(), "helper <- 1\n", Some(1));

        let effects = commit_test_edit(
            &mut state,
            &edited,
            new_text,
            crate::cross_file::CrossFileMetadata::default(),
            PreparedOpenCommitPlan {
                package_event: Some((edited.clone(), Arc::from(new_text))),
                package_fanout_uris: vec![sibling.clone()],
                ..PreparedOpenCommitPlan::default()
            },
        )
        .unwrap();
        assert!(
            effects
                .revalidations
                .iter()
                .any(|ticket| ticket.uri == sibling)
        );
    }

    #[test]
    fn prepared_package_projection_advances_both_records_and_routing_owner() {
        let root = std::path::PathBuf::from("/work/pkg");
        let mut state = WorldState::new();
        let mut inputs = state.package_inputs.clone();
        inputs.workspace_root = Some(root);
        inputs.package_mode = crate::cross_file::config::PackageMode::Enabled;
        inputs.description = Some(crate::package_state::DescriptionInput {
            text: Arc::from("Package: pkg\n"),
        });
        let derived = crate::package_state::derive_package_state(
            &state.package_state,
            &inputs,
            &crate::package_state::PackageInputDelta::Initial,
        );
        let input_generation = state.package_input_generation();
        let record_generation = state.package_state_record_generation;
        let routing_owner = state.system_file_routing_owner_generation();

        state.install_prepared_package_projection(PreparedPackageProjection::new(inputs, derived));

        assert_eq!(state.package_input_generation(), input_generation + 1);
        assert_eq!(state.package_state_record_generation, record_generation + 1);
        assert_ne!(state.system_file_routing_owner_generation(), routing_owner);
        assert_eq!(
            state
                .package_state
                .workspace()
                .map(|workspace| workspace.name.as_str()),
            Some("pkg")
        );
    }

    #[test]
    fn prepared_nonrouting_projection_preserves_routing_owner() {
        let root = std::path::PathBuf::from("/work/pkg");
        let mut state = WorldState::new();
        let mut initial_inputs = state.package_inputs.clone();
        initial_inputs.workspace_root = Some(root);
        initial_inputs.package_mode = crate::cross_file::config::PackageMode::Enabled;
        initial_inputs.description = Some(crate::package_state::DescriptionInput {
            text: Arc::from("Package: pkg\n"),
        });
        let initial_state = crate::package_state::derive_package_state(
            &state.package_state,
            &initial_inputs,
            &crate::package_state::PackageInputDelta::Initial,
        );
        state.install_prepared_package_projection(PreparedPackageProjection::new(
            initial_inputs,
            initial_state,
        ));

        let mut namespace_inputs = state.package_inputs.clone();
        namespace_inputs.namespace = Some(crate::package_state::NamespaceInput {
            text: Arc::from("export(foo)\n"),
        });
        let namespace_state = crate::package_state::derive_package_state(
            &state.package_state,
            &namespace_inputs,
            &crate::package_state::PackageInputDelta::NamespaceChanged,
        );
        let record_generation = state.package_state_record_generation;
        let routing_owner = state.system_file_routing_owner_generation();

        state.install_prepared_package_projection(PreparedPackageProjection::new(
            namespace_inputs,
            namespace_state,
        ));

        assert_eq!(state.package_state_record_generation, record_generation + 1);
        assert_eq!(state.system_file_routing_owner_generation(), routing_owner);
    }

    #[test]
    fn value_equal_seed_event_mints_fresh_routing_owner() {
        let root = std::path::PathBuf::from("/work/pkg");
        let mut state = WorldState::new();
        state.package_inputs.workspace_root = Some(root);
        state.package_inputs.package_mode = crate::cross_file::config::PackageMode::Enabled;
        state.package_inputs.description = Some(crate::package_state::DescriptionInput {
            text: Arc::from("Package: pkg\n"),
        });
        state.record_package_input_mutation();
        state.apply_package_seed_event(&crate::package_state::PackageInputDelta::Initial);
        let first_owner = state.system_file_routing_owner_generation();
        let record_generation = state.package_state_record_generation;

        state.record_package_input_mutation();
        state.apply_package_seed_event(&crate::package_state::PackageInputDelta::Initial);

        assert_ne!(
            state.system_file_routing_owner_generation(),
            first_owner,
            "a value-equal seed replay starts a new routing ownership lifecycle"
        );
        assert_eq!(state.package_state_record_generation, record_generation + 1);
    }

    #[test]
    fn open_metadata_seed_union_caps_and_marks_once() {
        let edited = Url::parse("file:///workspace/edited.R").unwrap();
        let first = Url::parse("file:///workspace/first.R").unwrap();
        let evicted = Url::parse("file:///workspace/evicted.R").unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.max_revalidations_per_trigger = 2;
        state.open_document(edited.clone(), "x <- 1\n", Some(1));
        state.open_document(first.clone(), "first <- 1\n", Some(1));
        state.open_document(evicted.clone(), "evicted <- 1\n", Some(1));
        let generation = state.documents.get_record(&edited).unwrap().generation();
        let captured = state
            .capture_open_metadata_derivation(&edited, generation)
            .unwrap();
        let plan = PreparedOpenCommitPlan {
            seed_revalidation_uris: vec![first.clone(), evicted.clone(), first.clone()],
            subject_debounce_ms: Some(77),
            ..PreparedOpenCommitPlan::default()
        };
        let prepared = state
            .prepare_captured_open_metadata_analysis(
                captured,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
                plan,
                Vec::new(),
            )
            .unwrap();
        let effects = state
            .try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(prepared)))
            .unwrap();

        assert_eq!(effects.revalidations.len(), 2);
        assert_eq!(effects.revalidations[0].uri, edited);
        assert_eq!(effects.revalidations[0].debounce_ms, 77);
        let selected = effects.revalidations[1].uri.clone();
        let dropped = if selected == first { evicted } else { first };
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&selected),
            1
        );
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&dropped),
            0
        );
    }

    #[test]
    fn direct_subject_publish_leaves_cap_one_for_dependent() {
        let subject = Url::parse("file:///workspace/excluded.R").unwrap();
        let dependent = Url::parse("file:///workspace/dependent.R").unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.max_revalidations_per_trigger = 1;
        state.open_document(subject.clone(), "x <- 1\n", Some(1));
        state.open_document(dependent.clone(), "y <- x\n", Some(1));

        let effects = commit_test_edit(
            &mut state,
            &subject,
            "x <- 2\n",
            crate::cross_file::CrossFileMetadata::default(),
            PreparedOpenCommitPlan {
                seed_revalidation_uris: vec![subject.clone(), dependent.clone()],
                direct_subject_publish: true,
                ..PreparedOpenCommitPlan::default()
            },
        )
        .unwrap();

        assert_eq!(effects.revalidations.len(), 1);
        assert_eq!(effects.revalidations[0].uri, dependent);
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&subject),
            0
        );
        assert_eq!(
            state
                .diagnostics_gate
                .force_republish_count_for_test(&dependent),
            1
        );
    }

    #[test]
    fn report_link_replacement_revalidates_removed_and_added_documents() {
        use crate::cross_file::types::{CrossFileMetadata, TarchetypesDocumentLink};

        fn report_link(path: &str) -> CrossFileMetadata {
            CrossFileMetadata {
                tarchetypes_document_links: vec![TarchetypesDocumentLink {
                    path: path.to_string(),
                    line: 0,
                    column: 31,
                    end_column: 31 + path.len() as u32,
                }],
                ..Default::default()
            }
        }

        let root = Url::parse("file:///workspace").unwrap();
        let pipeline = Url::parse("file:///workspace/_targets.R").unwrap();
        let old_report = Url::parse("file:///workspace/old.qmd").unwrap();
        let new_report = Url::parse("file:///workspace/new.qmd").unwrap();
        let mut state = WorldState::new();
        state.workspace_folders.push(root.clone());
        state.open_document(
            pipeline.clone(),
            "tarchetypes::tar_quarto(report, \"old.qmd\")\n",
            Some(1),
        );
        state.open_document(old_report.clone(), "```{r}\n1\n```\n", Some(1));
        state.open_document(new_report.clone(), "```{r}\n1\n```\n", Some(1));

        let old_metadata = report_link("old.qmd");
        let new_metadata = report_link("new.qmd");
        state
            .cross_file_graph
            .update_file(&pipeline, &old_metadata, Some(&root), |_| None);
        let effects = commit_test_edit(
            &mut state,
            &pipeline,
            "tarchetypes::tar_quarto(report, \"new.qmd\")\n",
            new_metadata.clone(),
            PreparedOpenCommitPlan {
                graph: vec![open_projection(
                    pipeline.clone(),
                    new_metadata.clone(),
                    Some(old_metadata),
                    new_metadata,
                    false,
                )],
                ..PreparedOpenCommitPlan::default()
            },
        )
        .unwrap();
        let affected: HashSet<_> = effects
            .revalidations
            .into_iter()
            .map(|ticket| ticket.uri)
            .collect();

        assert!(
            affected.contains(&old_report),
            "the removed report endpoint must be retained from the pre-update graph"
        );
        assert!(
            affected.contains(&new_report),
            "the added report endpoint must be discovered from the post-update graph"
        );
    }

    #[test]
    fn open_report_metadata_keeps_reads_but_not_pipeline_authority() {
        let uri = Url::parse("file:///workspace/report.qmd").unwrap();
        let text = r#"---
title: report
---

```{r}
targets::tar_target(fake, 1)
targets::tar_read(fake)
tarchetypes::tar_render(nested, "nested.Rmd")
```
"#;
        let mut state = WorldState::new();
        state.open_document_with_language_id(uri.clone(), text, Some(1), Some("quarto"));
        let metadata = state.documents.get_record(&uri).unwrap().metadata();

        assert!(metadata.target_declarations.is_empty());
        assert!(metadata.tarchetypes_document_links.is_empty());
        assert_eq!(
            metadata
                .target_references
                .iter()
                .map(|reference| reference.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fake"]
        );
    }

    #[test]
    fn open_edit_full_sync_then_utf16_ranges_preserve_rmd_raw_and_masked_views() {
        let uri = Url::parse("file:///workspace/report.Rmd").unwrap();
        let mut state = WorldState::new();
        state.open_document_with_language_id(
            uri.clone(),
            "old prose\n```{r}\nold <- 0\n```\n",
            Some(1),
            Some("rmd"),
        );
        let changes = [
            full_change("intro 🎉\n```{r}\nx <- 1\n```\n"),
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 8,
                    },
                    end: Position {
                        line: 0,
                        character: 8,
                    },
                }),
                range_length: None,
                text: "!".to_owned(),
            },
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 2,
                        character: 0,
                    },
                    end: Position {
                        line: 2,
                        character: 1,
                    },
                }),
                range_length: None,
                text: "中".to_owned(),
            },
        ];
        let edit = state.prepare_document_changes(&uri, changes, 2).unwrap();
        let analysis_text = edit.document().analysis_text();
        assert!(analysis_text.contains("中 <- 1"));
        assert!(!analysis_text.contains("intro"));
        state
            .try_commit_analysis(PreparedAnalysisCommit::OpenEdit(Box::new(
                PreparedOpenEditAnalysis::new(
                    edit,
                    Arc::new(crate::cross_file::extract_metadata(&analysis_text)),
                    PreparedOpenCommitPlan::default(),
                ),
            )))
            .unwrap();
        let document = state.documents.get(&uri).unwrap();
        assert_eq!(document.text(), "intro 🎉!\n```{r}\n中 <- 1\n```\n");
        assert!(document.analysis_text().contains("中 <- 1"));
        assert!(!document.analysis_text().contains("intro"));
    }

    #[test]
    fn open_edit_rejects_metadata_cross_kind_change_without_side_effects() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        let cached = cache_snapshot(7, 11);
        state
            .cross_file_file_cache
            .insert(uri.clone(), cached.clone(), "on disk".to_owned());

        let edit = state
            .prepare_document_changes(&uri, [full_change("stale <- 2\n")], 2)
            .unwrap();
        let generation = state.documents.get_record(&uri).unwrap().generation();
        let mut refreshed = crate::cross_file::CrossFileMetadata::default();
        refreshed.working_directory = Some("/new/context".to_owned());
        state
            .replace_open_document_metadata_if_current(&uri, generation, Arc::new(refreshed))
            .unwrap();
        let record_after_refresh = state.documents.get_record(&uri).unwrap().generation();
        let graph_authority_after_refresh = state.workspace_graph_authority_generation;
        let open_authority_after_refresh = state.open_context_authority_generation;

        assert!(matches!(
            state.commit_document_changes(
                &uri,
                edit,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
            ),
            Err(AnalysisCommitRejected::StaleBasis)
        ));
        assert_eq!(
            state.documents.get_record(&uri).unwrap().generation(),
            record_after_refresh
        );
        assert_eq!(
            state.documents.get(&uri).unwrap().text(),
            "before <- 1\n",
            "stale edit must not overwrite the metadata-refreshed record"
        );
        assert_eq!(state.cross_file_file_cache.get_snapshot(&uri), Some(cached));
        assert_eq!(
            state.workspace_graph_authority_generation,
            graph_authority_after_refresh
        );
        assert_eq!(
            state.open_context_authority_generation,
            open_authority_after_refresh
        );
    }

    #[test]
    fn open_metadata_rejects_edit_cross_kind_change_without_side_effects() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        let stale_basis = state.capture_open_analysis_basis(&uri).unwrap();
        let stale_generation = state.documents.get_record(&uri).unwrap().generation();

        let edit = state
            .prepare_document_changes(&uri, [full_change("after <- 2\n")], 2)
            .unwrap();
        state
            .commit_document_changes(
                &uri,
                edit,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
            )
            .unwrap();
        let committed_generation = state.documents.get_record(&uri).unwrap().generation();
        let graph_authority = state.workspace_graph_authority_generation;
        let open_authority = state.open_context_authority_generation;

        let mut stale_metadata = crate::cross_file::CrossFileMetadata::default();
        stale_metadata.working_directory = Some("/stale".to_owned());
        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(
                PreparedOpenMetadataAnalysis::new(
                    stale_basis,
                    uri.clone(),
                    stale_generation,
                    Arc::new(stale_metadata),
                    PreparedOpenCommitPlan::default(),
                ),
            ))),
            Err(AnalysisCommitRejected::StaleBasis)
        );
        let current = state.documents.get_record(&uri).unwrap();
        assert_eq!(current.generation(), committed_generation);
        assert_eq!(current.document().text(), "after <- 2\n");
        assert_eq!(
            current.metadata().working_directory,
            None,
            "stale metadata must not replace the edited record"
        );
        assert_eq!(state.workspace_graph_authority_generation, graph_authority);
        assert_eq!(state.open_context_authority_generation, open_authority);
    }

    fn prepared_captured_metadata(
        state: &WorldState,
        captured: CapturedOpenMetadataAnalysis,
    ) -> PreparedOpenMetadataAnalysis {
        state
            .prepare_captured_open_metadata_analysis(
                captured,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
                PreparedOpenCommitPlan::default(),
                Vec::new(),
            )
            .unwrap()
    }

    #[test]
    fn open_metadata_rejects_changed_raw_and_closed_snapshot_authorities() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let raw_parent = Url::parse("file:///workspace/raw-parent.R").unwrap();
        let closed_parent = Url::parse("file:///workspace/closed-parent.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        state.cross_file_file_cache.insert(
            raw_parent.clone(),
            cache_snapshot(1, 1),
            "raw <- 1\n".to_owned(),
        );
        state.insert_workspace_document_for_test(
            closed_parent.clone(),
            Document::new_with_uri("closed <- 1\n", None, &closed_parent),
        );
        let generation = state.documents.get_record(&uri).unwrap().generation();

        let captured_raw = state
            .capture_open_metadata_derivation(&uri, generation)
            .unwrap();
        state.cross_file_file_cache.insert(
            raw_parent,
            cache_snapshot(2, 2),
            "raw <- 2\n".to_owned(),
        );
        let prepared = prepared_captured_metadata(&state, captured_raw);
        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(prepared))),
            Err(AnalysisCommitRejected::StaleBasis)
        );

        let generation = state.documents.get_record(&uri).unwrap().generation();
        let captured_closed = state
            .capture_open_metadata_derivation(&uri, generation)
            .unwrap();
        state.insert_workspace_document_for_test(
            closed_parent.clone(),
            Document::new_with_uri("closed <- 2\n", None, &closed_parent),
        );
        let prepared = prepared_captured_metadata(&state, captured_closed);
        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(prepared))),
            Err(AnalysisCommitRejected::StaleBasis)
        );
        assert_eq!(state.documents.get(&uri).unwrap().text(), "before <- 1\n");
    }

    #[test]
    fn open_metadata_rejects_changed_target_raw_authority() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        state.cross_file_file_cache.insert(
            uri.clone(),
            cache_snapshot(1, 1),
            "disk <- 1\n".to_owned(),
        );
        let generation = state.documents.get_record(&uri).unwrap().generation();
        let captured = state
            .capture_open_metadata_derivation(&uri, generation)
            .unwrap();
        state.cross_file_file_cache.insert(
            uri.clone(),
            cache_snapshot(2, 2),
            "disk <- 2\n".to_owned(),
        );
        let prepared = prepared_captured_metadata(&state, captured);
        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(prepared))),
            Err(AnalysisCommitRejected::StaleBasis)
        );
        assert_eq!(state.documents.get(&uri).unwrap().text(), "before <- 1\n");
    }

    #[test]
    fn open_metadata_rejects_changed_open_context_and_editor_eligibility() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let parent = Url::parse("file:///workspace/parent.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        state.open_document(parent.clone(), "parent <- 1\n", Some(1));
        let generation = state.documents.get_record(&uri).unwrap().generation();

        let captured_open = state
            .capture_open_metadata_derivation(&uri, generation)
            .unwrap();
        let parent_generation = state.documents.get_record(&parent).unwrap().generation();
        state
            .replace_open_document_metadata_if_current(
                &parent,
                parent_generation,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
            )
            .unwrap();
        let prepared = prepared_captured_metadata(&state, captured_open);
        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(prepared))),
            Err(AnalysisCommitRejected::StaleBasis)
        );

        let generation = state.documents.get_record(&uri).unwrap().generation();
        let captured_eligibility = state
            .capture_open_metadata_derivation(&uri, generation)
            .unwrap();
        state.replace_editor_diagnostic_uris(Some(HashSet::new()));
        let prepared = prepared_captured_metadata(&state, captured_eligibility);
        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(prepared))),
            Err(AnalysisCommitRejected::StaleBasis)
        );
        assert_eq!(state.documents.get(&uri).unwrap().text(), "before <- 1\n");
    }

    #[test]
    fn open_edit_rejects_changed_raw_authority_and_preserves_new_cache_entry() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        state.cross_file_file_cache.insert(
            uri.clone(),
            cache_snapshot(3, 1),
            "old disk".to_owned(),
        );
        let edit = state
            .prepare_document_changes(&uri, [full_change("stale <- 2\n")], 2)
            .unwrap();
        let new_snapshot = cache_snapshot(4, 2);
        state.cross_file_file_cache.insert(
            uri.clone(),
            new_snapshot.clone(),
            "new disk".to_owned(),
        );

        assert!(matches!(
            state.commit_document_changes(
                &uri,
                edit,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
            ),
            Err(AnalysisCommitRejected::StaleBasis)
        ));
        assert_eq!(
            state.cross_file_file_cache.get_snapshot(&uri),
            Some(new_snapshot)
        );
        assert_eq!(
            state.cross_file_file_cache.get(&uri).as_deref(),
            Some("new disk")
        );
        assert_eq!(state.documents.get(&uri).unwrap().text(), "before <- 1\n");
    }

    #[test]
    fn open_edit_rejects_editor_eligibility_context_change() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        let edit = state
            .prepare_document_changes(&uri, [full_change("stale <- 2\n")], 2)
            .unwrap();
        state.replace_editor_diagnostic_uris(Some(HashSet::new()));
        let open_authority = state.open_context_authority_generation;

        assert!(matches!(
            state.commit_document_changes(
                &uri,
                edit,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
            ),
            Err(AnalysisCommitRejected::StaleBasis)
        ));
        assert_eq!(state.open_context_authority_generation, open_authority);
        assert_eq!(state.documents.get(&uri).unwrap().text(), "before <- 1\n");
    }

    #[test]
    fn open_edit_rejects_changed_raw_parent_context() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let parent = Url::parse("file:///workspace/parent.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        let edit = state
            .prepare_document_changes(&uri, [full_change("stale <- 2\n")], 2)
            .unwrap();
        let edit = state
            .attach_open_edit_context_authorities(edit, vec![parent.clone()])
            .ok()
            .unwrap();
        state.cross_file_file_cache.insert(
            parent.clone(),
            cache_snapshot(3, 9),
            "new parent".to_owned(),
        );

        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenEdit(Box::new(
                PreparedOpenEditAnalysis::new(
                    edit,
                    Arc::new(crate::cross_file::CrossFileMetadata::default()),
                    PreparedOpenCommitPlan::default(),
                ),
            ))),
            Err(AnalysisCommitRejected::StaleBasis)
        );
        assert_eq!(
            state.cross_file_file_cache.get(&parent).as_deref(),
            Some("new parent")
        );
        assert_eq!(state.documents.get(&uri).unwrap().text(), "before <- 1\n");
    }

    #[test]
    fn open_edit_rejects_changed_closed_parent_context() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let parent = Url::parse("file:///workspace/parent.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        state.insert_workspace_document_for_test(
            parent.clone(),
            Document::new_with_uri("parent <- 1\n", None, &parent),
        );
        let edit = state
            .prepare_document_changes(&uri, [full_change("stale <- 2\n")], 2)
            .unwrap();
        let edit = state
            .attach_open_edit_context_authorities(edit, vec![parent.clone()])
            .ok()
            .unwrap();
        state.insert_workspace_document_for_test(
            parent.clone(),
            Document::new_with_uri("parent <- 2\n", None, &parent),
        );

        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenEdit(Box::new(
                PreparedOpenEditAnalysis::new(
                    edit,
                    Arc::new(crate::cross_file::CrossFileMetadata::default()),
                    PreparedOpenCommitPlan::default(),
                ),
            ))),
            Err(AnalysisCommitRejected::StaleBasis)
        );
        assert_eq!(state.documents.get(&uri).unwrap().text(), "before <- 1\n");
    }

    #[test]
    fn open_edit_rejects_changed_open_parent_context() {
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        let parent = Url::parse("file:///workspace/parent.R").unwrap();
        let mut state = WorldState::new();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        state.open_document(parent.clone(), "parent <- 1\n", Some(1));
        let edit = state
            .prepare_document_changes(&uri, [full_change("stale <- 2\n")], 2)
            .unwrap();
        let generation = state.documents.get_record(&parent).unwrap().generation();
        state
            .replace_open_document_metadata_if_current(
                &parent,
                generation,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
            )
            .unwrap();

        assert_eq!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenEdit(Box::new(
                PreparedOpenEditAnalysis::new(
                    edit,
                    Arc::new(crate::cross_file::CrossFileMetadata::default()),
                    PreparedOpenCommitPlan::default(),
                ),
            ))),
            Err(AnalysisCommitRejected::StaleBasis)
        );
        assert_eq!(state.documents.get(&uri).unwrap().text(), "before <- 1\n");
    }

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

    #[test]
    fn direct_workspace_apply_builds_graph_from_artifact_only_entries() {
        use crate::cross_file::file_cache::FileSnapshot;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        use crate::workspace_index::{IndexEntry, WorkspaceIndexConfig};

        let parent = Url::parse("file:///workspace/main.R").unwrap();
        let child = Url::parse("file:///workspace/lib.R").unwrap();
        let make_entry = |metadata| IndexEntry {
            contents: Rope::from_str("x <- 1\n"),
            tree: None,
            loaded_packages: Vec::new(),
            data_packages: Vec::new(),
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: None,
            },
            metadata: Arc::new(metadata),
            artifacts: Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 0,
        };
        let mut entries = HashMap::new();
        entries.insert(child.clone(), make_entry(CrossFileMetadata::default()));
        entries.insert(
            parent.clone(),
            make_entry(CrossFileMetadata {
                sources: vec![ForwardSource {
                    resolved_uri: Some(child.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        );

        let mut state = WorldState::new();
        state.workspace_index = crate::workspace_index::WorkspaceIndex::new(WorkspaceIndexConfig {
            max_files: 1,
            ..Default::default()
        });
        state.workspace_index.resize_artifacts(2);
        state.apply_workspace_index(entries);

        assert_eq!(state.workspace_index.len(), 1);
        assert_eq!(state.workspace_index.artifact_uris().len(), 2);
        let dependencies = state.cross_file_graph.get_dependencies(&parent);
        assert_eq!(dependencies.len(), 1);
        assert_eq!(dependencies[0].to, child);
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

        let index = scan_workspace_with_exclusions(&[root], 20, &exclusions);
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
    fn changed_exclusions_refinalize_open_shiny_and_closed_tar_source_batches() {
        use serde_json::json;
        use std::fs;

        let tmp = tempfile::TempDir::new().unwrap();
        let shiny_root = tmp.path().join("shiny");
        let pipeline_root = tmp.path().join("pipeline");
        fs::create_dir_all(shiny_root.join("R")).unwrap();
        fs::create_dir_all(pipeline_root.join("R")).unwrap();
        fs::write(shiny_root.join("app.R"), "shiny_helper\n").unwrap();
        fs::write(shiny_root.join("R/helper.R"), "shiny_helper <- 1\n").unwrap();
        fs::write(
            pipeline_root.join("_targets.R"),
            "targets::tar_source(\"R\")\ntar_helper\n",
        )
        .unwrap();
        fs::write(pipeline_root.join("R/helper.R"), "tar_helper <- 1\n").unwrap();

        let root_uri = Url::from_directory_path(tmp.path()).unwrap();
        let app_uri = Url::from_file_path(shiny_root.join("app.R")).unwrap();
        let targets_uri = Url::from_file_path(pipeline_root.join("_targets.R")).unwrap();
        let mut entries = scan_workspace(std::slice::from_ref(&root_uri), 20);

        let app_entry = entries.get(&app_uri).unwrap();
        let shiny_helper_uri = app_entry
            .metadata
            .sources
            .iter()
            .find_map(|source| {
                source
                    .is_source_batch_member()
                    .then_some(source.resolved_uri.clone())
            })
            .flatten()
            .expect("initial Shiny expansion must contain its helper");
        let tar_helper_uri = entries[&targets_uri]
            .metadata
            .sources
            .iter()
            .find_map(|source| {
                source
                    .is_source_batch_member()
                    .then_some(source.resolved_uri.clone())
            })
            .flatten()
            .expect("initial tar_source expansion must contain its helper");
        let open_overlay = WorkspaceGraphOverlay {
            uri: app_uri.clone(),
            content: app_entry.contents.clone(),
            chunk_kind: ChunkKind::R,
            metadata: Some(app_entry.metadata.clone()),
            graph_roots: vec![app_uri.clone()],
            excluded: false,
        };
        let exclusions = crate::config_file::compile_workspace_exclusions(
            &json!({
                "workspace": {
                    "exclude": ["shiny/R/**", "pipeline/R/**"]
                }
            }),
            vec![tmp.path().to_path_buf()],
        );
        let context = WorkspaceGraphDerivationContext {
            workspace_root: Some(root_uri),
            max_depth: 20,
            exclusions,
            system_file_workspace_name: None,
            system_file_workspace_root: None,
            system_file_library_paths: Vec::new(),
        };

        let (graph, open_metadata) =
            derive_workspace_dependency_graph(&mut entries, None, &[open_overlay], &context, false);

        assert!(
            open_metadata[&app_uri]
                .sources
                .iter()
                .all(|source| source.resolved_uri.as_ref() != Some(&shiny_helper_uri)),
            "changed exclusions must invalidate the open Shiny expansion"
        );
        assert!(
            entries[&targets_uri]
                .metadata
                .sources
                .iter()
                .all(|source| source.resolved_uri.as_ref() != Some(&tar_helper_uri)),
            "changed exclusions must invalidate the closed tar_source expansion"
        );
        assert!(
            graph
                .get_dependencies(&app_uri)
                .iter()
                .all(|edge| edge.to != shiny_helper_uri),
            "excluded Shiny helpers must leave the dependency graph"
        );
        assert!(
            graph
                .get_dependencies(&targets_uri)
                .iter()
                .all(|edge| edge.to != tar_helper_uri),
            "excluded tar_source members must leave the dependency graph"
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
    fn stat_model_extensions_include_all_jags_suffixes_case_insensitively() {
        for path in [
            "model.jags",
            "model.JAGS",
            "model.bugs",
            "model.BUGS",
            "model.bug",
            "model.BUG",
        ] {
            assert!(is_stat_model_extension(Path::new(path)), "{path}");
        }
        assert!(!is_stat_model_extension(Path::new("model.bugx")));
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
    fn document_change_batch_applies_sequential_utf16_edits_and_parses_once() {
        let mut doc = Document::new("a🎉b\nlibrary(old)", Some(1));
        DOCUMENT_PARSE_COUNT.with(|count| count.set(0));

        doc.apply_changes([
            TextDocumentContentChangeEvent {
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
            },
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 1,
                        character: 8,
                    },
                    end: Position {
                        line: 1,
                        character: 11,
                    },
                }),
                range_length: None,
                text: "new".to_string(),
            },
        ]);

        assert_eq!(doc.text(), "a🎉xb\nlibrary(new)");
        assert_eq!(doc.loaded_packages, vec!["new"]);
        assert_eq!(doc.revision, 2, "revision remains per content event");
        DOCUMENT_PARSE_COUNT.with(|count| {
            assert_eq!(
                count.get(),
                1,
                "one didChange batch must rebuild the analysis tree once"
            );
        });
    }

    #[test]
    fn stan_document_parses_once_per_batch_and_diagnostics_reuse_the_tree() {
        DOCUMENT_PARSE_COUNT.with(|count| count.set(0));
        let mut document = Document::new_with_file_type(
            "parameters { real x; }\nmodel { x ~ normal(0, 1); }\n",
            Some(1),
            FileType::Stan,
        );
        DOCUMENT_PARSE_COUNT.with(|count| assert_eq!(count.get(), 1));
        DOCUMENT_PARSE_COUNT.with(|count| count.set(0));
        document.apply_changes([
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 18), Position::new(0, 19))),
                range_length: None,
                text: "y".to_string(),
            },
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(1, 8), Position::new(1, 9))),
                range_length: None,
                text: "y".to_string(),
            },
        ]);
        DOCUMENT_PARSE_COUNT.with(|count| assert_eq!(count.get(), 1));

        let uri = Url::parse("untitled:stan-parse-count").unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.stan_diagnostics_enabled = true;
        state.open_document_with_language_id(uri.clone(), &document.text(), Some(2), Some("stan"));
        DOCUMENT_PARSE_COUNT.with(|count| count.set(0));
        let snapshot = crate::handlers::DiagnosticsSnapshot::build(&state, &uri).unwrap();
        let findings = crate::handlers::diagnostics_from_snapshot(
            &snapshot,
            &uri,
            &crate::handlers::DiagCancelToken::never(),
        )
        .unwrap();
        assert!(findings.is_empty());
        DOCUMENT_PARSE_COUNT.with(|count| {
            assert_eq!(
                count.get(),
                0,
                "diagnostics must reuse the stored Stan tree"
            )
        });
    }

    #[test]
    fn jags_document_uses_native_tree_and_keeps_r_analysis_inert() {
        let uri = Url::parse("file:///workspace/model.jags").unwrap();
        let source = "model { library(fake) <- 1; x ~ dunknown(y) }\n";
        let document = Document::new_with_uri(source, Some(1), &uri);

        assert_eq!(document.file_type, FileType::Jags);
        assert_eq!(
            document.tree.as_ref().unwrap().root_node().kind(),
            "program"
        );
        assert!(document.loaded_packages.is_empty());
        assert!(document.data_packages.is_empty());
        let metadata = document.cross_file_metadata();
        assert!(metadata.sources.is_empty());
        assert!(metadata.box_imports.is_empty());
        assert!(metadata.import_calls.is_empty());
        let artifacts = document.cross_file_artifacts(&uri, &metadata);
        assert!(artifacts.exported_interface.is_empty());
        assert!(artifacts.timeline.is_empty());
    }

    #[test]
    fn jags_batch_reparse_matches_fresh_tree_and_diagnostics_reuse_it() {
        let original = "model { x <- 1 }\n";
        let changed = "model { x <- * 1 }\n";
        let uri = Url::parse("untitled:jags-parse-count").unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.jags_diagnostics_enabled = true;
        state.open_document_with_language_id(uri.clone(), original, Some(1), Some("jags"));

        DOCUMENT_PARSE_COUNT.with(|count| count.set(0));
        JAGS_INCREMENTAL_PARSE_COUNT.with(|count| count.set(0));
        let prepared = state
            .prepare_document_changes(
                &uri,
                [TextDocumentContentChangeEvent {
                    range: Some(Range::new(Position::new(0, 13), Position::new(0, 13))),
                    range_length: None,
                    text: "* ".to_string(),
                }],
                2,
            )
            .unwrap();
        assert_eq!(prepared.document().text(), changed);
        DOCUMENT_PARSE_COUNT.with(|count| assert_eq!(count.get(), 1));
        JAGS_INCREMENTAL_PARSE_COUNT.with(|count| assert_eq!(count.get(), 1));
        state
            .commit_document_changes(
                &uri,
                prepared,
                Arc::new(crate::cross_file::CrossFileMetadata::default()),
            )
            .unwrap();

        let fresh_document = Document::new_with_file_type(changed, Some(2), FileType::Jags);
        let incremental_document = state.documents.get(&uri).unwrap();
        assert_eq!(incremental_document.text(), changed);
        assert_eq!(
            incremental_document
                .tree
                .as_ref()
                .unwrap()
                .root_node()
                .to_sexp(),
            fresh_document.tree.as_ref().unwrap().root_node().to_sexp()
        );
        assert_eq!(
            incremental_document
                .tree
                .as_ref()
                .unwrap()
                .root_node()
                .range(),
            fresh_document.tree.as_ref().unwrap().root_node().range()
        );

        DOCUMENT_PARSE_COUNT.with(|count| count.set(0));
        let snapshot = crate::handlers::DiagnosticsSnapshot::build(&state, &uri).unwrap();
        let incremental_findings = crate::handlers::diagnostics_from_snapshot(
            &snapshot,
            &uri,
            &crate::handlers::DiagCancelToken::never(),
        )
        .unwrap();
        assert!(!incremental_findings.is_empty());
        DOCUMENT_PARSE_COUNT.with(|count| {
            assert_eq!(
                count.get(),
                0,
                "diagnostics must reuse the incrementally edited JAGS tree"
            )
        });

        let fresh_uri = Url::parse("untitled:fresh-jags-parse-count").unwrap();
        let mut fresh_state = WorldState::new();
        fresh_state.cross_file_config.jags_diagnostics_enabled = true;
        fresh_state.open_document_with_language_id(
            fresh_uri.clone(),
            changed,
            Some(2),
            Some("jags"),
        );
        let fresh_snapshot =
            crate::handlers::DiagnosticsSnapshot::build(&fresh_state, &fresh_uri).unwrap();
        let fresh_findings = crate::handlers::diagnostics_from_snapshot(
            &fresh_snapshot,
            &fresh_uri,
            &crate::handlers::DiagCancelToken::never(),
        )
        .unwrap();
        assert_eq!(incremental_findings, fresh_findings);
    }

    #[test]
    fn jags_incremental_utf16_batch_uses_one_edited_old_tree_and_matches_fresh_parse() {
        let original = "model {\n  /* 💥 */ x <- 1\n}\n";
        let expected = "model {\n  /* 💥 */ y <- 2\n}\n";
        let mut document = Document::new_with_file_type(original, Some(1), FileType::Jags);
        DOCUMENT_PARSE_COUNT.with(|count| count.set(0));
        JAGS_INCREMENTAL_PARSE_COUNT.with(|count| count.set(0));

        document.apply_changes([
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(1, 11), Position::new(1, 12))),
                range_length: None,
                text: "y".to_string(),
            },
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(1, 16), Position::new(1, 17))),
                range_length: None,
                text: "2".to_string(),
            },
        ]);

        assert_eq!(document.text(), expected);
        DOCUMENT_PARSE_COUNT.with(|count| assert_eq!(count.get(), 1));
        JAGS_INCREMENTAL_PARSE_COUNT.with(|count| assert_eq!(count.get(), 1));
        let fresh = Document::new_with_file_type(expected, Some(2), FileType::Jags);
        assert_eq!(
            document.tree.as_ref().unwrap().root_node().to_sexp(),
            fresh.tree.as_ref().unwrap().root_node().to_sexp()
        );
        assert_eq!(
            document.tree.as_ref().unwrap().root_node().range(),
            fresh.tree.as_ref().unwrap().root_node().range()
        );
    }

    #[test]
    fn document_loaded_packages_excludes_conditional_bare_p_load() {
        let generic = Document::new("p_load <- function(...) NULL\np_load(not_a_package)", None);
        assert!(
            generic.loaded_packages.is_empty(),
            "a locally defined p_load must not contribute package metadata"
        );

        let inactive = Document::new("p_load(not_a_package)", None);
        assert!(inactive.loaded_packages.is_empty());

        let active = Document::new("library(pacman)\np_load(dplyr)", None);
        assert_eq!(active.loaded_packages, vec!["pacman"]);

        let ordered = Document::new("p_load(before)\nlibrary(pacman)\np_load(after)", None);
        assert_eq!(ordered.loaded_packages, vec!["pacman"]);

        let qualified = Document::new("pacman::p_load(ggplot2)", None);
        assert_eq!(qualified.loaded_packages, vec!["ggplot2"]);
    }

    #[test]
    fn document_loaded_packages_includes_targets_pipeline_packages() {
        let document = Document::new(
            "targets::tar_option_set(packages = c(\"dplyr\", \"tidyr\"))",
            None,
        );
        assert_eq!(document.loaded_packages, ["dplyr", "tidyr"]);
    }

    #[test]
    fn document_loaded_packages_uses_static_loop_packages_not_iterator_name() {
        let document = Document::new(
            "packages <- c(\"alpha\", \"beta\", NULL)\n\
             for (package in packages) library(package, character.only = TRUE)",
            None,
        );
        assert_eq!(document.loaded_packages, ["alpha", "beta"]);

        let dynamic = Document::new(
            "for (package in packages) library(package, character.only = TRUE)",
            None,
        );
        assert!(dynamic.loaded_packages.is_empty());
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
                        chdir: false,
                        is_sys_source: false,
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
        let interface = &processed.entry.artifacts.exported_interface;
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
            .entry
            .tree
            .as_ref()
            .expect("Rmd doc must have a tree");
        let raw = processed.entry.contents.to_string();
        let analysis =
            crate::cross_file::analysis_text_for_kind(crate::chunks::ChunkKind::Rmd, &raw);
        assert!(
            tree_has_identifier(doc_tree, &analysis, "chunk_symbol"),
            "masked tree must contain the chunk-defined identifier"
        );
    }

    /// The detached system-file transaction is what every library-swap site
    /// (startup post-ready retry, `raven.refreshPackages`, and config
    /// reconciliation) uses to re-resolve deferred
    /// `system.file()` sources once `lib_paths` become available. Exercise the
    /// full wiring at the `WorldState` level: a workspace-index entry whose
    /// source was deferred (indexed while `lib_paths` was empty) must resolve
    /// in place after the library swap, not just in a detached metadata value.
    #[tokio::test]
    async fn resolve_system_file_in_workspace_re_resolves_after_library_swap() {
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
            data_packages: vec![],
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: Some(1),
            },
            metadata: Arc::new(metadata),
            artifacts: Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 1,
        };

        let filler = entry.clone();
        let mut state = WorldState::new();
        state.workspace_index = crate::workspace_index::WorkspaceIndex::new(
            crate::workspace_index::WorkspaceIndexConfig {
                max_files: 1,
                ..Default::default()
            },
        );
        state.workspace_index.insert(uri.clone(), entry);
        state
            .workspace_index
            .insert(Url::parse("file:///workspace/filler.R").unwrap(), filler);
        assert!(
            state.workspace_index.get(&uri).is_none(),
            "precondition: the system.file source is artifact-only"
        );

        // Before the swap: lib_paths is empty, so the source stays deferred.
        crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("deferred convergence should commit");
        let deferred = state
            .workspace_index
            .get_artifact_entry(&uri)
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
        crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("post-library convergence should commit");

        let resolved = state
            .workspace_index
            .get_artifact_entry(&uri)
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

    #[tokio::test]
    async fn system_file_prepared_graph_pins_all_new_external_targets_before_admission() {
        let library = tempfile::tempdir().unwrap();
        let package = library.path().join("otherpkg");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("first.R"), "first_value <- 1\n").unwrap();
        std::fs::write(package.join("second.R"), "second_value <- 2\n").unwrap();
        let source = Url::parse("file:///workspace/source.R").unwrap();
        let text = concat!(
            "source(system.file(\"first.R\", package = \"otherpkg\"))\n",
            "source(system.file(\"second.R\", package = \"otherpkg\"))\n",
        );
        let mut state = WorldState::new();
        state.workspace_index = crate::workspace_index::WorkspaceIndex::new(
            crate::workspace_index::WorkspaceIndexConfig {
                max_files: 1,
                ..Default::default()
            },
        );
        state.open_document(source.clone(), text, Some(1));
        let generation = state.documents.get_record(&source).unwrap().generation();
        state
            .replace_open_document_metadata_if_current(
                &source,
                generation,
                Arc::new(crate::cross_file::extract_metadata(text)),
            )
            .unwrap();
        let mut package_library = crate::package_library::PackageLibrary::new_empty();
        package_library.set_lib_paths(vec![library.path().to_path_buf()]);
        state.install_package_library(Arc::new(package_library), true);

        crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("system.file convergence should commit");

        let first = Url::from_file_path(package.join("first.R")).unwrap();
        let second = Url::from_file_path(package.join("second.R")).unwrap();
        assert!(
            state.workspace_index.get(&first).is_some(),
            "the first newly reachable closed target must survive max_files=1"
        );
        assert!(
            state.workspace_index.get(&second).is_some(),
            "the second newly reachable closed target must survive max_files=1"
        );
        let dependencies = state.cross_file_graph.get_dependencies(&source);
        assert!(dependencies.iter().any(|edge| edge.to == first));
        assert!(dependencies.iter().any(|edge| edge.to == second));
    }

    #[tokio::test]
    async fn system_file_refreshes_changed_dynamic_target_and_retains_invalid_bytes() {
        let library = tempfile::tempdir().unwrap();
        let package = library.path().join("otherpkg");
        std::fs::create_dir_all(&package).unwrap();
        let helper_path = package.join("helper.R");
        std::fs::write(&helper_path, "old_symbol <- 1\n").unwrap();
        let helper = Url::from_file_path(&helper_path).unwrap();
        let source = Url::parse("file:///workspace/dynamic-source.R").unwrap();
        let text = "source(system.file(\"helper.R\", package = \"otherpkg\"))\n";
        let mut state = WorldState::new();
        state.open_document(source.clone(), text, Some(1));
        let generation = state.documents.get_record(&source).unwrap().generation();
        state
            .replace_open_document_metadata_if_current(
                &source,
                generation,
                Arc::new(crate::cross_file::extract_metadata(text)),
            )
            .unwrap();
        let mut package_library = crate::package_library::PackageLibrary::new_empty();
        package_library.set_lib_paths(vec![library.path().to_path_buf()]);
        state.install_package_library(Arc::new(package_library), true);

        crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("initial dynamic target install commits");
        assert!(
            state
                .workspace_index
                .get(&helper)
                .unwrap()
                .artifacts
                .exported_interface
                .keys()
                .any(|name| &**name == "old_symbol")
        );

        std::fs::write(&helper_path, "new_symbol <- 2\n").unwrap();
        let captured = state.capture_system_file_analysis(None);
        let draft = prepare_system_file_analysis(captured);
        let prepared = state
            .finish_system_file_analysis(draft)
            .expect("same-URI dynamic refresh remains admissible");
        let effects = state
            .try_commit_analysis(PreparedAnalysisCommit::SystemFile(Box::new(prepared)))
            .expect("same-URI dynamic refresh commits");
        let transfer = effects
            .system_file
            .expect("changed content installs a fanout handoff");
        assert!(
            state
                .analysis_transfer_candidate_uris_for_test(transfer.handle)
                .contains(&source),
            "the open dependent must be owned by the changed-target fanout"
        );
        let refreshed = state.workspace_index.get(&helper).unwrap();
        assert_eq!(refreshed.contents.to_string(), "new_symbol <- 2\n");
        assert!(
            refreshed
                .artifacts
                .exported_interface
                .keys()
                .any(|name| &**name == "new_symbol")
        );
        let unchanged = crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("identical dynamic observation commits as a no-op");
        assert!(
            unchanged.is_empty(),
            "identical bytes must not create content/interface fanout"
        );

        std::fs::write(&helper_path, [0xff]).unwrap();
        crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("invalid observation is a retaining no-op");
        assert_eq!(
            state
                .workspace_index
                .get(&helper)
                .unwrap()
                .contents
                .to_string(),
            "new_symbol <- 2\n"
        );
    }

    #[test]
    fn artifact_only_source_protects_resolved_external_target_from_orphan_cleanup() {
        use crate::cross_file::file_cache::FileSnapshot;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        use crate::workspace_index::{IndexEntry, WorkspaceIndexConfig};

        let target = Url::parse("file:///external/helper.R").unwrap();
        let source = Url::parse("file:///workspace/source.R").unwrap();
        let make_entry = |metadata| IndexEntry {
            contents: Rope::from_str("x <- 1\n"),
            tree: None,
            loaded_packages: Vec::new(),
            data_packages: Vec::new(),
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: None,
            },
            metadata: Arc::new(metadata),
            artifacts: Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 0,
        };
        let mut state = WorldState::new();
        state.workspace_index = WorkspaceIndex::new(WorkspaceIndexConfig {
            max_files: 1,
            ..Default::default()
        });
        state.workspace_index.insert(
            source.clone(),
            make_entry(CrossFileMetadata {
                sources: vec![ForwardSource {
                    resolved_uri: Some(target.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
        );
        state
            .workspace_index
            .insert(target.clone(), make_entry(CrossFileMetadata::default()));
        assert!(state.workspace_index.get(&source).is_none());
        assert!(state.workspace_index.is_complete(&source));

        state.drop_orphaned_external_entries(HashSet::from([target.clone()]));

        assert!(
            state.workspace_index.is_complete(&target),
            "artifact-only sources must protect their resolved target"
        );
    }

    #[test]
    fn pending_external_target_is_not_synchronously_overwritten() {
        use crate::cross_file::file_cache::FileSnapshot;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        use crate::workspace_index::{ClaimEnrichment, ClosedProvenance, IndexEntry};

        let tmp = tempfile::tempdir().unwrap();
        let target_path = tmp.path().join("helper.R");
        std::fs::write(&target_path, "helper_value <- 1\n").unwrap();
        let target = Url::from_file_path(&target_path).unwrap();
        let source = Url::parse("file:///workspace/source.R").unwrap();
        let source_entry = IndexEntry {
            contents: Rope::from_str("source(system.file(\"helper.R\", package=\"p\"))\n"),
            tree: None,
            loaded_packages: Vec::new(),
            data_packages: Vec::new(),
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: None,
            },
            metadata: Arc::new(CrossFileMetadata {
                sources: vec![ForwardSource {
                    resolved_uri: Some(target.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            artifacts: Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 0,
        };
        let mut state = WorldState::new();
        state.workspace_index.insert(source, source_entry);
        let claim = match state
            .workspace_index
            .claim_enrichment(target.clone(), ClosedProvenance::Dynamic)
        {
            ClaimEnrichment::Claimed(claim) => claim,
            other => panic!("expected target claim, got {other:?}"),
        };

        state.index_cross_package_resolved_files();

        assert_eq!(
            state.workspace_index.enrichment_status(&target),
            Some(crate::workspace_index::EnrichmentStatus::Pending)
        );
        assert!(state.workspace_index.get(&target).is_none());
        state
            .workspace_index
            .abort_enrichment(&claim)
            .expect("original worker still owns Pending");
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

        assert_eq!(
            state.open_document_uri_for_authoritative_uri(&canonical_uri),
            Some(link_uri.clone()),
            "canonical child URI must resolve to the symlink-open buffer"
        );

        crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("deferred convergence should commit");
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
        let changed = crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("post-library convergence should commit");

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

    #[test]
    fn open_package_warm_basis_rejects_a_new_open_record() {
        let mut state = WorldState::new();
        let candidate = Arc::new(PackageLibrary::new_empty());
        let mut basis = state.capture_open_package_warm_basis(&candidate);
        basis.record_successfully_warmed(HashSet::new());
        assert!(state.open_package_warm_basis_is_current(&basis, &candidate));

        let uri = Url::parse("file:///warm-basis-new-open.R").unwrap();
        state.open_document(uri, "library(newpkg)\n", Some(1));
        assert!(
            !state.open_package_warm_basis_is_current(&basis, &candidate),
            "an open installed after package collection must force clear + rewarm"
        );
    }

    #[tokio::test]
    async fn replacement_intent_rebases_only_across_additive_content() {
        let mut state = WorldState::new();
        let library = state.package_library.clone();
        let lease = library.routing_lease().await;
        let epoch = library.cache_operation_epoch(&lease);
        let basis = state
            .capture_library_routing_basis(
                &library,
                epoch,
                LibraryRoutingMutation::Replacement,
                None,
            )
            .unwrap();
        drop(lease);

        state.record_package_library_content_change();
        let lease = library.routing_lease().await;
        let additive_epoch = library.cache_operation_epoch(&lease);
        let rebased = state
            .rebase_library_replacement_basis(&basis, &library, additive_epoch)
            .expect("additive content must preserve and rebase the same replacement intent");
        assert_eq!(rebased.replacement_intent, basis.replacement_intent);
        drop(lease);

        state.package_library_ready = !basis.ready;
        assert!(
            state
                .rebase_library_replacement_basis(&basis, &library, additive_epoch)
                .is_none(),
            "readiness drift is a construction-key change, not an additive rebase"
        );
        assert_eq!(
            state.library_replacement_lifecycle.lock().pending,
            basis.replacement_intent,
            "a failed old-intent rebase must not clear its still-current owner"
        );
    }

    #[tokio::test]
    async fn replacement_guard_drop_retires_exact_intent_and_requests_reconcile() {
        let mut state = WorldState::new();
        let library = state.package_library.clone();
        let lease = library.routing_lease().await;
        let basis = state
            .capture_library_routing_basis(
                &library,
                library.cache_operation_epoch(&lease),
                LibraryRoutingMutation::Replacement,
                None,
            )
            .unwrap();
        let guard = state
            .guard_library_replacement(&basis, LibraryReplacementAbortPolicy::Reconcile)
            .unwrap();
        drop(lease);

        drop(guard);

        assert!(state.library_replacement_lifecycle.lock().pending.is_none());
        let request = state
            .library_routing_reconcile_request_for_test()
            .expect("exact cancellation must deposit durable reconcile work");
        assert_eq!(
            request.telemetry.package_config_generation,
            basis.package_config_generation
        );
        assert_eq!(request.telemetry.workspace_folders, basis.workspace_folders);
    }

    #[tokio::test]
    async fn old_replacement_guard_drop_does_not_touch_newer_intent() {
        let mut state = WorldState::new();
        let library = state.package_library.clone();
        let lease = library.routing_lease().await;
        let epoch = library.cache_operation_epoch(&lease);
        let old_basis = state
            .capture_library_routing_basis(
                &library,
                epoch,
                LibraryRoutingMutation::Replacement,
                None,
            )
            .unwrap();
        let old_guard = state
            .guard_library_replacement(&old_basis, LibraryReplacementAbortPolicy::Reconcile)
            .unwrap();
        let new_basis = state
            .capture_library_routing_basis(
                &library,
                epoch,
                LibraryRoutingMutation::Replacement,
                None,
            )
            .unwrap();
        let new_guard = state
            .guard_library_replacement(&new_basis, LibraryReplacementAbortPolicy::Reconcile)
            .unwrap();
        drop(lease);

        drop(old_guard);

        assert_eq!(
            state.library_replacement_lifecycle.lock().pending,
            new_basis.replacement_intent
        );
        assert!(state.library_routing_reconcile_request_for_test().is_none());
        state.abort_library_replacement(&new_basis);
        drop(new_guard);
    }

    #[tokio::test]
    async fn refresh_replacement_guard_drop_does_not_request_reconcile() {
        let mut state = WorldState::new();
        let library = state.package_library.clone();
        let lease = library.routing_lease().await;
        let basis = state
            .capture_library_routing_basis(
                &library,
                library.cache_operation_epoch(&lease),
                LibraryRoutingMutation::Replacement,
                None,
            )
            .unwrap();
        let guard = state
            .guard_library_replacement(&basis, LibraryReplacementAbortPolicy::NoReconcile)
            .unwrap();
        drop(lease);

        drop(guard);

        assert!(state.library_replacement_lifecycle.lock().pending.is_none());
        assert!(state.library_routing_reconcile_request_for_test().is_none());
    }

    #[tokio::test]
    async fn replacement_guard_unwind_is_synchronous_and_disarmed_drop_never_locks() {
        let mut state = WorldState::new();
        let library = state.package_library.clone();
        let lease = library.routing_lease().await;
        let basis = state
            .capture_library_routing_basis(
                &library,
                library.cache_operation_epoch(&lease),
                LibraryRoutingMutation::Replacement,
                None,
            )
            .unwrap();
        let guard = state
            .guard_library_replacement(&basis, LibraryReplacementAbortPolicy::Reconcile)
            .unwrap();
        drop(lease);
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = guard;
            panic!("exercise cancellation unwind");
        }));
        assert!(unwind.is_err());
        assert!(state.library_routing_reconcile_request_for_test().is_some());

        let lease = library.routing_lease().await;
        let next_basis = state
            .capture_library_routing_basis(
                &library,
                library.cache_operation_epoch(&lease),
                LibraryRoutingMutation::Replacement,
                None,
            )
            .unwrap();
        let mut disarmed = state
            .guard_library_replacement(&next_basis, LibraryReplacementAbortPolicy::NoReconcile)
            .unwrap();
        disarmed.armed = false;
        let lifecycle = Arc::clone(&state.library_replacement_lifecycle);
        let mut locked = lifecycle.lock();
        drop(disarmed);
        locked.pending = None;
        drop(locked);
        drop(lease);
    }

    #[tokio::test]
    async fn shutdown_fences_guard_and_pre_seal_drop_registration() {
        let mut state = WorldState::new();
        let library = state.package_library.clone();
        let lease = library.routing_lease().await;
        let basis = state
            .capture_library_routing_basis(
                &library,
                library.cache_operation_epoch(&lease),
                LibraryRoutingMutation::Replacement,
                None,
            )
            .unwrap();
        let guard = state
            .guard_library_replacement(&basis, LibraryReplacementAbortPolicy::Reconcile)
            .unwrap();
        state.request_library_routing_reconcile_current();
        let (owner, mut late_deposit) = state.capture_current_library_routing_pre_seal();
        late_deposit
            .fallback
            .push(Url::parse("file:///shutdown-late-ledger.R").unwrap());

        state.retire_all_diagnostic_lifecycles();
        state.cancel_package_seed_retry();
        let _ = state.drain_library_routing_tails_for_shutdown();
        drop(guard);
        owner.deposit(late_deposit);

        let lifecycle = state.library_replacement_lifecycle.lock();
        assert!(lifecycle.pending.is_none());
        assert!(lifecycle.reconcile_required.is_none());
        assert!(lifecycle.pre_seal.is_none());
        drop(lifecycle);
        drop(lease);
    }

    #[test]
    fn routing_tail_survives_commit_handoff_until_state_locked_claim() {
        let mut state = WorldState::new();
        let post_seed = state.record_package_seed_installed();
        let (_owner, mut deposit) = state.capture_current_library_routing_pre_seal();
        deposit.post_seed = Some(LibraryRoutingPreSealPostSeed {
            root: PathBuf::from("/tmp/routing-tail"),
            identity: post_seed,
            deferred_system_file: None,
        });
        deposit.build_notes.push("retained warning".to_string());
        let identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: state.system_file_routing_owner_identity(),
            commit_generation: WorldState::mint_system_file_commit_generation(),
        });
        let handle = state.install_library_routing_transfer(identity, None, Vec::new(), deposit);

        assert!(
            state
                .analysis_transfers
                .get(&handle.identity)
                .is_some_and(|transfer| transfer.routing_tail.is_some()),
            "cancellation after CAS must leave the complete tail on the pending transfer"
        );
        assert_eq!(
            state.claim_library_routing_tail(handle),
            LibraryRoutingTailClaim::PostSeedAdded(LibraryRoutingPreSealPostSeed {
                root: PathBuf::from("/tmp/routing-tail"),
                identity: post_seed,
                deferred_system_file: None,
            })
        );
        assert!(state.analysis_transfer_is_pending_for_test(handle));
        assert!(state.post_seed_refresh_retry_is_current(post_seed));
        assert_eq!(
            state.take_deferred_library_routing_build_notes(),
            vec!["retained warning".to_string()]
        );
    }

    #[test]
    fn shutdown_drain_retires_claimed_and_unclaimed_routing_tails_without_publish() {
        let mut state = WorldState::new();
        let claimed_seed = state.record_package_seed_installed();
        let (_owner, mut claimed_deposit) = state.capture_current_library_routing_pre_seal();
        claimed_deposit.post_seed = Some(LibraryRoutingPreSealPostSeed {
            root: PathBuf::from("/tmp/claimed-tail"),
            identity: claimed_seed,
            deferred_system_file: None,
        });
        let claimed_identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: state.system_file_routing_owner_identity(),
            commit_generation: WorldState::mint_system_file_commit_generation(),
        });
        let claimed = state.install_library_routing_transfer(
            claimed_identity,
            None,
            Vec::new(),
            claimed_deposit,
        );
        assert!(matches!(
            state.claim_library_routing_tail(claimed),
            LibraryRoutingTailClaim::PostSeedAdded(_)
        ));

        let unclaimed_seed = state.record_package_seed_installed();
        let (_owner, mut unclaimed_deposit) = state.capture_current_library_routing_pre_seal();
        unclaimed_deposit.post_seed = Some(LibraryRoutingPreSealPostSeed {
            root: PathBuf::from("/tmp/unclaimed-tail"),
            identity: unclaimed_seed,
            deferred_system_file: None,
        });
        let unclaimed_identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: state.system_file_routing_owner_identity(),
            commit_generation: WorldState::mint_system_file_commit_generation(),
        });
        let unclaimed = state.install_library_routing_transfer(
            unclaimed_identity,
            Some(claimed.identity),
            Vec::new(),
            unclaimed_deposit,
        );
        let (_owner, mut post_drain_deposit) = state.capture_current_library_routing_pre_seal();
        let stranded_identity = AnalysisTransferIdentity::SystemFile(SystemFileTransferIdentity {
            routing_owner: state.system_file_routing_owner_identity(),
            commit_generation: WorldState::mint_system_file_commit_generation(),
        });
        let stranded_handle = state.install_analysis_transfer(stranded_identity, None, Vec::new());
        post_drain_deposit.handles.push(stranded_handle);
        let post_drain_seed = state.record_package_seed_installed();
        let deferred_post_drain_seed = state.record_package_seed_installed();
        let _ = state.begin_post_seed_refresh_retry(post_drain_seed);
        let _ = state.begin_system_file_seed_retry(deferred_post_drain_seed);
        post_drain_deposit.post_seed = Some(LibraryRoutingPreSealPostSeed {
            root: PathBuf::from("/tmp/post-drain-collapse"),
            identity: post_drain_seed,
            deferred_system_file: Some(deferred_post_drain_seed),
        });

        state.retire_all_diagnostic_lifecycles();
        state.cancel_package_seed_retry();
        let _ = state.drain_library_routing_tails_for_shutdown();
        assert!(
            state
                .collapse_current_library_routing_pre_seal(post_drain_deposit)
                .is_none(),
            "a tracked escrow reaching its collapse after shutdown must fail closed"
        );
        assert!(!state.analysis_transfer_is_pending_for_test(stranded_handle));
        assert!(!state.post_seed_refresh_retry_is_current(post_drain_seed));
        assert!(!state.system_file_seed_retry_is_current(deferred_post_drain_seed));

        assert!(!state.post_seed_refresh_retry_is_current(claimed_seed));
        assert!(!state.post_seed_refresh_retry_is_current(unclaimed_seed));
        assert!(!state.analysis_transfer_is_pending_for_test(claimed));
        assert!(!state.analysis_transfer_is_pending_for_test(unclaimed));
        assert!(state.pending_post_seed_outer_handles.is_empty());
        assert!(state.deferred_library_routing_build_notes.is_empty());
        assert_eq!(state.analysis_revalidation_reservation_count, 0);
    }

    #[test]
    fn libpath_primary_attach_failure_owns_one_exact_recovery_then_degrades() {
        let mut state = WorldState::new();
        let basis = state.capture_libpath_watcher_swap_basis().unwrap();
        let commit = state
            .try_commit_libpath_watcher_swap(
                &basis,
                PreparedLibpathWatcherInstall::AttachFailed {
                    recovery: false,
                    can_recover: true,
                },
            )
            .unwrap();
        let owner = commit
            .recovery_owner
            .expect("primary failure mints one exact recovery owner");
        assert!(
            commit.degraded_reconcile_owner.is_none(),
            "AwaitingRecovery is not terminal degradation"
        );
        assert!(matches!(
            state.libpath_watcher,
            LibpathWatcherState::AwaitingRecovery
        ));
        assert_eq!(state.libpath_watcher_owner(), owner);
        assert!(state.library_routing_reconcile_request_for_test().is_none());

        let recovery = state
            .capture_libpath_watcher_recovery_basis(owner)
            .expect("the exact owner may claim one recovery");
        let terminal = state
            .try_commit_libpath_watcher_swap(
                &recovery,
                PreparedLibpathWatcherInstall::AttachFailed {
                    recovery: true,
                    can_recover: false,
                },
            )
            .unwrap();
        assert!(terminal.recovery_owner.is_none());
        assert_eq!(
            terminal.degraded_reconcile_owner,
            Some(owner),
            "terminal watcher-only degradation arms one exact reconcile owner"
        );
        assert!(matches!(
            state.libpath_watcher,
            LibpathWatcherState::Degraded {
                reconcile_pending: true
            }
        ));
        assert!(state.degraded_libpath_reconcile_is_current(owner));
        assert!(
            state
                .capture_libpath_watcher_recovery_basis(owner)
                .is_none()
        );
        assert!(state.library_routing_reconcile_request_for_test().is_none());
    }

    #[test]
    fn libpath_same_coverage_attach_failure_preserves_applied_owner() {
        let mut state = WorldState::new();
        let journal = state.install_libpath_journal_for_test();
        let owner = state.libpath_watcher_owner();
        state.cross_file_config.packages_watch_debounce_ms += 1;
        let basis = state.capture_libpath_watcher_swap_basis().unwrap();
        let commit = state
            .try_commit_libpath_watcher_swap(
                &basis,
                PreparedLibpathWatcherInstall::AttachFailed {
                    recovery: false,
                    can_recover: true,
                },
            )
            .unwrap();
        assert!(commit.recovery_owner.is_none());
        assert!(commit.retired_handle.is_none());
        assert_eq!(state.libpath_watcher_owner(), owner);
        let LibpathWatcherState::ActiveUnapplied {
            applied, desired, ..
        } = &state.libpath_watcher
        else {
            panic!("same path coverage remains actively watched")
        };
        assert_ne!(applied.debounce_ms, desired.debounce_ms);
        assert!(!journal.is_closed_for_test());
    }

    #[test]
    fn libpath_changed_coverage_failure_closes_old_journal_and_awaits_recovery() {
        let mut state = WorldState::new();
        let journal = state.install_libpath_journal_for_test();
        let mut library = PackageLibrary::new_empty();
        library.set_lib_paths(vec![PathBuf::from("/tmp/raven-new-libpath")]);
        state.package_library = Arc::new(library);
        let basis = state.capture_libpath_watcher_swap_basis().unwrap();
        let commit = state
            .try_commit_libpath_watcher_swap(
                &basis,
                PreparedLibpathWatcherInstall::AttachFailed {
                    recovery: false,
                    can_recover: true,
                },
            )
            .unwrap();
        assert!(commit.recovery_owner.is_some());
        assert!(journal.is_closed_for_test());
        assert!(matches!(
            state.libpath_watcher,
            LibpathWatcherState::AwaitingRecovery
        ));
    }

    #[tokio::test]
    async fn stale_prospective_watcher_loser_closes_its_buffering_journal() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.packages_enabled = true;
        state.cross_file_config.packages_watch_library_paths = true;
        state.package_library_ready = true;
        let mut library = PackageLibrary::new_empty();
        library.set_lib_paths(vec![temp.path().to_path_buf()]);
        state.package_library = Arc::new(library);

        let basis = state.capture_libpath_watcher_swap_basis().unwrap();
        let journal = crate::libpath_watcher::LibpathWatchJournal::new_buffering();
        journal.require_rescan();
        let handle = crate::libpath_watcher::prearm_watcher(
            vec![temp.path().to_path_buf()],
            std::time::Duration::from_millis(basis.debounce_ms()),
            Arc::clone(&journal),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("temporary directory is fully watchable");

        // Invalidate the exact settings basis after prearm. The rejected CAS
        // consumes and drops the unpublished handle; its Drop closes the
        // buffering journal so the prospective consumer cannot survive.
        state.cross_file_config.packages_watch_debounce_ms += 1;
        assert!(
            state
                .try_commit_libpath_watcher_swap(
                    &basis,
                    PreparedLibpathWatcherInstall::Active {
                        handle: Arc::new(handle),
                        journal: Arc::clone(&journal),
                        recovery: false,
                    },
                )
                .is_err()
        );
        assert!(journal.is_closed_for_test());
        assert!(matches!(
            state.libpath_watcher,
            LibpathWatcherState::Disabled
        ));
    }

    #[tokio::test]
    async fn active_to_active_watcher_swap_activates_seed_and_retires_old_owner() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.packages_enabled = true;
        state.cross_file_config.packages_watch_library_paths = true;
        state.package_library_ready = true;
        let mut library = PackageLibrary::new_empty();
        library.set_lib_paths(vec![temp.path().to_path_buf()]);
        state.package_library = Arc::new(library);

        let first_basis = state.capture_libpath_watcher_swap_basis().unwrap();
        let first_journal = crate::libpath_watcher::LibpathWatchJournal::new_buffering();
        first_journal.require_rescan();
        let first_handle = crate::libpath_watcher::prearm_watcher(
            vec![temp.path().to_path_buf()],
            std::time::Duration::from_millis(first_basis.debounce_ms()),
            Arc::clone(&first_journal),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let first = state
            .try_commit_libpath_watcher_swap(
                &first_basis,
                PreparedLibpathWatcherInstall::Active {
                    handle: Arc::new(first_handle),
                    journal: Arc::clone(&first_journal),
                    recovery: false,
                },
            )
            .unwrap();
        assert!(first.retired_handle.is_none());
        let first_owner = state.libpath_watcher_owner();
        let mut first_seed = first_journal.claim().await.unwrap();
        assert!(matches!(
            first_seed.event(),
            crate::libpath_watcher::LibpathEvent::Rescan
        ));
        first_seed.ack();

        state.cross_file_config.packages_watch_debounce_ms += 1;
        let second_basis = state.capture_libpath_watcher_swap_basis().unwrap();
        let second_journal = crate::libpath_watcher::LibpathWatchJournal::new_buffering();
        second_journal.require_rescan();
        let second_handle = crate::libpath_watcher::prearm_watcher(
            vec![temp.path().to_path_buf()],
            std::time::Duration::from_millis(second_basis.debounce_ms()),
            Arc::clone(&second_journal),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let second = state
            .try_commit_libpath_watcher_swap(
                &second_basis,
                PreparedLibpathWatcherInstall::Active {
                    handle: Arc::new(second_handle),
                    journal: Arc::clone(&second_journal),
                    recovery: false,
                },
            )
            .unwrap();
        assert!(second.retired_handle.is_some());
        assert_ne!(state.libpath_watcher_owner(), first_owner);
        assert!(first_journal.is_closed_for_test());
        let mut second_seed = second_journal.claim().await.unwrap();
        assert!(matches!(
            second_seed.event(),
            crate::libpath_watcher::LibpathEvent::Rescan
        ));
        second_seed.ack();
        drop(second.retired_handle);
    }

    #[tokio::test]
    async fn concurrent_prospective_watchers_leave_one_active_winner_and_close_all_losers() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.packages_enabled = true;
        state.cross_file_config.packages_watch_library_paths = true;
        state.package_library_ready = true;
        let mut library = PackageLibrary::new_empty();
        library.set_lib_paths(vec![temp.path().to_path_buf()]);
        state.package_library = Arc::new(library);

        let mut prospective = Vec::new();
        for _ in 0..4 {
            let basis = state.capture_libpath_watcher_swap_basis().unwrap();
            let journal = crate::libpath_watcher::LibpathWatchJournal::new_buffering();
            journal.require_rescan();
            let handle = crate::libpath_watcher::prearm_watcher(
                vec![temp.path().to_path_buf()],
                std::time::Duration::from_millis(basis.debounce_ms()),
                Arc::clone(&journal),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            prospective.push((basis, journal, handle));
        }

        let (winner_basis, winner_journal, winner_handle) = prospective.remove(0);
        state
            .try_commit_libpath_watcher_swap(
                &winner_basis,
                PreparedLibpathWatcherInstall::Active {
                    handle: Arc::new(winner_handle),
                    journal: Arc::clone(&winner_journal),
                    recovery: false,
                },
            )
            .unwrap();

        for (basis, journal, handle) in prospective {
            let outcome = state.try_commit_libpath_watcher_swap(
                &basis,
                PreparedLibpathWatcherInstall::Active {
                    handle: Arc::new(handle),
                    journal: Arc::clone(&journal),
                    recovery: false,
                },
            );
            let rejected = match outcome {
                Err(rejected) => rejected,
                Ok(_) => panic!("the first exact owner supersedes every sibling basis"),
            };
            drop(rejected);
            assert!(journal.is_closed_for_test());
            assert!(journal.claim().await.is_none());
        }

        let active = state
            .libpath_watcher
            .active_journal()
            .expect("one exact winner remains active");
        assert!(Arc::ptr_eq(active, &winner_journal));
        assert!(!winner_journal.is_closed_for_test());
        let mut seed = winner_journal.claim().await.unwrap();
        assert!(matches!(
            seed.event(),
            crate::libpath_watcher::LibpathEvent::Rescan
        ));
        seed.ack();
    }

    #[test]
    fn tar_watch_event_fence_distinguishes_overlap_from_root_generation() {
        let parent = Url::parse("file:///workspace/_targets.R").unwrap();
        let root = PathBuf::from("/workspace/R");
        let mut state = WorldState::new();
        state
            .tar_source_parents_by_watch_path
            .insert(root.clone(), HashSet::from([parent.clone()]));
        state
            .tar_source_watch_paths_by_parent
            .insert(parent.clone(), vec![root.clone()]);
        state.bump_tar_source_watch_path_generation(&root);
        let root_generation = state.tar_source_watch_path_generations[&root];
        let global_generation = state.tar_source_event_generation;

        let unrelated = state
            .record_tar_source_filesystem_events([
                Url::parse("file:///workspace/other/file.R").unwrap()
            ]);
        assert!(unrelated.is_empty());
        assert!(state.tar_source_event_generation > global_generation);
        assert_eq!(
            state.tar_source_watch_path_generations[&root], root_generation,
            "an unrelated event advances the global pre-registry fence but not the root identity"
        );

        let overlapping = state
            .record_tar_source_filesystem_events(
                [Url::parse("file:///workspace/R/new.R").unwrap()],
            );
        assert_eq!(overlapping, [parent]);
        assert!(state.tar_source_watch_path_generations[&root] > root_generation);
    }

    #[test]
    fn plain_open_edit_checks_one_parent_without_full_registry_sweep() {
        let mut state = WorldState::new();
        for index in 0..256 {
            let uri = Url::parse(&format!("file:///workspace/closed-{index}.R")).unwrap();
            state.insert_workspace_document_for_test(
                uri.clone(),
                Document::new_with_uri("closed <- 1\n", None, &uri),
            );
        }
        let uri = Url::parse("file:///workspace/open.R").unwrap();
        state.open_document(uri.clone(), "before <- 1\n", Some(1));
        let rebuilds_before = state.tar_source_watch_full_rebuild_count;
        let checks_before = state.tar_source_watch_parent_check_count;

        commit_test_edit(
            &mut state,
            &uri,
            "after <- 2\n",
            crate::cross_file::CrossFileMetadata::default(),
            PreparedOpenCommitPlan::default(),
        )
        .unwrap();

        assert_eq!(
            state.tar_source_watch_full_rebuild_count, rebuilds_before,
            "ordinary typing must not scan the workspace-wide tar watch registry"
        );
        assert_eq!(
            state.tar_source_watch_parent_check_count,
            checks_before + 1,
            "the OpenEdit gate must inspect only its subject parent"
        );
        assert_tar_watch_registry_matches_full_oracle(&state);
    }

    #[test]
    fn gated_tar_watch_refresh_matches_full_oracle_across_root_transitions() {
        let parent = Url::parse("file:///workspace/_targets.R").unwrap();
        let old_root = PathBuf::from("/workspace/R");
        let new_root = PathBuf::from("/workspace/src");
        let mut state = WorldState::new();
        state.open_document(parent.clone(), "targets::tar_source(\"R\")\n", Some(1));

        let generation = state.documents.get_record(&parent).unwrap().generation();
        state
            .documents
            .replace_metadata_if_current(
                &parent,
                generation,
                Arc::new(crate::cross_file::CrossFileMetadata {
                    tar_source_expansion_watch_paths: vec![old_root.clone()],
                    ..Default::default()
                }),
            )
            .unwrap();
        let rebuilds_before = state.tar_source_watch_full_rebuild_count;
        state.refresh_tar_source_watch_registry(TarSourceWatchRegistryRefresh::Parents(vec![
            parent.clone(),
        ]));
        assert_eq!(
            state.tar_source_watch_full_rebuild_count,
            rebuilds_before + 1
        );
        assert_tar_watch_registry_matches_full_oracle(&state);
        let old_generation = state.tar_source_watch_path_generations[&old_root];

        let rebuilds_before = state.tar_source_watch_full_rebuild_count;
        state.refresh_tar_source_watch_registry(TarSourceWatchRegistryRefresh::Parents(vec![
            parent.clone(),
        ]));
        assert_eq!(
            state.tar_source_watch_full_rebuild_count, rebuilds_before,
            "an identical root set must stay on the bounded gate"
        );
        assert_eq!(
            state.tar_source_watch_path_generations[&old_root], old_generation,
            "an identical owner set must not bump its generation"
        );

        let generation = state.documents.get_record(&parent).unwrap().generation();
        state
            .documents
            .replace_metadata_if_current(
                &parent,
                generation,
                Arc::new(crate::cross_file::CrossFileMetadata {
                    tar_source_expansion_watch_paths: vec![new_root.clone()],
                    ..Default::default()
                }),
            )
            .unwrap();
        state.refresh_tar_source_watch_registry(TarSourceWatchRegistryRefresh::Parents(vec![
            parent.clone(),
        ]));
        assert_tar_watch_registry_matches_full_oracle(&state);
        assert!(
            !state
                .tar_source_parents_by_watch_path
                .contains_key(&old_root)
        );
        assert!(state.tar_source_watch_path_generations[&old_root] > old_generation);
        let retired_generation = state.tar_source_watch_path_generations[&old_root];
        let new_generation = state.tar_source_watch_path_generations[&new_root];

        state.close_document(&parent);
        assert_tar_watch_registry_matches_full_oracle(&state);
        assert!(state.tar_source_watch_paths_by_parent.is_empty());
        assert!(state.tar_source_parents_by_watch_path.is_empty());
        assert_eq!(
            state.tar_source_watch_path_generations[&old_root], retired_generation,
            "an unrelated later transition must preserve the old root tombstone"
        );
        assert!(state.tar_source_watch_path_generations[&new_root] > new_generation);
    }

    #[test]
    fn targeted_eviction_retires_tar_watch_root_and_rejected_swap_is_inert() {
        use crate::cross_file::file_cache::FileSnapshot;
        use crate::workspace_index::{
            ClosedProvenance, IndexEntry, WorkspaceIndexConfig, WorkspaceIndexTargetedChanges,
        };

        let victim = Url::parse("file:///workspace/_targets.R").unwrap();
        let replacement = Url::parse("file:///workspace/replacement.R").unwrap();
        let stale_replacement = Url::parse("file:///workspace/stale-replacement.R").unwrap();
        let root = PathBuf::from("/workspace/R");
        let make_entry = |metadata| IndexEntry {
            contents: Rope::from_str("x <- 1\n"),
            tree: None,
            loaded_packages: Vec::new(),
            data_packages: Vec::new(),
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: None,
            },
            metadata: Arc::new(metadata),
            artifacts: Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 0,
        };

        let mut state = WorldState::new();
        state.workspace_index = WorkspaceIndex::new(WorkspaceIndexConfig {
            max_files: 1,
            ..Default::default()
        });
        state.workspace_index.resize_artifacts(1);
        state.workspace_index.install_complete(
            victim.clone(),
            make_entry(crate::cross_file::CrossFileMetadata {
                tar_source_expansion_watch_paths: vec![root.clone()],
                ..Default::default()
            }),
            ClosedProvenance::Dynamic,
        );
        state.rebuild_tar_source_watch_registry();
        let root_generation = state.tar_source_watch_path_generations[&root];
        let rebuilds_before = state.tar_source_watch_full_rebuild_count;

        let prepared = state
            .workspace_index
            .prepare_targeted_batch_if_current(
                state.workspace_index.version(),
                WorkspaceIndexTargetedChanges {
                    metadata: Vec::new(),
                    installs: vec![(
                        replacement.clone(),
                        make_entry(crate::cross_file::CrossFileMetadata::default()),
                        ClosedProvenance::Dynamic,
                    )],
                    removals: Vec::new(),
                    pins: HashSet::new(),
                },
            )
            .unwrap()
            .unwrap();
        assert!(
            prepared.changed_uris().contains(&victim),
            "the prepared swap must carry its implicit Complete eviction victim"
        );
        let refresh = TarSourceWatchRegistryRefresh::Parents(prepared.changed_uris().to_vec());
        assert!(
            state
                .workspace_index
                .commit_prepared_targeted_batch(prepared)
                .unwrap()
        );
        state.refresh_tar_source_watch_registry(refresh);

        assert_eq!(
            state.tar_source_watch_full_rebuild_count,
            rebuilds_before + 1,
            "one successful ownership change needs exactly one full rebuild"
        );
        assert_eq!(
            state.tar_source_watch_path_generations[&root],
            root_generation.wrapping_add(1),
            "retiring the last owner bumps the root tombstone exactly once"
        );
        assert_tar_watch_registry_matches_full_oracle(&state);

        let prepared = state
            .workspace_index
            .prepare_targeted_batch_if_current(
                state.workspace_index.version(),
                WorkspaceIndexTargetedChanges {
                    metadata: Vec::new(),
                    installs: vec![(
                        stale_replacement,
                        make_entry(crate::cross_file::CrossFileMetadata::default()),
                        ClosedProvenance::Dynamic,
                    )],
                    removals: Vec::new(),
                    pins: HashSet::new(),
                },
            )
            .unwrap()
            .unwrap();
        let owners_before = state.tar_source_watch_paths_by_parent.clone();
        let reverse_before = state.tar_source_parents_by_watch_path.clone();
        let generations_before = state.tar_source_watch_path_generations.clone();
        let rebuilds_before = state.tar_source_watch_full_rebuild_count;
        assert!(state.workspace_index.replace_complete_metadata(
            &replacement,
            Arc::new(crate::cross_file::CrossFileMetadata::default())
        ));
        assert!(
            !state
                .workspace_index
                .commit_prepared_targeted_batch(prepared)
                .unwrap()
        );
        assert_eq!(state.tar_source_watch_paths_by_parent, owners_before);
        assert_eq!(state.tar_source_parents_by_watch_path, reverse_before);
        assert_eq!(state.tar_source_watch_path_generations, generations_before);
        assert_eq!(
            state.tar_source_watch_full_rebuild_count, rebuilds_before,
            "a rejected targeted CAS must not reach the registry gate"
        );
    }

    #[test]
    fn closed_upsert_reconciles_implicit_tar_watch_eviction() {
        use crate::workspace_index::{ClosedProvenance, WorkspaceIndexConfig};

        let victim = Url::parse("file:///workspace/_targets.R").unwrap();
        let subject = Url::parse("file:///workspace/new.R").unwrap();
        let root = PathBuf::from("/workspace/R");
        let mut state = WorldState::new();
        state.workspace_index = WorkspaceIndex::new(WorkspaceIndexConfig {
            max_files: 1,
            ..Default::default()
        });
        state.workspace_index.resize_artifacts(1);
        state.workspace_index.install_complete(
            victim.clone(),
            tar_watch_test_entry(crate::cross_file::CrossFileMetadata {
                tar_source_expansion_watch_paths: vec![root.clone()],
                ..Default::default()
            }),
            ClosedProvenance::Dynamic,
        );
        state.rebuild_tar_source_watch_registry();
        let root_generation = state.tar_source_watch_path_generations[&root];
        let rebuilds_before = state.tar_source_watch_full_rebuild_count;

        let basis = state.capture_closed_removal_analysis_basis(&subject);
        let entry = tar_watch_test_entry(crate::cross_file::CrossFileMetadata::default());
        let prepared = PreparedClosedAnalysis::new(
            basis,
            subject.clone(),
            entry.clone(),
            entry.snapshot.clone(),
            entry.contents.to_string(),
            entry.metadata.clone(),
            None,
            HashMap::new(),
            Vec::new(),
            Vec::new(),
        );
        state
            .try_commit_analysis(PreparedAnalysisCommit::Upsert(Box::new(prepared)))
            .unwrap();

        assert!(!state.workspace_index.is_complete(&victim));
        assert!(state.workspace_index.is_complete(&subject));
        assert_eq!(
            state.tar_source_watch_full_rebuild_count,
            rebuilds_before + 1
        );
        assert_eq!(
            state.tar_source_watch_path_generations[&root],
            root_generation.wrapping_add(1)
        );
        assert_tar_watch_registry_matches_full_oracle(&state);
    }

    #[test]
    fn pending_claim_reconciles_eviction_before_enrichment_finishes() {
        use crate::workspace_index::{ClaimEnrichment, ClosedProvenance, WorkspaceIndexConfig};

        let victim = Url::parse("file:///workspace/_targets.R").unwrap();
        let pending = Url::parse("file:///workspace/pending.R").unwrap();
        let root = PathBuf::from("/workspace/R");
        let mut state = WorldState::new();
        state.workspace_index = WorkspaceIndex::new(WorkspaceIndexConfig {
            max_files: 1,
            ..Default::default()
        });
        state.workspace_index.resize_artifacts(1);
        state.workspace_index.install_complete(
            victim.clone(),
            tar_watch_test_entry(crate::cross_file::CrossFileMetadata {
                tar_source_expansion_watch_paths: vec![root.clone()],
                ..Default::default()
            }),
            ClosedProvenance::Dynamic,
        );
        state.rebuild_tar_source_watch_registry();
        let root_generation = state.tar_source_watch_path_generations[&root];

        let (claim, evicted) = state
            .workspace_index
            .claim_enrichment_with_eviction(pending.clone(), ClosedProvenance::Dynamic);
        assert_eq!(evicted.as_ref(), Some(&victim));
        state.refresh_tar_source_watch_parents(std::iter::once(pending).chain(evicted));

        assert_eq!(
            state.tar_source_watch_path_generations[&root],
            root_generation.wrapping_add(1),
            "claim admission retires the evicted owner before detached enrichment"
        );
        assert_tar_watch_registry_matches_full_oracle(&state);
        let ClaimEnrichment::Claimed(claim) = claim else {
            panic!("expected a fresh Pending claim");
        };
        state.workspace_index.abort_enrichment(&claim).unwrap();
        assert_tar_watch_registry_matches_full_oracle(&state);
    }

    #[test]
    fn live_cache_shrink_reconciles_evicted_tar_watch_owner() {
        use crate::workspace_index::ClosedProvenance;

        let victim = Url::parse("file:///workspace/_targets.R").unwrap();
        let survivor = Url::parse("file:///workspace/survivor.R").unwrap();
        let root = PathBuf::from("/workspace/R");
        let mut state = WorldState::new();
        state.workspace_index.resize_artifacts(2);
        state.workspace_index.install_complete(
            victim.clone(),
            tar_watch_test_entry(crate::cross_file::CrossFileMetadata {
                tar_source_expansion_watch_paths: vec![root.clone()],
                ..Default::default()
            }),
            ClosedProvenance::Dynamic,
        );
        state.workspace_index.install_complete(
            survivor.clone(),
            tar_watch_test_entry(crate::cross_file::CrossFileMetadata::default()),
            ClosedProvenance::Dynamic,
        );
        state.rebuild_tar_source_watch_registry();
        let root_generation = state.tar_source_watch_path_generations[&root];
        let rebuilds_before = state.tar_source_watch_full_rebuild_count;

        let config = crate::cross_file::config::CrossFileConfig {
            cache_workspace_index_max_entries: 1,
            ..Default::default()
        };
        state.resize_caches(&config);

        assert!(!state.workspace_index.is_complete(&victim));
        assert!(state.workspace_index.is_complete(&survivor));
        assert_eq!(
            state.tar_source_watch_full_rebuild_count,
            rebuilds_before + 1
        );
        assert_eq!(
            state.tar_source_watch_path_generations[&root],
            root_generation.wrapping_add(1)
        );
        assert_tar_watch_registry_matches_full_oracle(&state);
    }

    #[test]
    fn tar_watch_root_replacement_bumps_tombstone_and_stales_basis() {
        let parent = Url::parse("file:///workspace/_targets.R").unwrap();
        let old_root = PathBuf::from("/workspace/R");
        let new_root = PathBuf::from("/workspace/src");
        let mut state = WorldState::new();
        state.open_document(parent.clone(), "targets::tar_source(\"R\")\n", Some(1));
        let generation = state.documents.get_record(&parent).unwrap().generation();
        let mut metadata = crate::cross_file::CrossFileMetadata {
            tar_source_expansion_watch_paths: vec![old_root.clone()],
            ..Default::default()
        };
        state
            .documents
            .replace_metadata_if_current(&parent, generation, Arc::new(metadata.clone()))
            .unwrap();
        state.rebuild_tar_source_watch_registry();
        let basis = state.capture_open_analysis_basis(&parent).unwrap();
        let old_generation = state.tar_source_watch_path_generations[&old_root];

        let generation = state.documents.get_record(&parent).unwrap().generation();
        metadata.tar_source_expansion_watch_paths = vec![new_root.clone()];
        state
            .documents
            .replace_metadata_if_current(&parent, generation, Arc::new(metadata))
            .unwrap();
        state.rebuild_tar_source_watch_registry();

        assert!(
            !state
                .tar_source_parents_by_watch_path
                .contains_key(&old_root)
        );
        assert_eq!(
            state.tar_source_parents_by_watch_path[&new_root],
            HashSet::from([parent.clone()])
        );
        assert!(state.tar_source_watch_path_generations[&old_root] > old_generation);
        assert!(
            !state.analysis_basis_is_current(&basis, &parent, &HashSet::new()),
            "a basis captured against the retired root must not commit"
        );
    }

    #[test]
    fn tar_watch_event_after_walk_rejects_store_and_registry_commit() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("R")).unwrap();
        std::fs::write(temp.path().join("R/old.R"), "old_value <- 1\n").unwrap();
        let parent_path = temp.path().join("_targets.R");
        let parent_text = "targets::tar_source(\"R\")\n";
        std::fs::write(&parent_path, parent_text).unwrap();
        let root_uri = Url::from_directory_path(temp.path()).unwrap();
        let parent_uri = Url::from_file_path(&parent_path).unwrap();
        let old_uri = Url::from_file_path(temp.path().join("R/old.R")).unwrap();
        let new_uri = Url::from_file_path(temp.path().join("R/new.R")).unwrap();

        let mut state = WorldState::new();
        state.workspace_folders = vec![root_uri.clone()];
        state.open_document(parent_uri.clone(), parent_text, Some(1));
        let mut initial_metadata = crate::cross_file::extract_metadata(parent_text);
        crate::cross_file::tar_source::finalize_tar_source_requests(
            &mut initial_metadata,
            &parent_uri,
            Some(&root_uri),
        );
        let initial_generation = state
            .documents
            .get_record(&parent_uri)
            .unwrap()
            .generation();
        state
            .documents
            .replace_metadata_if_current(
                &parent_uri,
                initial_generation,
                Arc::new(initial_metadata.clone()),
            )
            .unwrap();
        state
            .cross_file_graph
            .update_file(&parent_uri, &initial_metadata, Some(&root_uri), |_| None);
        state.rebuild_tar_source_watch_registry();

        let expected_generation = state
            .documents
            .get_record(&parent_uri)
            .unwrap()
            .generation();
        let captured = state
            .capture_open_metadata_derivation(&parent_uri, expected_generation)
            .unwrap();
        let owners_before = state.tar_source_watch_paths_by_parent.clone();
        let reverse_before = state.tar_source_parents_by_watch_path.clone();
        let rebuilds_before = state.tar_source_watch_full_rebuild_count;

        // The detached derivation performs its filesystem walk and observes
        // the new member before attempting to commit.
        std::fs::write(temp.path().join("R/new.R"), "new_value <- 2\n").unwrap();
        let mut walked_metadata = crate::cross_file::extract_metadata(parent_text);
        crate::cross_file::tar_source::finalize_tar_source_requests(
            &mut walked_metadata,
            &parent_uri,
            Some(&root_uri),
        );
        assert!(
            walked_metadata
                .sources
                .iter()
                .any(|source| { source.resolved_uri.as_ref() == Some(&new_uri) })
        );

        // A watcher event lands after the walk but before the prepared
        // metadata/store/registry transaction reaches its CAS.
        assert_eq!(
            state.record_tar_source_filesystem_events([new_uri.clone()]),
            std::slice::from_ref(&parent_uri)
        );
        let prepared = state
            .prepare_captured_open_metadata_analysis(
                captured,
                Arc::new(walked_metadata),
                PreparedOpenCommitPlan::default(),
                Vec::new(),
            )
            .unwrap();
        assert!(matches!(
            state.try_commit_analysis(PreparedAnalysisCommit::OpenMetadata(Box::new(prepared))),
            Err(AnalysisCommitRejected::StaleBasis)
        ));

        let record = state.documents.get_record(&parent_uri).unwrap();
        assert_eq!(record.generation(), expected_generation);
        assert!(
            record
                .metadata()
                .sources
                .iter()
                .any(|source| { source.resolved_uri.as_ref() == Some(&old_uri) })
        );
        assert!(
            !record
                .metadata()
                .sources
                .iter()
                .any(|source| { source.resolved_uri.as_ref() == Some(&new_uri) })
        );
        assert_eq!(state.tar_source_watch_paths_by_parent, owners_before);
        assert_eq!(state.tar_source_parents_by_watch_path, reverse_before);
        assert_eq!(
            state.tar_source_watch_full_rebuild_count, rebuilds_before,
            "a rejected CAS must not reach the post-commit registry gate"
        );
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&parent_uri)
                .iter()
                .all(|edge| edge.to != new_uri),
            "a rejected walk must not mutate the graph"
        );
    }

    #[test]
    fn workspace_scan_finalizes_tar_metadata_artifacts_and_graph() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("R")).unwrap();
        std::fs::write(
            temp.path().join("_targets.R"),
            "targets::tar_source(\"R\")\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("R/member.R"), "member <- 1\n").unwrap();
        let root = Url::from_directory_path(temp.path()).unwrap();
        let parent = Url::from_file_path(temp.path().join("_targets.R")).unwrap();
        let member = Url::from_file_path(temp.path().join("R/member.R")).unwrap();

        let entries = scan_workspace(std::slice::from_ref(&root), 10);
        let entry = entries
            .get(&parent)
            .expect("targets parent must be scanned");
        assert!(entry.metadata.sources.iter().any(|source| {
            source.tar_source_ordinal == Some(0) && source.resolved_uri.as_ref() == Some(&member)
        }));
        assert!(entry.artifacts.timeline.iter().any(|event| matches!(
            event,
            crate::cross_file::scope::ScopeEvent::SourceBatch { members, .. }
                if members.len() == 1
        )));

        let mut state = WorldState::new();
        state.workspace_folders = vec![root];
        state.apply_workspace_index(entries);
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&parent)
                .iter()
                .any(|edge| edge.to == member && edge.tar_source_ordinal == Some(0))
        );
    }

    #[test]
    fn workspace_scan_uses_stan_tree_and_keeps_r_analysis_inert() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("model.StAn");
        std::fs::write(
            &path,
            "# raven: source ignored-by-r-analysis.R\ndata { int N; }\nmodel {}\n",
        )
        .unwrap();
        let root = Url::from_directory_path(temp.path()).unwrap();
        let uri = Url::from_file_path(&path).unwrap();
        let entries = scan_workspace(&[root], 10);
        let entry = entries.get(&uri).expect("mixed-case Stan file is indexed");
        assert_eq!(entry.tree.as_ref().unwrap().root_node().kind(), "program");
        assert!(!entry.tree.as_ref().unwrap().root_node().has_error());
        assert!(entry.metadata.sources.is_empty());
        assert!(entry.metadata.sourced_by.is_empty());
        assert!(entry.artifacts.exported_interface.is_empty());
        assert!(entry.loaded_packages.is_empty());
        assert!(entry.data_packages.is_empty());
    }

    #[test]
    fn indexed_stan_document_reconstruction_preserves_masked_tree_text_pair() {
        let uri = Url::parse("file:///workspace/model.stan").unwrap();
        let raw = "\u{feff}# raven: source helper.R\r\ndata { int N }\r\nmodel {}\r\n";
        let document = Document::new_with_uri(raw, None, &uri);
        let metadata = Arc::new(document.cross_file_metadata());
        let artifacts = Arc::new(document.cross_file_artifacts(&uri, &metadata));
        let entry = crate::workspace_index::IndexEntry {
            contents: document.contents.clone(),
            tree: document.tree.clone(),
            loaded_packages: document.loaded_packages.clone(),
            data_packages: document.data_packages.clone(),
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: raw.len() as u64,
                content_hash: Some(1),
            },
            metadata,
            artifacts,
            indexed_at_version: 0,
        };

        let reconstructed = document_from_workspace_entry(&uri, &entry, ChunkKind::R);
        assert_eq!(reconstructed.analysis_text(), document.analysis_text());
        assert_eq!(
            reconstructed.tree.as_ref().unwrap().root_node().to_sexp(),
            document.tree.as_ref().unwrap().root_node().to_sexp()
        );

        let mut open_state = WorldState::new();
        open_state.workspace_scan_complete = true;
        open_state.cross_file_config.stan_diagnostics_enabled = true;
        open_state.documents.insert(uri.clone(), document);
        let open_findings = crate::handlers::diagnostics(
            &open_state,
            &uri,
            &crate::handlers::DiagCancelToken::never(),
        );
        let mut closed_state = WorldState::new();
        closed_state.workspace_scan_complete = true;
        closed_state.cross_file_config.stan_diagnostics_enabled = true;
        closed_state.workspace_index.insert(uri.clone(), entry);
        closed_state.documents.insert(uri.clone(), reconstructed);
        let closed_findings = crate::handlers::diagnostics(
            &closed_state,
            &uri,
            &crate::handlers::DiagCancelToken::never(),
        );
        assert_eq!(closed_findings, open_findings);
        assert!(!closed_findings.is_empty());
    }

    #[test]
    fn synchronous_dynamic_materialization_keeps_stan_analysis_inert() {
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};
        use crate::workspace_index::IndexEntry;

        let temp = tempfile::tempdir().unwrap();
        let model_path = temp.path().join("dynamic.stan");
        std::fs::write(&model_path, "data { int N }\nmodel {}\n").unwrap();
        let model_uri = Url::from_file_path(&model_path).unwrap();
        let source_uri = Url::parse("file:///workspace/source.R").unwrap();
        let source_entry = IndexEntry {
            contents: Rope::from_str("source(system.file('dynamic.stan', package='p'))\n"),
            tree: None,
            loaded_packages: Vec::new(),
            data_packages: Vec::new(),
            snapshot: crate::cross_file::file_cache::FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: Some(1),
            },
            metadata: Arc::new(CrossFileMetadata {
                sources: vec![ForwardSource {
                    resolved_uri: Some(model_uri.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            artifacts: Arc::new(crate::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 0,
        };
        let mut state = WorldState::new();
        state.workspace_scan_complete = true;
        state.workspace_index.insert(source_uri, source_entry);
        state.index_cross_package_resolved_files();

        let document = {
            let entry = state.workspace_index.get(&model_uri).unwrap();
            assert_eq!(entry.tree.as_ref().unwrap().root_node().kind(), "program");
            assert!(entry.metadata.sources.is_empty());
            assert!(entry.metadata.working_directory.is_none());
            assert!(entry.artifacts.exported_interface.is_empty());
            assert!(entry.loaded_packages.is_empty());
            assert!(entry.data_packages.is_empty());
            document_from_workspace_entry(&model_uri, &entry, ChunkKind::R)
        };
        let mut diagnostic_state = WorldState::new();
        diagnostic_state.workspace_scan_complete = true;
        diagnostic_state.cross_file_config.stan_diagnostics_enabled = true;
        diagnostic_state
            .documents
            .insert(model_uri.clone(), document);
        assert!(
            !crate::handlers::diagnostics(
                &diagnostic_state,
                &model_uri,
                &crate::handlers::DiagCancelToken::never()
            )
            .is_empty()
        );
    }

    #[tokio::test]
    async fn prepared_dynamic_materialization_keeps_stan_analysis_inert() {
        let library = tempfile::tempdir().unwrap();
        let package = library.path().join("otherpkg");
        std::fs::create_dir_all(&package).unwrap();
        let model_path = package.join("dynamic.stan");
        std::fs::write(&model_path, "data { int N }\nmodel {}\n").unwrap();
        let model_uri = Url::from_file_path(&model_path).unwrap();
        let source = Url::parse("file:///workspace/source.R").unwrap();
        let text = "source(system.file('dynamic.stan', package = 'otherpkg'))\n";
        let mut state = WorldState::new();
        state.workspace_scan_complete = true;
        state.open_document(source.clone(), text, Some(1));
        let generation = state.documents.get_record(&source).unwrap().generation();
        state
            .replace_open_document_metadata_if_current(
                &source,
                generation,
                Arc::new(crate::cross_file::extract_metadata(text)),
            )
            .unwrap();
        let mut package_library = crate::package_library::PackageLibrary::new_empty();
        package_library.set_lib_paths(vec![library.path().to_path_buf()]);
        state.install_package_library(Arc::new(package_library), true);

        crate::backend::run_system_file_convergence_for_test(&mut state, None)
            .await
            .expect("Stan dynamic target materialization commits");
        let document = {
            let entry = state.workspace_index.get(&model_uri).unwrap();
            assert_eq!(entry.tree.as_ref().unwrap().root_node().kind(), "program");
            assert!(entry.metadata.sources.is_empty());
            assert!(entry.metadata.working_directory.is_none());
            assert!(entry.artifacts.exported_interface.is_empty());
            assert!(entry.loaded_packages.is_empty());
            assert!(entry.data_packages.is_empty());
            document_from_workspace_entry(&model_uri, &entry, ChunkKind::R)
        };
        let mut diagnostic_state = WorldState::new();
        diagnostic_state.workspace_scan_complete = true;
        diagnostic_state.cross_file_config.stan_diagnostics_enabled = true;
        diagnostic_state
            .documents
            .insert(model_uri.clone(), document);
        assert!(
            !crate::handlers::diagnostics(
                &diagnostic_state,
                &model_uri,
                &crate::handlers::DiagCancelToken::never()
            )
            .is_empty()
        );
    }
}
