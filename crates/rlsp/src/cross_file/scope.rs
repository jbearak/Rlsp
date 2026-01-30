//
// cross_file/scope.rs
//
// Scope resolution for cross-file awareness
//

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use tower_lsp::lsp_types::Url;
use tree_sitter::{Node, Tree};

use super::source_detect::detect_source_calls;
use super::types::{byte_offset_to_utf16_column, ForwardSource};

/// Symbol kind
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Variable,
}

/// A symbol with its definition location
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub source_uri: Url,
    /// 0-based line of definition
    pub defined_line: u32,
    /// 0-based UTF-16 column of definition
    pub defined_column: u32,
    pub signature: Option<String>,
}

impl Hash for ScopedSymbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.kind.hash(state);
        self.source_uri.hash(state);
        self.defined_line.hash(state);
        self.defined_column.hash(state);
    }
}

/// A scope-introducing event within a file
#[derive(Debug, Clone)]
pub enum ScopeEvent {
    /// A symbol definition at a specific position
    Def {
        line: u32,
        column: u32,
        symbol: ScopedSymbol,
    },
    /// A source() call that introduces symbols from another file
    Source {
        line: u32,
        column: u32,
        source: ForwardSource,
    },
}

/// Per-file scope artifacts
#[derive(Debug, Clone)]
pub struct ScopeArtifacts {
    /// Exported interface (all symbols defined in this file)
    pub exported_interface: HashMap<String, ScopedSymbol>,
    /// Timeline of scope events in document order
    pub timeline: Vec<ScopeEvent>,
    /// Hash of exported interface for change detection
    pub interface_hash: u64,
}

impl Default for ScopeArtifacts {
    fn default() -> Self {
        Self {
            exported_interface: HashMap::new(),
            timeline: Vec::new(),
            interface_hash: 0,
        }
    }
}

/// Computed scope at a position
#[derive(Debug, Clone, Default)]
pub struct ScopeAtPosition {
    pub symbols: HashMap<String, ScopedSymbol>,
    pub chain: Vec<Url>,
}

/// Compute scope artifacts for a file from its AST.
/// This includes both definitions and source() calls in the timeline.
pub fn compute_artifacts(uri: &Url, tree: &Tree, content: &str) -> ScopeArtifacts {
    let mut artifacts = ScopeArtifacts::default();
    let root = tree.root_node();

    // Collect definitions from AST
    collect_definitions(root, content, uri, &mut artifacts);

    // Collect source() calls and add them to timeline
    let source_calls = detect_source_calls(tree, content);
    for source in source_calls {
        // Skip local=TRUE sources (symbols not inherited)
        if !source.local {
            artifacts.timeline.push(ScopeEvent::Source {
                line: source.line,
                column: source.column,
                source,
            });
        }
    }

    // Sort timeline by position for correct ordering
    artifacts.timeline.sort_by_key(|event| match event {
        ScopeEvent::Def { line, column, .. } => (*line, *column),
        ScopeEvent::Source { line, column, .. } => (*line, *column),
    });

    // Compute interface hash
    artifacts.interface_hash = compute_interface_hash(&artifacts.exported_interface);

    artifacts
}

/// Compute scope at a specific position (single file, no traversal)
pub fn scope_at_position(
    artifacts: &ScopeArtifacts,
    line: u32,
    column: u32,
) -> ScopeAtPosition {
    let mut scope = ScopeAtPosition::default();

    // Include symbols defined before the given position
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                // Include if definition is before or at the position
                if (*def_line, *def_col) <= (line, column) {
                    scope.symbols.insert(symbol.name.clone(), symbol.clone());
                }
            }
            ScopeEvent::Source { .. } => {
                // Source events are handled by scope_at_position_with_deps
            }
        }
    }

    scope
}

/// Compute scope at a position with cross-file traversal.
/// This is the main entry point for cross-file scope resolution.
pub fn scope_at_position_with_deps<F>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
{
    let mut visited = HashSet::new();
    scope_at_position_recursive(uri, line, column, get_artifacts, resolve_path, max_depth, 0, &mut visited)
}

