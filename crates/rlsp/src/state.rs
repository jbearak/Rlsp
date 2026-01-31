//
// state.rs
//
// Copyright (C) 2025 Posit Software, PBC. All rights reserved.
//
//

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use ropey::Rope;
use tower_lsp::lsp_types::TextDocumentContentChangeEvent;
use tower_lsp::lsp_types::Url;
use tree_sitter::Parser;
use tree_sitter::Tree;

use crate::cross_file::{
    ArtifactsCache, CrossFileActivityState, CrossFileConfig, CrossFileFileCache,
    CrossFileRevalidationState, CrossFileWorkspaceIndex, DependencyGraph, MetadataCache,
    ParentSelectionCache,
};
use crate::cross_file::revalidation::CrossFileDiagnosticsGate;

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
    parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .ok()?;
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
                
                if func_text == "library" || func_text == "require" || func_text == "loadNamespace" {
                    // Extract the first argument
                    if let Some(args_node) = node.child_by_field_name("arguments") {
                        for i in 0..args_node.child_count() {
                            if let Some(child) = args_node.child(i) {
                                if child.kind() == "argument" {
                                    if let Some(value_node) = child.child_by_field_name("value") {
                                        let value_text = &text[value_node.byte_range()];
                                        let pkg_name = value_text.trim_matches(|c: char| c == '"' || c == '\'');
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
    pub fn load(path: &PathBuf) -> Option<Self> {
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
            path: path.clone(),
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
            let value = line.splitn(2, ':').nth(1)?.trim();
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
            if let Some(args) = line.strip_prefix("export(").and_then(|s| s.strip_suffix(')')) {
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
            if let Some(args) = line.strip_prefix("S3method(").and_then(|s| s.strip_suffix(')')) {
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
            if !sym.is_empty() && sym.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
                symbols.push(sym.to_string());
            }
        }
    }

    Some(symbols)
}

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
            if let Some(args) = line.strip_prefix("import(").and_then(|s| s.strip_suffix(')')) {
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
            if let Some(args) = line.strip_prefix("importFrom(").and_then(|s| s.strip_suffix(')')) {
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
    // Existing fields
    pub documents: HashMap<Url, Document>,
    pub workspace_folders: Vec<Url>,
    pub library: Library,
    pub workspace_index: HashMap<Url, Document>,
    pub workspace_imports: Vec<String>, // Symbols imported via workspace NAMESPACE
    pub help_cache: crate::help::HelpCache,
    pub diagnostics_gate: CrossFileDiagnosticsGate,

    // Cross-file state
    pub cross_file_config: CrossFileConfig,
    pub cross_file_meta: MetadataCache,
    pub cross_file_graph: DependencyGraph,
    pub cross_file_cache: ArtifactsCache,
    pub cross_file_file_cache: CrossFileFileCache,
    pub cross_file_revalidation: CrossFileRevalidationState,
    pub cross_file_activity: CrossFileActivityState,
    pub cross_file_workspace_index: CrossFileWorkspaceIndex,
    #[allow(dead_code)]
    pub cross_file_parent_cache: ParentSelectionCache,
}

impl WorldState {
    pub fn new(library_paths: Vec<PathBuf>) -> Self {
        let config = CrossFileConfig::default();
        
        // Log default cross-file configuration at startup
        log::info!("Initializing cross-file configuration with defaults:");
        log::info!("  max_backward_depth: {}", config.max_backward_depth);
        log::info!("  max_forward_depth: {}", config.max_forward_depth);
        log::info!("  max_chain_depth: {}", config.max_chain_depth);
        log::info!("  assume_call_site: {:?}", config.assume_call_site);
        log::info!("  index_workspace: {}", config.index_workspace);
        log::info!("  max_revalidations_per_trigger: {}", config.max_revalidations_per_trigger);
        log::info!("  revalidation_debounce_ms: {}", config.revalidation_debounce_ms);
        log::info!("  undefined_variables_enabled: {}", config.undefined_variables_enabled);
        log::info!("  Diagnostic severities:");
        log::info!("    missing_file: {:?}", config.missing_file_severity);
        log::info!("    circular_dependency: {:?}", config.circular_dependency_severity);
        log::info!("    out_of_scope: {:?}", config.out_of_scope_severity);
        log::info!("    ambiguous_parent: {:?}", config.ambiguous_parent_severity);
        log::info!("    max_chain_depth: {:?}", config.max_chain_depth_severity);
        
        Self {
            documents: HashMap::new(),
            workspace_folders: Vec::new(),
            library: Library::new(library_paths),
            workspace_index: HashMap::new(),
            workspace_imports: Vec::new(),
            help_cache: crate::help::HelpCache::new(),
            diagnostics_gate: CrossFileDiagnosticsGate::new(),
            // Cross-file state
            cross_file_config: config,
            cross_file_meta: MetadataCache::new(),
            cross_file_graph: DependencyGraph::new(),
            cross_file_cache: ArtifactsCache::new(),
            cross_file_file_cache: CrossFileFileCache::new(),
            cross_file_revalidation: CrossFileRevalidationState::new(),
            cross_file_activity: CrossFileActivityState::new(),
            cross_file_workspace_index: CrossFileWorkspaceIndex::new(),
            cross_file_parent_cache: ParentSelectionCache::new(),
        }
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
    pub fn apply_workspace_index(&mut self, index: HashMap<Url, Document>, imports: Vec<String>, cross_file_entries: HashMap<Url, crate::cross_file::workspace_index::IndexEntry>) {
        self.workspace_index = index;
        self.workspace_imports = imports;
        
        // Populate cross-file workspace index
        for (uri, entry) in cross_file_entries {
            log::info!("Indexing cross-file entry: {} (exported symbols: {})", 
                uri, entry.artifacts.exported_interface.len());
            self.cross_file_workspace_index.insert(uri, entry);
        }
        
        log::info!("Applied {} workspace files, {} imports, {} cross-file entries", 
            self.workspace_index.len(), self.workspace_imports.len(), 
            self.cross_file_workspace_index.uris().len());
    }
    
    fn load_workspace_namespace(&mut self) {
        for folder_url in &self.workspace_folders {
            if let Ok(folder_path) = folder_url.to_file_path() {
                let namespace_path = folder_path.join("NAMESPACE");
                if namespace_path.exists() {
                    self.workspace_imports = parse_namespace_imports(&namespace_path, &self.library);
                    log::info!("Loaded {} workspace imports from NAMESPACE", self.workspace_imports.len());
                    break; // Only process first workspace folder with NAMESPACE
                }
            }
        }
    }

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
pub fn scan_workspace(folders: &[Url]) -> (HashMap<Url, Document>, Vec<String>, HashMap<Url, crate::cross_file::workspace_index::IndexEntry>) {
    let mut index = HashMap::new();
    let mut imports = Vec::new();
    let mut cross_file_entries = HashMap::new();

    for folder in folders {
        log::info!("Scanning folder: {}", folder);
        if let Ok(path) = folder.to_file_path() {
            scan_directory(&path, &mut index, &mut cross_file_entries);
            
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

    log::info!("Scanned {} workspace files ({} with cross-file metadata)", index.len(), cross_file_entries.len());
    (index, imports, cross_file_entries)
}

fn scan_directory(dir: &std::path::Path, index: &mut HashMap<Url, Document>, cross_file_entries: &mut HashMap<Url, crate::cross_file::workspace_index::IndexEntry>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        
        if path.is_dir() {
            scan_directory(&path, index, cross_file_entries);
        } else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            // Match both .R and .r extensions (case-insensitive)
            if ext.eq_ignore_ascii_case("r") {
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(uri) = Url::from_file_path(&path) {
                        log::trace!("Scanning file: {}", uri);
                        let doc = Document::new(&text, None);
                        index.insert(uri.clone(), doc);
                        
                        // Also compute cross-file metadata and artifacts
                        if let Ok(metadata_result) = fs::metadata(&path) {
                            let cross_file_meta = crate::cross_file::extract_metadata(&text);
                            
                            // Compute artifacts if we have a tree
                            let artifacts = if let Some(tree) = index.get(&uri).and_then(|d| d.tree.as_ref()) {
                                crate::cross_file::scope::compute_artifacts(&uri, tree, &text)
                            } else {
                                crate::cross_file::scope::ScopeArtifacts::default()
                            };
                            
                            let snapshot = crate::cross_file::file_cache::FileSnapshot::with_content_hash(
                                &metadata_result,
                                &text,
                            );
                            
                            cross_file_entries.insert(uri, crate::cross_file::workspace_index::IndexEntry {
                                snapshot,
                                metadata: cross_file_meta,
                                artifacts,
                                indexed_at_version: 0, // Initial version; not modified by insert()
                            });
                        }
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
            if let Some(args) = line.strip_prefix("importFrom(").and_then(|s| s.strip_suffix(')')) {
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
                start: Position { line: 0, character: 6 },
                end: Position { line: 0, character: 11 },
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
                start: Position { line: 0, character: 3 },
                end: Position { line: 0, character: 3 },
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
                start: Position { line: 0, character: 2 },
                end: Position { line: 0, character: 2 },
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
                start: Position { line: 0, character: 1 },
                end: Position { line: 0, character: 3 },
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
                start: Position { line: 1, character: 2 }, // After emoji (2 UTF-16 units)
                end: Position { line: 1, character: 7 },   // End of "line2"
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
