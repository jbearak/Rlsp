//
// state.rs
//
// Copyright (C) 2022-2026 Posit Software, PBC. All rights reserved.
// Modifications copyright (C) 2026 Jonathan Marc Bearak
//

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ropey::Rope;
use tower_lsp::lsp_types::TextDocumentContentChangeEvent;
use tower_lsp::lsp_types::Url;
use tree_sitter::Parser;
use tree_sitter::Tree;

use crate::content_provider::DefaultContentProvider;
use crate::cross_file::revalidation::CrossFileDiagnosticsGate;
use crate::cross_file::{
    ArtifactsCache, CrossFileActivityState, CrossFileConfig, CrossFileFileCache,
    CrossFileRevalidationState, CrossFileWorkspaceIndex, DependencyGraph, MetadataCache,
    ParentSelectionCache,
};
use crate::document_store::DocumentStore;
use crate::package_library::PackageLibrary;
use crate::workspace_index::WorkspaceIndex;

/// A parsed document
pub struct Document {
    pub contents: Rope,
    pub tree: Option<Tree>,
    pub loaded_packages: Vec<String>,
    pub version: Option<i32>,
    pub revision: u64,
}

impl Document {
    pub fn new(text: &str, version: Option<i32>) -> Self {
        let contents = Rope::from_str(text);
        let tree = parse_r(&contents);
        let loaded_packages = extract_loaded_packages(&tree, text);
        Self {
            contents,
            tree,
            loaded_packages,
            version,
            revision: 0,
        }
    }

    pub fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
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
        self.tree = parse_r(&self.contents);
        let text = self.contents.to_string();
        self.loaded_packages = extract_loaded_packages(&self.tree, &text);
    }

    #[allow(dead_code)]
    pub fn contents_hash(&self) -> u64 {
        self.revision
    }

    pub fn text(&self) -> String {
        self.contents.to_string()
    }
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

fn parse_r(contents: &Rope) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_r::LANGUAGE.into()).ok()?;
    let text = contents.to_string();
    parser.parse(&text, None)
}