fn scope_at_position_recursive<F>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<Url>,
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
{
    let mut scope = ScopeAtPosition::default();

    if current_depth >= max_depth || visited.contains(uri) {
        return scope;
    }
    visited.insert(uri.clone());
    scope.chain.push(uri.clone());

    let artifacts = match get_artifacts(uri) {
        Some(a) => a,
        None => return scope,
    };

    // Process timeline events up to the requested position
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                if (*def_line, *def_col) <= (line, column) {
                    // Local definitions take precedence (don't overwrite)
                    scope.symbols.entry(symbol.name.clone()).or_insert_with(|| symbol.clone());
                }
            }
            ScopeEvent::Source { line: src_line, column: src_col, source } => {
                // Only include if source() call is before the position
                if (*src_line, *src_col) < (line, column) {
                    // Resolve the path and get symbols from sourced file
                    if let Some(child_uri) = resolve_path(&source.path, uri) {
                        let child_scope = scope_at_position_recursive(
                            &child_uri,
                            u32::MAX, // Include all symbols from sourced file
                            u32::MAX,
                            get_artifacts,
                            resolve_path,
                            max_depth,
                            current_depth + 1,
                            visited,
                        );
                        // Merge child symbols (local definitions take precedence)
                        for (name, symbol) in child_scope.symbols {
                            scope.symbols.entry(name).or_insert(symbol);
                        }
                        scope.chain.extend(child_scope.chain);
                    }
                }
            }
        }
    }

    scope
}

fn collect_definitions(
    node: Node,
    content: &str,
    uri: &Url,
    artifacts: &mut ScopeArtifacts,
) {
    // Check for assignment expressions
    if node.kind() == "binary_operator" {
        if let Some(symbol) = try_extract_assignment(node, content, uri) {
            let event = ScopeEvent::Def {
                line: symbol.defined_line,
                column: symbol.defined_column,
                symbol: symbol.clone(),
            };
            artifacts.timeline.push(event);
            artifacts.exported_interface.insert(symbol.name.clone(), symbol);
        }
    }
    
    // Check for assign() calls (Requirement 17.4)
    if node.kind() == "call" {
        if let Some(symbol) = try_extract_assign_call(node, content, uri) {
            let event = ScopeEvent::Def {
                line: symbol.defined_line,
                column: symbol.defined_column,
                symbol: symbol.clone(),
            };
            artifacts.timeline.push(event);
            artifacts.exported_interface.insert(symbol.name.clone(), symbol);
        }
    }

    // Recurse into children
    for child in node.children(&mut node.walk()) {
        collect_definitions(child, content, uri, artifacts);
    }
}

/// Extract definition from assign("name", value) calls.
/// Only handles string literal names per Requirement 17.4.
fn try_extract_assign_call(node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // Get function name
    let func_node = node.child_by_field_name("function")?;
    let func_name = node_text(func_node, content);
    
    if func_name != "assign" {
        return None;
    }
    
    // Get arguments
    let args_node = node.child_by_field_name("arguments")?;
    
    // Find the first argument (the name)
    let mut name_arg = None;
    for child in args_node.children(&mut args_node.walk()) {
        if child.kind() == "argument" {
            // Check if it's a named argument
            if let Some(name_node) = child.child_by_field_name("name") {
                let arg_name = node_text(name_node, content);
                if arg_name == "x" {
                    // This is the name argument
                    name_arg = child.child_by_field_name("value");
                    break;
                }
            } else {
                // Positional argument - first one is the name
                name_arg = child.child_by_field_name("value");
                break;
            }
        }
    }
    
    let name_node = name_arg?;
    
    // Only handle string literals
    if name_node.kind() != "string" {
        return None;
    }
    
    // Extract the string content (remove quotes)
    let name_text = node_text(name_node, content);
    let name = name_text.trim_matches(|c| c == '"' || c == '\'').to_string();
    
    if name.is_empty() {
        return None;
    }
    
    // Get position with UTF-16 column
    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);
    
    Some(ScopedSymbol {
        name,
        kind: SymbolKind::Variable,
        source_uri: uri.clone(),
        defined_line: start.row as u32,
        defined_column: column,
        signature: None,
    })
}

