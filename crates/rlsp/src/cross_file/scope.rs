//
// cross_file/scope.rs
//
// Scope resolution for cross-file awareness
//

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use tower_lsp::lsp_types::Url;
use tree_sitter::{Node, Tree};

use super::types::byte_offset_to_utf16_column;

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

/// Compute scope artifacts for a file from its AST
pub fn compute_artifacts(uri: &Url, tree: &Tree, content: &str) -> ScopeArtifacts {
    let mut artifacts = ScopeArtifacts::default();
    let root = tree.root_node();

    // Collect definitions from AST
    collect_definitions(root, content, uri, &mut artifacts);

    // Compute interface hash
    artifacts.interface_hash = compute_interface_hash(&artifacts.exported_interface);

    artifacts
}

/// Compute scope at a specific position
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

    // Recurse into children
    for child in node.children(&mut node.walk()) {
        collect_definitions(child, content, uri, artifacts);
    }
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
}