fn extract_loaded_packages(tree: &Option<Tree>, text: &str) -> Vec<String> {
    let Some(tree) = tree else {
        return Vec::new();
    };

    let mut packages = Vec::new();
    let root = tree.root_node();

    fn visit_node(node: tree_sitter::Node, text: &str, packages: &mut Vec<String>) {
        if node.kind() == "call" {
            // Check if this is a library/require/loadNamespace call
            if let Some(func_node) = node.child_by_field_name("function") {
                let func_text = &text[func_node.byte_range()];

                if func_text == "library" || func_text == "require" || func_text == "loadNamespace"
                {
                    // Extract the first argument
                    if let Some(args_node) = node.child_by_field_name("arguments") {
                        for i in 0..args_node.child_count() {
                            if let Some(child) = args_node.child(i) {
                                if child.kind() == "argument" {
                                    if let Some(value_node) = child.child_by_field_name("value") {
                                        let value_text = &text[value_node.byte_range()];
                                        let pkg_name = value_text
                                            .trim_matches(|c: char| c == '"' || c == '\'');
                                        packages.push(pkg_name.to_string());
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Recurse into children
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                visit_node(child, text, packages);
            }
        }
    }

    visit_node(root, text, &mut packages);
    packages
}

/// Package metadata loaded from disk
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub path: PathBuf,
    pub exports: Vec<String>,
    pub description: Option<String>,
    pub version: Option<String>,
}

impl Package {
    #[allow(dead_code)]
    pub fn load(path: &Path) -> Option<Self> {
        let description_path = path.join("DESCRIPTION");
        if !description_path.exists() {
            return None;
        }

        let description_text = fs::read_to_string(&description_path).ok()?;
        let name = parse_dcf_field(&description_text, "Package")?;
        let version = parse_dcf_field(&description_text, "Version");
        let title = parse_dcf_field(&description_text, "Title");

        // Parse NAMESPACE for exports
        let exports = parse_namespace_exports(&path.join("NAMESPACE"));

        // Also include symbols from INDEX file (for datasets)
        let mut all_exports = exports;
        if let Some(index_exports) = parse_index(&path.join("INDEX")) {
            for sym in index_exports {
                if !all_exports.contains(&sym) {
                    all_exports.push(sym);
                }
            }
        }

        Some(Self {
            name,
            path: path.to_path_buf(),
            exports: all_exports,
            description: title,
            version,
        })
    }
}

#[allow(dead_code)]
fn parse_dcf_field(text: &str, field: &str) -> Option<String> {
    for line in text.lines() {
        if line.starts_with(field) && line.contains(':') {
            let value = line.split_once(':')?.1.trim();
            return Some(value.to_string());
        }
    }
    None
}

#[allow(dead_code)]
fn parse_namespace_exports(path: &PathBuf) -> Vec<String> {
    let mut exports = Vec::new();

    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return exports,
    };

    // Simple regex-free parsing of NAMESPACE export directives
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("export(") {
            // export(foo, bar, baz)
            if let Some(args) = line
                .strip_prefix("export(")
                .and_then(|s| s.strip_suffix(')'))
            {
                for arg in args.split(',') {
                    let sym = arg.trim().trim_matches('"');
                    if !sym.is_empty() {
                        exports.push(sym.to_string());
                    }
                }
            }
        } else if line.starts_with("exportPattern(") {
            // We can't expand patterns without R, skip
        } else if line.starts_with("S3method(") {
            // S3method(print, foo) exports print.foo
            if let Some(args) = line
                .strip_prefix("S3method(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let parts: Vec<&str> = args.split(',').map(|s| s.trim()).collect();
                if parts.len() >= 2 {
                    let method = format!("{}.{}", parts[0], parts[1]);
                    exports.push(method);
                }
            }
        }
    }

    exports
}

#[allow(dead_code)]
fn parse_index(path: &PathBuf) -> Option<Vec<String>> {
    let text = fs::read_to_string(path).ok()?;
    let mut symbols = Vec::new();

    for line in text.lines() {
        // INDEX format: symbol_name<whitespace>description
        if let Some(sym) = line.split_whitespace().next() {
            if !sym.is_empty()
                && sym
                    .chars()
                    .next()
                    .map(|c| c.is_alphabetic())
                    .unwrap_or(false)
            {
                symbols.push(sym.to_string());
            }
        }
    }

    Some(symbols)
}

#[allow(dead_code)]
fn parse_namespace_imports(path: &PathBuf, library: &Library) -> Vec<String> {
    let mut imports = Vec::new();

    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return imports,
    };

    for line in text.lines() {
        let line = line.trim();

        // import(pkg) - imports all exports from pkg
        if line.starts_with("import(") {
            if let Some(args) = line
                .strip_prefix("import(")
                .and_then(|s| s.strip_suffix(')'))
            {
                for pkg_name in args.split(',') {
                    let pkg_name = pkg_name.trim().trim_matches('"');
                    if let Some(pkg) = library.get(pkg_name) {
                        imports.extend(pkg.exports.clone());
                    }
                }
            }
        }
        // importFrom(pkg, sym1, sym2, ...)
        else if line.starts_with("importFrom(") {
            if let Some(args) = line
                .strip_prefix("importFrom(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let parts: Vec<&str> = args.split(',').map(|s| s.trim()).collect();
                if parts.len() >= 2 {
                    // First arg is package name, rest are symbols
                    for sym in &parts[1..] {
                        let sym = sym.trim_matches('"');
                        imports.push(sym.to_string());
                    }
                }
            }
        }
    }

    imports
}

/// Library of installed packages
#[allow(dead_code)]
pub struct Library {
    paths: Vec<PathBuf>,
    packages: RwLock<HashMap<String, Arc<Package>>>,
}

impl Library {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            packages: RwLock::new(HashMap::new()),
        }
    }

    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<Arc<Package>> {
        if let Ok(packages) = self.packages.read() {
            if let Some(pkg) = packages.get(name) {
                return Some(pkg.clone());
            }
        }

        // Try to load from library paths
        for lib_path in &self.paths {
            let pkg_path = lib_path.join(name);
            if let Some(pkg) = Package::load(&pkg_path) {
                let pkg = Arc::new(pkg);
                if let Ok(mut packages) = self.packages.write() {
                    packages.insert(name.to_string(), pkg.clone());
                }
                return Some(pkg);
            }
        }

        None
    }

    /// List all installed package names
    #[allow(dead_code)]
    pub fn list_packages(&self) -> Vec<String> {
        let mut names = Vec::new();
        for lib_path in &self.paths {
            if let Ok(entries) = fs::read_dir(lib_path) {
                for entry in entries.flatten() {
                    if entry.path().join("DESCRIPTION").exists() {
                        if let Some(name) = entry.file_name().to_str() {
                            if !names.contains(&name.to_string()) {
                                names.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
        names.sort();
        names
    }
}

/// Global LSP state
pub struct WorldState {
    // Document management (new architecture)
    pub document_store: DocumentStore,
    pub workspace_index_new: WorkspaceIndex,

    // Legacy fields (kept for migration compatibility)
    pub documents: HashMap<Url, Document>,
    pub workspace_index: HashMap<Url, Document>,
    pub workspace_imports: Vec<String>, // Symbols imported via workspace NAMESPACE

    // Workspace configuration
    pub workspace_folders: Vec<Url>,
    pub library: Library,

    // Package function awareness
    // Manages installed packages, their exports, and caching for package-aware scope resolution
    // Requirement 13.4: THE Package_Cache SHALL support concurrent read access from multiple LSP handlers
    // Arc allows sharing across async tasks without holding WorldState lock
    pub package_library: Arc<PackageLibrary>,

    // Caches
    pub help_cache: crate::help::HelpCache,
    pub cross_file_file_cache: CrossFileFileCache,
    pub diagnostics_gate: CrossFileDiagnosticsGate,

    // Cross-file state
    pub cross_file_config: CrossFileConfig,
    pub cross_file_meta: MetadataCache,
    pub cross_file_graph: DependencyGraph,
    pub cross_file_cache: ArtifactsCache,
    pub cross_file_revalidation: CrossFileRevalidationState,
    pub cross_file_activity: CrossFileActivityState,
    pub cross_file_workspace_index: CrossFileWorkspaceIndex,
    #[allow(dead_code)]
    pub cross_file_parent_cache: ParentSelectionCache,
    pub package_library_ready: bool,
}

impl WorldState {
    /// Creates a new WorldState initialized with default cross-file configuration and empty caches.
    ///
    /// The returned state is populated with:
    /// - default CrossFileConfig (logged at initialization),
    /// - empty document and workspace indexes (legacy and new),
    /// - a Library constructed from `library_paths`,
    /// - an empty, concurrently accessible PackageLibrary,
    /// - all cross-file caches and auxiliary structures in their default state.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::PathBuf;
    /// let ws = WorldState::new(vec![PathBuf::from("/usr/lib/R/library")]);
    /// // newly created state has no opened documents or workspace folders by default
    /// assert!(ws.documents.is_empty());
    /// assert!(ws.workspace_folders.is_empty());
    /// ```
    pub fn new(library_paths: Vec<PathBuf>) -> Self {
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
        log::info!(
            "  undefined_variables_enabled: {}",
            config.undefined_variables_enabled
        );
        log::info!("  Diagnostic severities:");
        log::info!("    missing_file: {:?}", config.missing_file_severity);
        log::info!(
            "    circular_dependency: {:?}",
            config.circular_dependency_severity
        );
        log::info!("    out_of_scope: {:?}", config.out_of_scope_severity);
        log::info!(
            "    ambiguous_parent: {:?}",
            config.ambiguous_parent_severity
        );
        log::info!("    max_chain_depth: {:?}", config.max_chain_depth_severity);

        Self {
            // New architecture components
            document_store: DocumentStore::new(Default::default()),
            workspace_index_new: WorkspaceIndex::new(Default::default()),

            // Legacy fields (kept for migration compatibility)
            documents: HashMap::new(),
            workspace_index: HashMap::new(),
            workspace_imports: Vec::new(),

            // Workspace configuration
            workspace_folders: Vec::new(),
            library: Library::new(library_paths),

            // Package function awareness
            // Initialize with empty state - will be populated via initialize() or async initialization
            // Requirement 13.4: THE Package_Cache SHALL support concurrent read access
            package_library: Arc::new(PackageLibrary::new_empty()),

            // Caches
            help_cache: crate::help::HelpCache::new(),
            cross_file_file_cache: CrossFileFileCache::new(),
            diagnostics_gate: CrossFileDiagnosticsGate::new(),

            // Cross-file state
            cross_file_config: config,
            cross_file_meta: MetadataCache::new(),
            cross_file_graph: DependencyGraph::new(),
            cross_file_cache: ArtifactsCache::new(),
            cross_file_revalidation: CrossFileRevalidationState::new(),
            cross_file_activity: CrossFileActivityState::new(),
            cross_file_workspace_index: CrossFileWorkspaceIndex::new(),
            cross_file_parent_cache: ParentSelectionCache::new(),
            package_library_ready: false,
        }
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
        DefaultContentProvider::with_legacy(
            &self.document_store,
            &self.workspace_index_new,
            &self.cross_file_file_cache,
            &self.documents,
            &self.workspace_index,
            &self.cross_file_workspace_index,
        )
    }

    pub fn open_document(&mut self, uri: Url, text: &str, version: Option<i32>) {
        self.documents.insert(uri, Document::new(text, version));
    }

    pub fn close_document(&mut self, uri: &Url) {
        self.documents.remove(uri);
    }

    pub fn apply_change(&mut self, uri: &Url, change: TextDocumentContentChangeEvent) {
        if let Some(doc) = self.documents.get_mut(uri) {
            doc.apply_change(change);
        }
    }

    pub fn get_document(&self, uri: &Url) -> Option<&Document> {
        self.documents.get(uri)
    }

    /// Get enriched metadata for a URI, preferring already-enriched sources.
    ///
    /// Priority order:
    /// 1. DocumentStore (open documents with enriched metadata)
    /// 2. WorkspaceIndex (new unified index)
    /// 3. Legacy cross_file_workspace_index
    /// 4. Legacy documents HashMap (re-extract metadata)
    /// 5. File cache (re-extract metadata)
    pub fn get_enriched_metadata(&self, uri: &Url) -> Option<crate::cross_file::CrossFileMetadata> {
        self.document_store
            .get_without_touch(uri)
            .map(|doc| doc.metadata.clone())
            .or_else(|| self.workspace_index_new.get_metadata(uri))
            .or_else(|| self.cross_file_workspace_index.get_metadata(uri))
            .or_else(|| {
                self.documents
                    .get(uri)
                    .map(|doc| crate::cross_file::extract_metadata(&doc.text()))
            })
            .or_else(|| {
                self.cross_file_file_cache
                    .get(uri)
                    .map(|content| crate::cross_file::extract_metadata(&content))
            })
    }

    #[allow(dead_code)]
    pub fn index_workspace(&mut self) {
        let folders = self.workspace_folders.clone();
        log::info!("Indexing {} workspace folders", folders.len());
        for folder in &folders {
            log::info!("Indexing folder: {}", folder);
            if let Ok(path) = folder.to_file_path() {
                self.index_directory(&path);
            }
        }
        log::info!("Indexed {} workspace files", self.workspace_index.len());

        // Load workspace NAMESPACE imports
        self.load_workspace_namespace();
    }

    /// Apply pre-scanned workspace index results (for non-blocking initialization)
    ///
    /// **Validates: Requirements 11.1, 13.1**
    pub fn apply_workspace_index(
        &mut self,
        index: HashMap<Url, Document>,
        imports: Vec<String>,
        cross_file_entries: HashMap<Url, crate::cross_file::workspace_index::IndexEntry>,
        new_index_entries: HashMap<Url, crate::workspace_index::IndexEntry>,
    ) {
        self.workspace_index = index;
        self.workspace_imports = imports;

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
            "Applied {} workspace files, {} imports, {} cross-file entries, {} new index entries",
            self.workspace_index.len(),
            self.workspace_imports.len(),
            self.cross_file_workspace_index.uris().len(),
            self.workspace_index_new.len()
        );
    }

    #[allow(dead_code)]
    fn load_workspace_namespace(&mut self) {
        for folder_url in &self.workspace_folders {
            if let Ok(folder_path) = folder_url.to_file_path() {
                let namespace_path = folder_path.join("NAMESPACE");
                if namespace_path.exists() {
                    self.workspace_imports =
                        parse_namespace_imports(&namespace_path, &self.library);
                    log::info!(
                        "Loaded {} workspace imports from NAMESPACE",
                        self.workspace_imports.len()
                    );
                    break; // Only process first workspace folder with NAMESPACE
                }
            }
        }
    }

    #[allow(dead_code)]
    fn index_directory(&mut self, dir: &std::path::Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_dir() {
                self.index_directory(&path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("R") {
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(uri) = Url::from_file_path(&path) {
                        log::trace!("Indexing file: {}", uri);
                        self.workspace_index.insert(uri, Document::new(&text, None));
                    }
                }
            }
        }
    }
}

/// Scan workspace folders for R files without holding any locks (Requirement 13a)
///
/// Returns:
/// - `HashMap<Url, Document>` - Legacy index for backward compatibility
/// - `Vec<String>` - Workspace imports from NAMESPACE
/// - `HashMap<Url, crate::cross_file::workspace_index::IndexEntry>` - Cross-file entries (legacy)
/// - `HashMap<Url, crate::workspace_index::IndexEntry>` - New unified WorkspaceIndex entries
///
/// **Validates: Requirements 11.1, 11.2, 11.3, 11.4, 11.5**
pub type WorkspaceScanResult = (
    HashMap<Url, Document>,
    Vec<String>,
    HashMap<Url, crate::cross_file::workspace_index::IndexEntry>,
    HashMap<Url, crate::workspace_index::IndexEntry>,
);

pub fn scan_workspace(folders: &[Url], max_chain_depth: usize) -> WorkspaceScanResult {
    let mut index = HashMap::new();
    let mut imports = Vec::new();
    let mut cross_file_entries = HashMap::new();
    let mut new_index_entries = HashMap::new();

    // Get workspace root for path resolution
    let workspace_root = folders.first().cloned();

    for folder in folders {
        log::info!("Scanning folder: {}", folder);
        if let Ok(path) = folder.to_file_path() {
            scan_directory(
                &path,
                &mut index,
                &mut cross_file_entries,
                &mut new_index_entries,
            );

            // Check for NAMESPACE file
            let namespace_path = path.join("NAMESPACE");
            if namespace_path.exists() && imports.is_empty() {
                if let Ok(text) = fs::read_to_string(&namespace_path) {
                    imports = parse_namespace_imports_from_text(&text);
                    log::info!("Found {} imports from NAMESPACE", imports.len());
                }
            }
        }
    }

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
            log::trace!("Workspace scan enrichment converged after {} iteration(s)", iteration + 1);
            break;
        }

        // Build metadata map from current state
        let metadata_map: HashMap<Url, crate::cross_file::CrossFileMetadata> = new_index_entries
            .iter()
            .map(|(uri, entry)| (uri.clone(), entry.metadata.clone()))
            .collect();

        let mut newly_enriched = Vec::new();

        // Only process files that need enrichment
        for uri in &files_needing_enrichment {
            if let Some(entry) = new_index_entries.get_mut(uri) {
                let old_inherited = entry.metadata.inherited_working_directory.clone();
                crate::cross_file::enrich_metadata_with_inherited_wd(
                    &mut entry.metadata,
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
                crate::cross_file::enrich_metadata_with_inherited_wd(
                    &mut entry.metadata,
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
            log::trace!("Workspace scan enrichment converged after {} iteration(s)", iteration + 1);
            break;
        }
    }

    log::info!(
        "Scanned {} workspace files ({} with cross-file metadata, {} new index entries)",
        index.len(),
        cross_file_entries.len(),
        new_index_entries.len()
    );
    (index, imports, cross_file_entries, new_index_entries)
}

/// Directories to skip during workspace scanning.
///
/// This is a conservative list of directories that are extremely unlikely to
/// contain user R source files. The workspace scan runs in the background,
/// so the primary goal is to avoid wasting time on directories that would
/// never contain R files.
///
/// Note: We intentionally keep this list minimal. Directories like `renv/`,
/// `.Rproj.user/`, or `build/` might legitimately contain R code that users
/// want to navigate to. Only skip directories that are definitively not
/// R-related.
const SKIP_DIRECTORIES: &[&str] = &[
    "node_modules", // JavaScript dependencies (can have 100k+ files)
    ".git",         // Git internal files
    "target",       // Rust build artifacts
];

/// Check if a directory should be skipped during scanning
fn should_skip_directory(dir_name: &str) -> bool {
    SKIP_DIRECTORIES
        .iter()
        .any(|skip| dir_name.eq_ignore_ascii_case(skip))
}

fn scan_directory(
    dir: &std::path::Path,
    index: &mut HashMap<Url, Document>,
    cross_file_entries: &mut HashMap<Url, crate::cross_file::workspace_index::IndexEntry>,
    new_index_entries: &mut HashMap<Url, crate::workspace_index::IndexEntry>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            // Skip common non-R directories to improve scan performance
            if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                if should_skip_directory(dir_name) {
                    log::trace!("Skipping directory: {}", path.display());
                    continue;
                }
            }
            scan_directory(&path, index, cross_file_entries, new_index_entries);
        } else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            // Match both .R and .r extensions (case-insensitive)
            if ext.eq_ignore_ascii_case("r") {
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(uri) = Url::from_file_path(&path) {
                        log::trace!("Scanning file: {}", uri);
                        let doc = Document::new(&text, None);

                        // Also compute cross-file metadata and artifacts
                        if let Ok(metadata_result) = fs::metadata(&path) {
                            let cross_file_meta = crate::cross_file::extract_metadata(&text);

                            // Compute artifacts if we have a tree
                            // Use compute_artifacts_with_metadata to include declared symbols from directives
                            // **Validates: Requirements 5.1, 5.2, 5.3, 5.4** (Diagnostic suppression for declared symbols)
                            let artifacts = if let Some(tree) = doc.tree.as_ref() {
                                crate::cross_file::scope::compute_artifacts_with_metadata(&uri, tree, &text, Some(&cross_file_meta))
                            } else {
                                crate::cross_file::scope::ScopeArtifacts::default()
                            };

                            let snapshot =
                                crate::cross_file::file_cache::FileSnapshot::with_content_hash(
                                    &metadata_result,
                                    &text,
                                );

                            // Create legacy cross-file entry
                            cross_file_entries.insert(
                                uri.clone(),
                                crate::cross_file::workspace_index::IndexEntry {
                                    snapshot: snapshot.clone(),
                                    metadata: cross_file_meta.clone(),
                                    artifacts: artifacts.clone(),
                                    indexed_at_version: 0, // Initial version; not modified by insert()
                                },
                            );

                            // Create new unified IndexEntry with all derived data
                            // **Validates: Requirements 11.1, 11.2, 11.3**
                            new_index_entries.insert(
                                uri.clone(),
                                crate::workspace_index::IndexEntry {
                                    contents: doc.contents.clone(),
                                    tree: doc.tree.clone(),
                                    loaded_packages: doc.loaded_packages.clone(),
                                    snapshot,
                                    metadata: cross_file_meta,
                                    artifacts,
                                    indexed_at_version: 0, // Initial version
                                },
                            );
                        }

                        index.insert(uri, doc);
                    }
                }
            }
        }
    }
}

/// Parse NAMESPACE imports without needing Library reference
fn parse_namespace_imports_from_text(text: &str) -> Vec<String> {
    let mut imports = Vec::new();

    for line in text.lines() {
        let line = line.trim();

        // importFrom(pkg, sym1, sym2, ...)
        if line.starts_with("importFrom(") {
            if let Some(args) = line
                .strip_prefix("importFrom(")
                .and_then(|s| s.strip_suffix(')'))
            {
                let parts: Vec<&str> = args.split(',').map(|s| s.trim()).collect();
                if parts.len() >= 2 {
                    for sym in &parts[1..] {
                        let sym = sym.trim_matches('"');
                        imports.push(sym.to_string());
                    }
                }
            }
        }
    }

    imports
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};

    // Include workspace scanning tests
    include!("state_tests.rs");

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
}