fn try_extract_assignment(node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // Check if this is an assignment operator - the operator is a direct child, not a field
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();
    
    if children.len() != 3 {
        return None;
    }
    
    let lhs = children[0];
    let op = children[1];
    let rhs = children[2];
    
    // Check operator
    let op_text = node_text(op, content);
    if !matches!(op_text, "<-" | "=" | "<<-") {
        return None;
    }

    // Get the left-hand side (name)
    if lhs.kind() != "identifier" {
        return None;
    }
    let name = node_text(lhs, content).to_string();

    // Get the right-hand side to determine kind
    let (kind, signature) = if rhs.kind() == "function_definition" {
        let sig = extract_function_signature(rhs, &name, content);
        (SymbolKind::Function, Some(sig))
    } else {
        (SymbolKind::Variable, None)
    };

    // Get position with UTF-16 column
    let start = lhs.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);

    Some(ScopedSymbol {
        name,
        kind,
        source_uri: uri.clone(),
        defined_line: start.row as u32,
        defined_column: column,
        signature,
    })
}

fn extract_function_signature(func_node: Node, name: &str, content: &str) -> String {
    // Find the parameters node
    let mut cursor = func_node.walk();
    for child in func_node.children(&mut cursor) {
        if child.kind() == "parameters" {
            let params = node_text(child, content);
            return format!("{}{}", name, params);
        }
    }
    format!("{}()", name)
}

fn node_text<'a>(node: Node<'a>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

fn compute_interface_hash(interface: &HashMap<String, ScopedSymbol>) -> u64 {
    let mut hasher = DefaultHasher::new();
    // Sort keys for deterministic hashing
    let mut keys: Vec<_> = interface.keys().collect();
    keys.sort();
    for key in keys {
        if let Some(symbol) = interface.get(key) {
            symbol.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Compute scope at a position with backward directive support.
/// This processes backward directives FIRST (parent context), then forward sources.
/// 
/// Property 19: Backward-First Resolution Order
/// - Backward directives establish parent context (symbols available before this file runs)
/// - Forward source() calls add symbols in document order
pub fn scope_at_position_with_backward<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
    parent_call_site: Option<(u32, u32)>, // (line, column) in parent where this file is sourced
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
    G: Fn(&Url) -> Option<super::types::CrossFileMetadata>,
{
    let mut visited = HashSet::new();
    scope_at_position_with_backward_recursive(
        uri, line, column, get_artifacts, get_metadata, resolve_path,
        max_depth, 0, &mut visited, parent_call_site,
    )
}

fn scope_at_position_with_backward_recursive<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    resolve_path: &impl Fn(&str, &Url) -> Option<Url>,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<Url>,
    _parent_call_site: Option<(u32, u32)>, // Currently unused but reserved for future use
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
    G: Fn(&Url) -> Option<super::types::CrossFileMetadata>,
{
    let mut scope = ScopeAtPosition::default();

    if current_depth >= max_depth || visited.contains(uri) {
        return scope;
    }
    visited.insert(uri.clone());
    scope.chain.push(uri.clone());

    let artifacts = match get_artifacts(uri) {
        Some(a) => a,
        None => return scope,
    };

    // STEP 1: Process backward directives FIRST (parent context)
    // This establishes symbols that are available at the START of this file
    if let Some(metadata) = get_metadata(uri) {
        for directive in &metadata.sourced_by {
            if let Some(parent_uri) = resolve_path(&directive.path, uri) {
                // Get the call site in the parent
                let call_site = match &directive.call_site {
                    super::types::CallSiteSpec::Line(n) => Some((*n, u32::MAX)), // end of line
                    super::types::CallSiteSpec::Match(_) => None, // TODO: implement match
                    super::types::CallSiteSpec::Default => Some((u32::MAX, u32::MAX)), // end of file
                };

                if let Some((call_line, call_col)) = call_site {
                    // Get parent's scope at the call site
                    let parent_scope = scope_at_position_with_backward_recursive(
                        &parent_uri,
                        call_line,
                        call_col,
                        get_artifacts,
                        get_metadata,
                        resolve_path,
                        max_depth,
                        current_depth + 1,
                        visited,
                        None, // parent doesn't have a parent call site in this context
                    );

                    // Merge parent symbols (they are available at the START of this file)
                    // These have lower precedence than local definitions
                    for (name, symbol) in parent_scope.symbols {
                        scope.symbols.entry(name).or_insert(symbol);
                    }
                    scope.chain.extend(parent_scope.chain);
                }
            }
        }
    }

    // STEP 2: Process timeline events (local definitions and forward sources)
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                if (*def_line, *def_col) <= (line, column) {
                    // Local definitions take precedence over inherited symbols
                    scope.symbols.insert(symbol.name.clone(), symbol.clone());
                }
            }
            ScopeEvent::Source { line: src_line, column: src_col, source } => {
                // Only include if source() call is before the position
                if (*src_line, *src_col) < (line, column) {
                    // Resolve the path and get symbols from sourced file
                    if let Some(child_uri) = resolve_path(&source.path, uri) {
                        let child_scope = scope_at_position_with_backward_recursive(
                            &child_uri,
                            u32::MAX, // Include all symbols from sourced file
                            u32::MAX,
                            get_artifacts,
                            get_metadata,
                            resolve_path,
                            max_depth,
                            current_depth + 1,
                            visited,
                            Some((*src_line, *src_col)), // pass the call site
                        );
                        // Merge child symbols (local definitions take precedence)
                        for (name, symbol) in child_scope.symbols {
                            scope.symbols.entry(name).or_insert(symbol);
                        }
                        scope.chain.extend(child_scope.chain);
                    }
                }
            }
        }
    }

    scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
        parser.parse(code, None).unwrap()
    }

    fn test_uri() -> Url {
        Url::parse("file:///test.R").unwrap()
    }

    #[test]
    fn test_function_definition() {
        let code = "my_func <- function(x, y) { x + y }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        let symbol = artifacts.exported_interface.get("my_func").unwrap();
        assert_eq!(symbol.kind, SymbolKind::Function);
        assert_eq!(symbol.signature, Some("my_func(x, y)".to_string()));
    }

    #[test]
    fn test_variable_definition() {
        let code = "x <- 42";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        let symbol = artifacts.exported_interface.get("x").unwrap();
        assert_eq!(symbol.kind, SymbolKind::Variable);
        assert!(symbol.signature.is_none());
    }

    #[test]
    fn test_equals_assignment() {
        let code = "x = 42";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        assert!(artifacts.exported_interface.contains_key("x"));
    }

    #[test]
    fn test_super_assignment() {
        let code = "x <<- 42";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        assert!(artifacts.exported_interface.contains_key("x"));
    }

    #[test]
    fn test_multiple_definitions() {
        let code = "x <- 1\ny <- 2\nz <- function() {}";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 3);
        assert!(artifacts.exported_interface.contains_key("x"));
        assert!(artifacts.exported_interface.contains_key("y"));
        assert!(artifacts.exported_interface.contains_key("z"));
    }

    #[test]
    fn test_scope_at_position() {
        let code = "x <- 1\ny <- 2\nz <- 3";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // At line 0, only x should be in scope
        let scope = scope_at_position(&artifacts, 0, 10);
        assert!(scope.symbols.contains_key("x"));
        assert!(!scope.symbols.contains_key("y"));

        // At line 1, x and y should be in scope
        let scope = scope_at_position(&artifacts, 1, 10);
        assert!(scope.symbols.contains_key("x"));
        assert!(scope.symbols.contains_key("y"));
        assert!(!scope.symbols.contains_key("z"));

        // At line 2, all should be in scope
        let scope = scope_at_position(&artifacts, 2, 10);
        assert_eq!(scope.symbols.len(), 3);
    }

    #[test]
    fn test_interface_hash_deterministic() {
        let code = "x <- 1\ny <- 2";
        let tree = parse_r(code);
        let artifacts1 = compute_artifacts(&test_uri(), &tree, code);
        let artifacts2 = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts1.interface_hash, artifacts2.interface_hash);
    }

    #[test]
    fn test_interface_hash_changes() {
        let code1 = "x <- 1";
        let code2 = "x <- 1\ny <- 2";
        let tree1 = parse_r(code1);
        let tree2 = parse_r(code2);
        let artifacts1 = compute_artifacts(&test_uri(), &tree1, code1);
        let artifacts2 = compute_artifacts(&test_uri(), &tree2, code2);

        assert_ne!(artifacts1.interface_hash, artifacts2.interface_hash);
    }

    #[test]
    fn test_assign_call_string_literal() {
        let code = r#"assign("my_var", 42)"#;
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        let symbol = artifacts.exported_interface.get("my_var").unwrap();
        assert_eq!(symbol.kind, SymbolKind::Variable);
    }

    #[test]
    fn test_assign_call_dynamic_name_ignored() {
        let code = r#"assign(name_var, 42)"#;
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Dynamic name should not be treated as a definition
        assert_eq!(artifacts.exported_interface.len(), 0);
    }
}