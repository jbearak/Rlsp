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
    Parameter,
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
    /// A function definition that introduces parameter scope
    FunctionScope {
        start_line: u32,
        start_column: u32,
        end_line: u32,
        end_column: u32,
        parameters: Vec<ScopedSymbol>,
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
    /// Cached function scopes for O(1) lookup: (start_line, start_column, end_line, end_column)
    pub function_scopes: Vec<(u32, u32, u32, u32)>,
}

impl Default for ScopeArtifacts {
    fn default() -> Self {
        Self {
            exported_interface: HashMap::new(),
            timeline: Vec::new(),
            interface_hash: 0,
            function_scopes: Vec::new(),
        }
    }
}

/// Computed scope at a position
#[derive(Debug, Clone, Default)]
pub struct ScopeAtPosition {
    pub symbols: HashMap<String, ScopedSymbol>,
    pub chain: Vec<Url>,
    /// URIs where max depth was exceeded, with the source call position (line, col)
    pub depth_exceeded: Vec<(Url, u32, u32)>,
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
        // Skip sources that don't inherit symbols (local=TRUE or sys.source with non-global env)
        if source.inherits_symbols() {
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
        ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
    });

    // Populate function_scopes cache for O(1) lookup
    artifacts.function_scopes = artifacts.timeline.iter()
        .filter_map(|e| {
            if let ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, .. } = e {
                Some((*start_line, *start_column, *end_line, *end_column))
            } else {
                None
            }
        })
        .collect();

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

    // First pass: collect all function scopes that contain the query position
    let mut active_function_scopes = Vec::new();
    for event in &artifacts.timeline {
        if let ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, .. } = event {
            if (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                active_function_scopes.push((*start_line, *start_column, *end_line, *end_column));
            }
        }
    }

    // Second pass: process events and apply function scope filtering
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                // Include if definition is before or at the position
                if (*def_line, *def_col) <= (line, column) {
                    // Check if this definition is inside any function scope using cached lookup
                    // Use max_by_key to get the innermost (most recent start) containing scope
                    let def_function_scope = artifacts.function_scopes.iter()
                        .filter(|(start_line, start_column, end_line, end_column)| {
                            (*start_line, *start_column) <= (*def_line, *def_col) && (*def_line, *def_col) <= (*end_line, *end_column)
                        })
                        .max_by_key(|(start_line, start_column, _, _)| (*start_line, *start_column))
                        .copied();
                    
                    match def_function_scope {
                        None => {
                            // Global definition - always include
                            scope.symbols.insert(symbol.name.clone(), symbol.clone());
                        }
                        Some(def_scope) => {
                            // Function-local definition - only include if we're inside the same function
                            if active_function_scopes.contains(&def_scope) {
                                scope.symbols.insert(symbol.name.clone(), symbol.clone());
                            }
                        }
                    }
                }
            }
            ScopeEvent::Source { .. } => {
                // Source events are handled by scope_at_position_with_deps
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                if (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.insert(param.name.clone(), param.clone());
                    }
                }
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
    log::trace!("Resolving scope at {}:{}:{}", uri, line, column);
    let mut visited = HashSet::new();
    let scope = scope_at_position_recursive(uri, line, column, get_artifacts, resolve_path, max_depth, 0, &mut visited);
    log::trace!("Found {} symbols in scope", scope.symbols.len());
    scope
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
    log::trace!("Traversing to file: {} (depth {})", uri, current_depth);
    let mut scope = ScopeAtPosition::default();

    if current_depth >= max_depth || visited.contains(uri) {
        return scope;
    }
    visited.insert(uri.clone());
    scope.chain.push(uri.clone());

    let artifacts = match get_artifacts(uri) {
        Some(a) => a,
        None => {
            log::trace!("No artifacts found for {}", uri);
            return scope;
        }
    };

    // Process timeline events up to the requested position
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                if (*def_line, *def_col) <= (line, column) {
                    // Local definitions take precedence (don't overwrite)
                    scope.symbols.entry(symbol.name.clone()).or_insert_with(|| {
                        log::trace!("  Found symbol: {} ({})", symbol.name, match symbol.kind {
                            SymbolKind::Function => "function",
                            SymbolKind::Variable => "variable",
                            SymbolKind::Parameter => "parameter",
                        });
                        symbol.clone()
                    });
                }
            }
            ScopeEvent::Source { line: src_line, column: src_col, source } => {
                // Only include if source() call is before the position
                if (*src_line, *src_col) < (line, column) {
                    // Resolve the path and get symbols from sourced file
                    if let Some(child_uri) = resolve_path(&source.path, uri) {
                        // Check if we would exceed max depth
                        if current_depth + 1 >= max_depth {
                            scope.depth_exceeded.push((uri.clone(), *src_line, *src_col));
                            continue;
                        }

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
                        scope.depth_exceeded.extend(child_scope.depth_exceeded);
                    }
                }
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                if (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.entry(param.name.clone()).or_insert_with(|| param.clone());
                    }
                }
            }
        }
    }

    log::trace!("File {} contributed {} symbols", uri, scope.symbols.len());
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

    // Check for for loop iterators
    if node.kind() == "for_statement" {
        if let Some(symbol) = try_extract_for_loop_iterator(node, content, uri) {
            let event = ScopeEvent::Def {
                line: symbol.defined_line,
                column: symbol.defined_column,
                symbol: symbol.clone(),
            };
            artifacts.timeline.push(event);
            artifacts.exported_interface.insert(symbol.name.clone(), symbol);
        }
    }

    // Check for function definitions to extract parameter scope
    if node.kind() == "function_definition" {
        if let Some(function_scope) = try_extract_function_scope(node, content, uri) {
            artifacts.timeline.push(function_scope);
        }
    }

    // Recurse into children
    for child in node.children(&mut node.walk()) {
        collect_definitions(child, content, uri, artifacts);
    }
}

/// Extract function parameter scope from function_definition nodes.
/// Creates ScopedSymbol for each parameter and determines function body boundaries.
fn try_extract_function_scope(node: Node, content: &str, uri: &Url) -> Option<ScopeEvent> {
    // Get the parameters node
    let params_node = node.child_by_field_name("parameters")?;
    
    // Get the body node to determine boundaries
    let body_node = node.child_by_field_name("body")?;
    
    // Extract parameters
    let mut parameters = Vec::new();
    for child in params_node.children(&mut params_node.walk()) {
        if child.kind() == "parameter" {
            if let Some(param_symbol) = extract_parameter_symbol(child, content, uri) {
                parameters.push(param_symbol);
            }
        }
    }
    
    // Determine function body boundaries
    let body_start = body_node.start_position();
    let body_end = body_node.end_position();
    
    // Convert to UTF-16 columns
    let start_line_text = content.lines().nth(body_start.row).unwrap_or("");
    let end_line_text = content.lines().nth(body_end.row).unwrap_or("");
    let start_column = byte_offset_to_utf16_column(start_line_text, body_start.column);
    let end_column = byte_offset_to_utf16_column(end_line_text, body_end.column);
    
    Some(ScopeEvent::FunctionScope {
        start_line: body_start.row as u32,
        start_column,
        end_line: body_end.row as u32,
        end_column,
        parameters,
    })
}

/// Extract a parameter symbol from a parameter node
fn extract_parameter_symbol(param_node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // Handle different parameter types
    match param_node.kind() {
        "parameter" => {
            // Look for identifier child
            for child in param_node.children(&mut param_node.walk()) {
                if child.kind() == "identifier" {
                    let name = node_text(child, content).to_string();
                    let start = child.start_position();
                    let line_text = content.lines().nth(start.row).unwrap_or("");
                    let column = byte_offset_to_utf16_column(line_text, start.column);
                    
                    return Some(ScopedSymbol {
                        name,
                        kind: SymbolKind::Parameter,
                        source_uri: uri.clone(),
                        defined_line: start.row as u32,
                        defined_column: column,
                        signature: None,
                    });
                }
            }
        }
        "identifier" => {
            // Direct identifier (for ellipsis or simple params)
            let name = node_text(param_node, content).to_string();
            let start = param_node.start_position();
            let line_text = content.lines().nth(start.row).unwrap_or("");
            let column = byte_offset_to_utf16_column(line_text, start.column);
            
            return Some(ScopedSymbol {
                name,
                kind: SymbolKind::Parameter,
                source_uri: uri.clone(),
                defined_line: start.row as u32,
                defined_column: column,
                signature: None,
            });
        }
        "dots" => {
            // Handle ellipsis (...) parameter
            let start = param_node.start_position();
            let line_text = content.lines().nth(start.row).unwrap_or("");
            let column = byte_offset_to_utf16_column(line_text, start.column);
            
            return Some(ScopedSymbol {
                name: "...".to_string(),
                kind: SymbolKind::Parameter,
                source_uri: uri.clone(),
                defined_line: start.row as u32,
                defined_column: column,
                signature: None,
            });
        }
        _ => {}
    }
    
    None
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

/// Extract loop iterator from for_statement nodes.
/// In R, loop iterators persist after the loop completes.
fn try_extract_for_loop_iterator(node: Node, content: &str, uri: &Url) -> Option<ScopedSymbol> {
    // Get the variable field (iterator)
    let var_node = node.child_by_field_name("variable")?;
    
    // Only handle identifier nodes
    if var_node.kind() != "identifier" {
        return None;
    }
    
    let name = node_text(var_node, content).to_string();
    
    // Get position with UTF-16 column
    let start = var_node.start_position();
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
    
    // Handle -> operator: RHS is the name, LHS is the value
    if op_text == "->" {
        if rhs.kind() != "identifier" {
            return None;
        }
        let name = node_text(rhs, content).to_string();
        
        let (kind, signature) = if lhs.kind() == "function_definition" {
            let sig = extract_function_signature(lhs, &name, content);
            (SymbolKind::Function, Some(sig))
        } else {
            (SymbolKind::Variable, None)
        };
        
        // Position is at RHS (the identifier being defined)
        let start = rhs.start_position();
        let line_text = content.lines().nth(start.row).unwrap_or("");
        let column = byte_offset_to_utf16_column(line_text, start.column);
        
        return Some(ScopedSymbol {
            name,
            kind,
            source_uri: uri.clone(),
            defined_line: start.row as u32,
            defined_column: column,
            signature,
        });
    }
    
    // Handle <- = <<- operators: LHS is the name, RHS is the value
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

/// Extended scope resolution that also uses dependency graph edges.
/// This is the preferred entry point when a DependencyGraph is available.
pub fn scope_at_position_with_graph<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    graph: &super::dependency::DependencyGraph,
    workspace_root: Option<&Url>,
    max_depth: usize,
) -> ScopeAtPosition
where
    F: Fn(&Url) -> Option<ScopeArtifacts>,
    G: Fn(&Url) -> Option<super::types::CrossFileMetadata>,
{
    let mut visited = HashSet::new();
    
    // Build initial PathContext for the root file
    let meta = get_metadata(uri);
    let path_ctx = meta.as_ref()
        .and_then(|m| super::path_resolve::PathContext::from_metadata(uri, m, workspace_root))
        .or_else(|| super::path_resolve::PathContext::new(uri, workspace_root));
    
    scope_at_position_with_graph_recursive(
        uri, line, column, get_artifacts, get_metadata, graph, workspace_root,
        path_ctx, max_depth, 0, &mut visited,
    )
}

fn scope_at_position_with_graph_recursive<F, G>(
    uri: &Url,
    line: u32,
    column: u32,
    get_artifacts: &F,
    get_metadata: &G,
    graph: &super::dependency::DependencyGraph,
    workspace_root: Option<&Url>,
    path_ctx: Option<super::path_resolve::PathContext>,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<Url>,
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

    // STEP 1: Process parent context from dependency graph edges
    // Get edges where this file is the child (callee)
    for edge in graph.get_dependents(uri) {
        // Skip local=TRUE edges (symbols not inherited)
        if edge.local {
            continue;
        }
        // Skip sys.source with non-global env
        if edge.is_sys_source {
            // For sys.source, we need to check if it's global env
            // The edge doesn't store this directly, so we check metadata
            if let Some(meta) = get_metadata(&edge.from) {
                let is_global = meta.sources.iter().any(|s| {
                    s.is_sys_source && s.sys_source_global_env && 
                    s.line == edge.call_site_line.unwrap_or(u32::MAX)
                });
                if !is_global {
                    continue;
                }
            }
        }

        // Get call site position for filtering
        let call_site_line = edge.call_site_line.unwrap_or(u32::MAX);
        let call_site_col = edge.call_site_column.unwrap_or(u32::MAX);

        // Check if we would exceed max depth
        if current_depth + 1 >= max_depth {
            scope.depth_exceeded.push((uri.clone(), call_site_line, call_site_col));
            continue;
        }

        // Build PathContext for parent
        let parent_meta = get_metadata(&edge.from);
        let parent_ctx = parent_meta.as_ref()
            .and_then(|m| super::path_resolve::PathContext::from_metadata(&edge.from, m, workspace_root))
            .or_else(|| super::path_resolve::PathContext::new(&edge.from, workspace_root));

        // Get parent's scope at the call site
        let parent_scope = scope_at_position_with_graph_recursive(
            &edge.from,
            call_site_line,
            call_site_col,
            get_artifacts,
            get_metadata,
            graph,
            workspace_root,
            parent_ctx,
            max_depth,
            current_depth + 1,
            visited,
        );

        // Merge parent symbols (they are available at the START of this file)
        for (name, symbol) in parent_scope.symbols {
            scope.symbols.entry(name).or_insert(symbol);
        }
        scope.chain.extend(parent_scope.chain);
        scope.depth_exceeded.extend(parent_scope.depth_exceeded);
    }

    // STEP 2: Process timeline events (local definitions and forward sources)
    // First pass: collect all function scopes that contain the query position
    let mut active_function_scopes = Vec::new();
    for event in &artifacts.timeline {
        if let ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, .. } = event {
            if (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                active_function_scopes.push((*start_line, *start_column, *end_line, *end_column));
            }
        }
    }
    
    // Second pass: process events and apply function scope filtering
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                if (*def_line, *def_col) <= (line, column) {
                    // Check if this definition is inside any function scope using cached lookup
                    // Use max_by_key to get the innermost (most recent start) containing scope
                    let def_function_scope = artifacts.function_scopes.iter()
                        .filter(|(start_line, start_column, end_line, end_column)| {
                            (*start_line, *start_column) <= (*def_line, *def_col) && (*def_line, *def_col) <= (*end_line, *end_column)
                        })
                        .max_by_key(|(start_line, start_column, _, _)| (*start_line, *start_column))
                        .copied();
                    
                    match def_function_scope {
                        None => {
                            // Global definition - always include (local definitions take precedence over inherited symbols)
                            scope.symbols.insert(symbol.name.clone(), symbol.clone());
                        }
                        Some(def_scope) => {
                            // Function-local definition - only include if we're inside the same function
                            if active_function_scopes.contains(&def_scope) {
                                scope.symbols.insert(symbol.name.clone(), symbol.clone());
                            }
                        }
                    }
                }
            }
            ScopeEvent::Source { line: src_line, column: src_col, source } => {
                // Only include if source() call is before the position
                if (*src_line, *src_col) < (line, column) {
                    // Resolve the path using PathContext
                    let child_uri = path_ctx.as_ref().and_then(|ctx| {
                        let resolved = super::path_resolve::resolve_path(&source.path, ctx)?;
                        super::path_resolve::path_to_uri(&resolved)
                    });
                    
                    if let Some(child_uri) = child_uri {
                        // Check if we would exceed max depth
                        if current_depth + 1 >= max_depth {
                            scope.depth_exceeded.push((uri.clone(), *src_line, *src_col));
                            continue;
                        }

                        // Build child PathContext, respecting chdir flag
                        let child_path = child_uri.to_file_path().ok();
                        let child_ctx = child_path.as_ref().and_then(|cp| {
                            let ctx = path_ctx.as_ref()?;
                            // Get child's metadata for its own working directory directive
                            let child_meta = get_metadata(&child_uri);
                            if let Some(cm) = child_meta {
                                // Child has its own metadata - use it, but inherit working dir if no explicit one
                                let mut child_ctx = super::path_resolve::PathContext::from_metadata(&child_uri, &cm, workspace_root)?;
                                if child_ctx.working_directory.is_none() {
                                    // Inherit from parent based on chdir flag
                                    child_ctx.inherited_working_directory = if source.chdir {
                                        Some(cp.parent()?.to_path_buf())
                                    } else {
                                        Some(ctx.effective_working_directory())
                                    };
                                }
                                Some(child_ctx)
                            } else {
                                // No metadata for child - create context based on chdir
                                Some(ctx.child_context_for_source(cp, source.chdir))
                            }
                        });

                        let child_scope = scope_at_position_with_graph_recursive(
                            &child_uri,
                            u32::MAX, // Include all symbols from sourced file
                            u32::MAX,
                            get_artifacts,
                            get_metadata,
                            graph,
                            workspace_root,
                            child_ctx,
                            max_depth,
                            current_depth + 1,
                            visited,
                        );
                        // Merge child symbols (local definitions take precedence)
                        for (name, symbol) in child_scope.symbols {
                            scope.symbols.entry(name).or_insert(symbol);
                        }
                        scope.chain.extend(child_scope.chain);
                        scope.depth_exceeded.extend(child_scope.depth_exceeded);
                    }
                }
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                if (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.insert(param.name.clone(), param.clone());
                    }
                }
            }
        }
    }

    scope
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
                    super::types::CallSiteSpec::Match(_) => {
                        // Match resolution requires content provider - not available in this path
                        // Fall back to end of file (conservative)
                        Some((u32::MAX, u32::MAX))
                    }
                    super::types::CallSiteSpec::Default => Some((u32::MAX, u32::MAX)), // end of file
                };

                if let Some((call_line, call_col)) = call_site {
                    // Check if we would exceed max depth
                    if current_depth + 1 >= max_depth {
                        scope.depth_exceeded.push((uri.clone(), directive.directive_line, 0));
                        continue;
                    }

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
                    scope.depth_exceeded.extend(parent_scope.depth_exceeded);
                }
            }
        }
    }

    // STEP 2: Process timeline events (local definitions and forward sources)
    // First pass: collect all function scopes that contain the query position
    let mut active_function_scopes = Vec::new();
    for event in &artifacts.timeline {
        if let ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, .. } = event {
            if (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                active_function_scopes.push((*start_line, *start_column, *end_line, *end_column));
            }
        }
    }
    
    // Second pass: process events and apply function scope filtering
    for event in &artifacts.timeline {
        match event {
            ScopeEvent::Def { line: def_line, column: def_col, symbol } => {
                if (*def_line, *def_col) <= (line, column) {
                    // Check if this definition is inside any function scope
                    let def_function_scope = artifacts.timeline.iter()
                        .filter_map(|e| {
                            if let ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, .. } = e {
                                if (*start_line, *start_column) <= (*def_line, *def_col) && (*def_line, *def_col) <= (*end_line, *end_column) {
                                    Some((*start_line, *start_column, *end_line, *end_column))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        })
                        .next();
                    
                    match def_function_scope {
                        None => {
                            // Global definition - local definitions take precedence over inherited symbols
                            scope.symbols.insert(symbol.name.clone(), symbol.clone());
                        }
                        Some(def_scope) => {
                            // Function-local definition - only include if we're inside the same function
                            if active_function_scopes.contains(&def_scope) {
                                scope.symbols.insert(symbol.name.clone(), symbol.clone());
                            }
                        }
                    }
                }
            }
            ScopeEvent::Source { line: src_line, column: src_col, source } => {
                // Only include if source() call is before the position
                if (*src_line, *src_col) < (line, column) {
                    // Resolve the path and get symbols from sourced file
                    if let Some(child_uri) = resolve_path(&source.path, uri) {
                        // Check if we would exceed max depth
                        if current_depth + 1 >= max_depth {
                            scope.depth_exceeded.push((uri.clone(), *src_line, *src_col));
                            continue;
                        }

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
                        scope.depth_exceeded.extend(child_scope.depth_exceeded);
                    }
                }
            }
            ScopeEvent::FunctionScope { start_line, start_column, end_line, end_column, parameters } => {
                // Include function parameters if position is within function body
                if (*start_line, *start_column) <= (line, column) && (line, column) <= (*end_line, *end_column) {
                    for param in parameters {
                        scope.symbols.insert(param.name.clone(), param.clone());
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

    #[test]
    fn test_for_loop_iterator_extraction() {
        let code = "for (i in 1:10) { print(i) }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        let symbol = artifacts.exported_interface.get("i").unwrap();
        assert_eq!(symbol.kind, SymbolKind::Variable);
        assert_eq!(symbol.name, "i");
        assert!(symbol.signature.is_none());
    }

    #[test]
    fn test_for_loop_iterator_with_complex_sequence() {
        let code = "for (item in my_list) { process(item) }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        assert_eq!(artifacts.exported_interface.len(), 1);
        assert!(artifacts.exported_interface.contains_key("item"));
    }

    #[test]
    fn test_for_loop_iterator_persists_after_loop() {
        let code = "for (j in 1:5) { }\nresult <- j";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Both j (iterator) and result should be in scope
        assert_eq!(artifacts.exported_interface.len(), 2);
        assert!(artifacts.exported_interface.contains_key("j"));
        assert!(artifacts.exported_interface.contains_key("result"));
    }

    #[test]
    fn test_nested_for_loops() {
        let code = "for (i in 1:3) { for (j in 1:2) { print(i, j) } }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Both iterators should be in scope
        assert_eq!(artifacts.exported_interface.len(), 2);
        assert!(artifacts.exported_interface.contains_key("i"));
        assert!(artifacts.exported_interface.contains_key("j"));
    }

    #[test]
    fn test_backward_directive_call_site_filtering() {
        use crate::cross_file::types::{BackwardDirective, CallSiteSpec, CrossFileMetadata};

        let parent_uri = Url::parse("file:///parent.R").unwrap();
        let child_uri = Url::parse("file:///child.R").unwrap();

        // Parent code: a on line 0, x1 on line 1, x2 on line 2, y on line 3
        let parent_code = "a <- 1\nx1 <- 1\nx2 <- 2\ny <- 2";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Verify parent artifacts
        println!("Parent timeline:");
        for event in &parent_artifacts.timeline {
            match event {
                ScopeEvent::Def { line, column, symbol } => {
                    println!("  Def: {} at ({}, {})", symbol.name, line, column);
                }
                _ => {}
            }
        }

        // Child with backward directive line=2 (1-based, so 0-based line 1)
        let child_code = "z <- 3";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Line(1), // 0-based line 1
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "../parent.R" { Some(parent_uri.clone()) } else { None }
        };

        // Test scope at end of child file (line 0, after z definition)
        let scope = scope_at_position_with_backward(
            &child_uri, 0, 10, &get_artifacts, &get_metadata, &resolve_path, 10, None
        );

        // Should have: a (from parent line 0), x1 (from parent line 1), z (local)
        // Should NOT have: x2 (parent line 2), y (parent line 3) - after call site
        assert!(scope.symbols.contains_key("a"), "Should have 'a' from parent");
        assert!(scope.symbols.contains_key("x1"), "Should have 'x1' from parent");
        assert!(scope.symbols.contains_key("z"), "Should have 'z' from local");
        assert!(!scope.symbols.contains_key("x2"), "Should NOT have 'x2' - after call site");
        assert!(!scope.symbols.contains_key("y"), "Should NOT have 'y' - after call site");
    }

    #[test]
    fn test_source_local_false_global_scope() {
        // Test that source() with local=FALSE makes symbols available (inherits_symbols() returns true)
        let source = ForwardSource {
            path: "child.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            local: false,  // local=FALSE
            chdir: false,
            is_sys_source: false,
            sys_source_global_env: false,
        };

        assert!(source.inherits_symbols(), "source() with local=FALSE should inherit symbols");

        // Test that such sources are included in timeline
        let code = "x <- 1\nsource(\"child.R\", local = FALSE)\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have source event in timeline
        let source_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Source { source, .. } => Some(source),
                _ => None,
            })
            .collect();

        assert_eq!(source_events.len(), 1, "Should have one source event");
        assert!(!source_events[0].local, "Source should have local=FALSE");
        assert!(source_events[0].inherits_symbols(), "Source should inherit symbols");
    }

    #[test]
    fn test_source_local_true_not_inherited() {
        // Test that source() with local=TRUE does NOT make symbols available (inherits_symbols() returns false)
        let source = ForwardSource {
            path: "child.R".to_string(),
            line: 0,
            column: 0,
            is_directive: false,
            local: true,   // local=TRUE
            chdir: false,
            is_sys_source: false,
            sys_source_global_env: false,
        };

        assert!(!source.inherits_symbols(), "source() with local=TRUE should NOT inherit symbols");

        // Test that such sources are NOT included in timeline
        let code = "x <- 1\nsource(\"child.R\", local = TRUE)\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should NOT have source event in timeline (filtered out by compute_artifacts)
        let source_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Source { .. } => Some(e),
                _ => None,
            })
            .collect();

        assert_eq!(source_events.len(), 0, "Should have no source events (local=TRUE filtered out)");
    }

    #[test]
    fn test_source_default_local_false() {
        // Test that source() without local parameter defaults to local=FALSE behavior
        let code = "x <- 1\nsource(\"child.R\")\ny <- 2";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have source event in timeline (defaults to local=FALSE)
        let source_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| match e {
                ScopeEvent::Source { source, .. } => Some(source),
                _ => None,
            })
            .collect();

        assert_eq!(source_events.len(), 1, "Should have one source event");
        assert!(!source_events[0].local, "Source should default to local=FALSE");
        assert!(source_events[0].inherits_symbols(), "Source should inherit symbols by default");
    }

    #[test]
    fn test_scope_at_position_with_graph() {
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: defines 'a' then sources child
        let parent_code = "a <- 1\nsource(\"child.R\")";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines 'b'
        let child_code = "b <- 2";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // At end of parent file, both 'a' and 'b' should be available
        let scope = scope_at_position_with_graph(
            &parent_uri, 10, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        assert!(scope.symbols.contains_key("a"), "a should be available");
        assert!(scope.symbols.contains_key("b"), "b should be available from sourced file");
    }

    #[test]
    fn test_scope_with_graph_parent_context() {
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent code: defines 'parent_var' then sources child at line 1
        let parent_code = "parent_var <- 1\nsource(\"child.R\")";
        let parent_tree = parse_r(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: defines 'child_var'
        let child_code = "child_var <- 2";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "child.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // In child file, parent_var should be available via dependency graph edge
        let scope = scope_at_position_with_graph(
            &child_uri, 10, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        assert!(scope.symbols.contains_key("parent_var"), "parent_var should be available from parent");
        assert!(scope.symbols.contains_key("child_var"), "child_var should be available locally");
    }

    #[test]
    fn test_max_depth_exceeded_forward() {
        // Test that depth_exceeded is populated when max depth is hit on forward sources
        let uri_a = Url::parse("file:///project/a.R").unwrap();
        let uri_b = Url::parse("file:///project/b.R").unwrap();
        let uri_c = Url::parse("file:///project/c.R").unwrap();

        // a.R sources b.R, b.R sources c.R
        let code_a = "source(\"b.R\")";
        let code_b = "source(\"c.R\")";
        let code_c = "x <- 1";

        let tree_a = parse_r(code_a);
        let tree_b = parse_r(code_b);
        let tree_c = parse_r(code_c);

        let artifacts_a = compute_artifacts(&uri_a, &tree_a, code_a);
        let artifacts_b = compute_artifacts(&uri_b, &tree_b, code_b);
        let artifacts_c = compute_artifacts(&uri_c, &tree_c, code_c);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &uri_a { Some(artifacts_a.clone()) }
            else if uri == &uri_b { Some(artifacts_b.clone()) }
            else if uri == &uri_c { Some(artifacts_c.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, from: &Url| -> Option<Url> {
            if from == &uri_a && path == "b.R" { Some(uri_b.clone()) }
            else if from == &uri_b && path == "c.R" { Some(uri_c.clone()) }
            else { None }
        };

        // With max_depth=2, traversing a->b->c should exceed at b->c
        let scope = scope_at_position_with_deps(&uri_a, u32::MAX, u32::MAX, &get_artifacts, &resolve_path, 2);

        // Should have depth_exceeded entry for b.R at the source("c.R") call
        assert!(!scope.depth_exceeded.is_empty(), "depth_exceeded should not be empty");
        assert!(scope.depth_exceeded.iter().any(|(uri, _, _)| uri == &uri_b), 
            "depth_exceeded should contain b.R");
    }

    #[test]
    fn test_max_depth_exceeded_backward() {
        // Test that depth_exceeded is populated when max depth is hit on backward directives
        use super::super::dependency::DependencyGraph;
        use super::super::types::{CrossFileMetadata, ForwardSource};

        let uri_a = Url::parse("file:///project/a.R").unwrap();
        let uri_b = Url::parse("file:///project/b.R").unwrap();
        let uri_c = Url::parse("file:///project/c.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // c.R is sourced by b.R, b.R is sourced by a.R
        let code_a = "a_var <- 1\nsource(\"b.R\")";
        let code_b = "b_var <- 2\nsource(\"c.R\")";
        let code_c = "c_var <- 3";

        let tree_a = parse_r(code_a);
        let tree_b = parse_r(code_b);
        let tree_c = parse_r(code_c);

        let artifacts_a = compute_artifacts(&uri_a, &tree_a, code_a);
        let artifacts_b = compute_artifacts(&uri_b, &tree_b, code_b);
        let artifacts_c = compute_artifacts(&uri_c, &tree_c, code_c);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let meta_a = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "b.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        let meta_b = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "c.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };

        graph.update_file(&uri_a, &meta_a, Some(&workspace_root), |_| None);
        graph.update_file(&uri_b, &meta_b, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &uri_a { Some(artifacts_a.clone()) }
            else if uri == &uri_b { Some(artifacts_b.clone()) }
            else if uri == &uri_c { Some(artifacts_c.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &uri_a { Some(meta_a.clone()) }
            else if uri == &uri_b { Some(meta_b.clone()) }
            else { None }
        };

        // With max_depth=2, traversing c->b->a should exceed
        let scope = scope_at_position_with_graph(
            &uri_c, u32::MAX, u32::MAX, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 2,
        );

        // Should have depth_exceeded entry
        assert!(!scope.depth_exceeded.is_empty(), "depth_exceeded should not be empty with max_depth=2");
    }

    #[test]
    fn test_lsp_source_directive_in_scope() {
        // Test that @lsp-source directives are treated as source call sites for scope resolution
        use super::super::types::ForwardSource;

        let parent_uri = Url::parse("file:///project/parent.R").unwrap();
        let child_uri = Url::parse("file:///project/child.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // Parent file: has @lsp-source directive on line 2 (0-based: line 1)
        // The directive is parsed into sources with is_directive=true
        let parent_code = "x <- 1\n# @lsp-source child.R\ny <- 2";
        let parent_tree = parse_r(parent_code);
        let mut parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);
        
        // Manually add the directive source (normally done by directive parsing)
        parent_artifacts.timeline.push(ScopeEvent::Source {
            line: 1,
            column: 0,
            source: ForwardSource {
                path: "child.R".to_string(),
                line: 1,
                column: 0,
                is_directive: true,  // This is the key - it's a directive
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            },
        });
        parent_artifacts.timeline.sort_by_key(|e| match e {
            ScopeEvent::Def { line, column, .. } => (*line, *column),
            ScopeEvent::Source { line, column, .. } => (*line, *column),
            ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
        });

        // Child file: defines 'child_var'
        let child_code = "child_var <- 42";
        let child_tree = parse_r(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Before the @lsp-source directive (line 0), child_var should NOT be in scope
        let scope_before = scope_at_position_with_deps(&parent_uri, 0, 10, &get_artifacts, &resolve_path, 10);
        assert!(!scope_before.symbols.contains_key("child_var"), 
            "child_var should NOT be in scope before @lsp-source directive");

        // After the @lsp-source directive (line 2), child_var SHOULD be in scope
        let scope_after = scope_at_position_with_deps(&parent_uri, 2, 0, &get_artifacts, &resolve_path, 10);
        assert!(scope_after.symbols.contains_key("child_var"), 
            "child_var SHOULD be in scope after @lsp-source directive");
    }

    #[test]
    fn test_chdir_affects_nested_path_resolution() {
        // Test that chdir=TRUE causes child's relative paths to resolve from child's directory
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        // Directory structure:
        // /project/main.R - sources data/loader.R with chdir=TRUE
        // /project/data/loader.R - sources helpers.R (relative to data/)
        // /project/data/helpers.R - defines helper_func
        let main_uri = Url::parse("file:///project/main.R").unwrap();
        let loader_uri = Url::parse("file:///project/data/loader.R").unwrap();
        let helpers_uri = Url::parse("file:///project/data/helpers.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // main.R: sources data/loader.R with chdir=TRUE
        let main_code = "x <- 1\nsource(\"data/loader.R\", chdir = TRUE)";
        let main_tree = parse_r(main_code);
        let main_artifacts = compute_artifacts(&main_uri, &main_tree, main_code);

        // loader.R: sources helpers.R (relative path)
        let loader_code = "source(\"helpers.R\")\nloader_var <- 1";
        let loader_tree = parse_r(loader_code);
        let loader_artifacts = compute_artifacts(&loader_uri, &loader_tree, loader_code);

        // helpers.R: defines helper_func
        let helpers_code = "helper_func <- function() {}";
        let helpers_tree = parse_r(helpers_code);
        let helpers_artifacts = compute_artifacts(&helpers_uri, &helpers_tree, helpers_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let main_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "data/loader.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: true, // chdir=TRUE
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };
        let loader_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "helpers.R".to_string(),
                line: 0,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };

        graph.update_file(&main_uri, &main_meta, Some(&workspace_root), |_| None);
        graph.update_file(&loader_uri, &loader_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &main_uri { Some(main_artifacts.clone()) }
            else if uri == &loader_uri { Some(loader_artifacts.clone()) }
            else if uri == &helpers_uri { Some(helpers_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &main_uri { Some(main_meta.clone()) }
            else if uri == &loader_uri { Some(loader_meta.clone()) }
            else { None }
        };

        // At end of main.R, helper_func should be available because:
        // 1. main.R sources data/loader.R with chdir=TRUE
        // 2. loader.R's working directory becomes /project/data/
        // 3. loader.R sources "helpers.R" which resolves to /project/data/helpers.R
        let scope = scope_at_position_with_graph(
            &main_uri, u32::MAX, u32::MAX, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        assert!(scope.symbols.contains_key("x"), "x should be available");
        assert!(scope.symbols.contains_key("loader_var"), "loader_var should be available from loader.R");
        assert!(scope.symbols.contains_key("helper_func"), "helper_func should be available from helpers.R via chdir");
    }

    #[test]
    fn test_working_directory_directive_affects_path_resolution() {
        // Test that @lsp-working-directory affects path resolution
        use crate::cross_file::dependency::DependencyGraph;
        use crate::cross_file::types::{CrossFileMetadata, ForwardSource};

        // Directory structure:
        // /project/scripts/main.R - has @lsp-working-directory /data, sources helpers.R
        // /project/data/helpers.R - defines helper_func
        let main_uri = Url::parse("file:///project/scripts/main.R").unwrap();
        let helpers_uri = Url::parse("file:///project/data/helpers.R").unwrap();
        let workspace_root = Url::parse("file:///project").unwrap();

        // main.R: has working directory directive, sources helpers.R
        let main_code = "# @lsp-working-directory /data\nsource(\"helpers.R\")";
        let main_tree = parse_r(main_code);
        let main_artifacts = compute_artifacts(&main_uri, &main_tree, main_code);

        // helpers.R: defines helper_func
        let helpers_code = "helper_func <- function() {}";
        let helpers_tree = parse_r(helpers_code);
        let helpers_artifacts = compute_artifacts(&helpers_uri, &helpers_tree, helpers_code);

        // Build dependency graph with working directory
        let mut graph = DependencyGraph::new();
        let main_meta = CrossFileMetadata {
            working_directory: Some("/data".to_string()), // workspace-root-relative
            sources: vec![ForwardSource {
                path: "helpers.R".to_string(),
                line: 1,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            }],
            ..Default::default()
        };

        graph.update_file(&main_uri, &main_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &main_uri { Some(main_artifacts.clone()) }
            else if uri == &helpers_uri { Some(helpers_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &main_uri { Some(main_meta.clone()) }
            else { None }
        };

        // At end of main.R, helper_func should be available because:
        // 1. main.R has @lsp-working-directory /data
        // 2. source("helpers.R") resolves to /project/data/helpers.R
        let scope = scope_at_position_with_graph(
            &main_uri, u32::MAX, u32::MAX, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        assert!(scope.symbols.contains_key("helper_func"), "helper_func should be available via working directory directive");
    }

    #[test]
    fn test_function_parameters_available_inside_function() {
        let code = "my_func <- function(x, y) { x + y }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Inside function body, parameters should be available
        let scope_inside = scope_at_position(&artifacts, 0, 30); // Position within function body
        assert!(scope_inside.symbols.contains_key("x"), "Parameter x should be available inside function");
        assert!(scope_inside.symbols.contains_key("y"), "Parameter y should be available inside function");
        assert!(scope_inside.symbols.contains_key("my_func"), "Function name should be available inside function");
    }

    #[test]
    fn test_function_parameters_not_available_outside_function() {
        let code = "my_func <- function(x, y) { x + y }\nresult <- 42";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Outside function, parameters should NOT be available
        let scope_outside = scope_at_position(&artifacts, 1, 10); // Position on second line
        assert!(scope_outside.symbols.contains_key("my_func"), "Function name should be available outside function");
        assert!(scope_outside.symbols.contains_key("result"), "Global variable should be available outside function");
        assert!(!scope_outside.symbols.contains_key("x"), "Parameter x should NOT be available outside function");
        assert!(!scope_outside.symbols.contains_key("y"), "Parameter y should NOT be available outside function");
    }

    #[test]
    fn test_function_local_variables_not_available_outside() {
        let code = "my_func <- function() { local_var <- 42; local_var }\nglobal_var <- 100";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Outside function, local variable should NOT be available
        let scope_outside = scope_at_position(&artifacts, 1, 10);
        assert!(scope_outside.symbols.contains_key("my_func"), "Function name should be available outside function");
        assert!(scope_outside.symbols.contains_key("global_var"), "Global variable should be available outside function");
        assert!(!scope_outside.symbols.contains_key("local_var"), "Function-local variable should NOT be available outside function");

        // Inside function, local variable SHOULD be available
        let scope_inside = scope_at_position(&artifacts, 0, 40);
        assert!(scope_inside.symbols.contains_key("my_func"), "Function name should be available inside function");
        assert!(scope_inside.symbols.contains_key("local_var"), "Function-local variable should be available inside function");
        assert!(!scope_inside.symbols.contains_key("global_var"), "Global variable defined after function should NOT be available inside function");
    }

    #[test]
    fn test_nested_functions_separate_scopes() {
        let code = "outer <- function() { outer_var <- 1; inner <- function() { inner_var <- 2 } }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Outside all functions
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        assert!(scope_outside.symbols.contains_key("outer"), "Outer function should be available outside");
        assert!(!scope_outside.symbols.contains_key("inner"), "Inner function should NOT be available outside outer function");
        assert!(!scope_outside.symbols.contains_key("outer_var"), "Outer function variable should NOT be available outside");
        assert!(!scope_outside.symbols.contains_key("inner_var"), "Inner function variable should NOT be available outside");

        // Inside outer function but outside inner function
        let scope_outer = scope_at_position(&artifacts, 0, 35);
        assert!(scope_outer.symbols.contains_key("outer"), "Outer function should be available inside itself");
        assert!(scope_outer.symbols.contains_key("outer_var"), "Outer function variable should be available inside outer function");
        assert!(scope_outer.symbols.contains_key("inner"), "Inner function should be available inside outer function");
        assert!(!scope_outer.symbols.contains_key("inner_var"), "Inner function variable should NOT be available outside inner function");

        // Inside inner function
        let scope_inner = scope_at_position(&artifacts, 0, 75);
        assert!(scope_inner.symbols.contains_key("outer"), "Outer function should be available inside inner function");
        assert!(scope_inner.symbols.contains_key("outer_var"), "Outer function variable should be available inside inner function");
        assert!(scope_inner.symbols.contains_key("inner"), "Inner function should be available inside itself");
        assert!(scope_inner.symbols.contains_key("inner_var"), "Inner function variable should be available inside inner function");
    }

    #[test]
    fn test_function_scope_boundaries_with_multiple_functions() {
        let code = "func1 <- function(a) { var1 <- a }\nfunc2 <- function(b) { var2 <- b }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Inside first function
        let scope_func1 = scope_at_position(&artifacts, 0, 25);
        assert!(scope_func1.symbols.contains_key("func1"), "Function 1 should be available inside itself");
        assert!(scope_func1.symbols.contains_key("a"), "Parameter a should be available inside function 1");
        assert!(scope_func1.symbols.contains_key("var1"), "Variable 1 should be available inside function 1");
        assert!(!scope_func1.symbols.contains_key("func2"), "Function 2 should NOT be available inside function 1 (defined later)");
        assert!(!scope_func1.symbols.contains_key("b"), "Parameter b should NOT be available inside function 1");
        assert!(!scope_func1.symbols.contains_key("var2"), "Variable 2 should NOT be available inside function 1");

        // Inside second function
        let scope_func2 = scope_at_position(&artifacts, 1, 25);
        assert!(scope_func2.symbols.contains_key("func1"), "Function 1 should be available inside function 2");
        assert!(scope_func2.symbols.contains_key("func2"), "Function 2 should be available inside itself");
        assert!(scope_func2.symbols.contains_key("b"), "Parameter b should be available inside function 2");
        assert!(scope_func2.symbols.contains_key("var2"), "Variable 2 should be available inside function 2");
        assert!(!scope_func2.symbols.contains_key("a"), "Parameter a should NOT be available inside function 2");
        assert!(!scope_func2.symbols.contains_key("var1"), "Variable 1 should NOT be available inside function 2");

        // Outside both functions
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        assert!(scope_outside.symbols.contains_key("func1"), "Function 1 should be available outside");
        assert!(scope_outside.symbols.contains_key("func2"), "Function 2 should be available outside");
        assert!(!scope_outside.symbols.contains_key("a"), "Parameter a should NOT be available outside");
        assert!(!scope_outside.symbols.contains_key("b"), "Parameter b should NOT be available outside");
        assert!(!scope_outside.symbols.contains_key("var1"), "Variable 1 should NOT be available outside");
        assert!(!scope_outside.symbols.contains_key("var2"), "Variable 2 should NOT be available outside");
    }

    #[test]
    fn test_function_with_default_parameter_values() {
        let code = "my_func <- function(x = 1, y = 2) { x * y }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have FunctionScope event with parameters
        let function_scope_event = artifacts.timeline.iter().find(|event| {
            matches!(event, ScopeEvent::FunctionScope { .. })
        });
        assert!(function_scope_event.is_some());

        if let Some(ScopeEvent::FunctionScope { parameters, .. }) = function_scope_event {
            assert_eq!(parameters.len(), 2);
            let param_names: Vec<&str> = parameters.iter().map(|p| p.name.as_str()).collect();
            assert!(param_names.contains(&"x"));
            assert!(param_names.contains(&"y"));
        }

        // Parameters should be available within function body
        let scope_in_body = scope_at_position(&artifacts, 0, 40);
        assert!(scope_in_body.symbols.contains_key("x"));
        assert!(scope_in_body.symbols.contains_key("y"));
    }

    #[test]
    fn test_function_with_ellipsis_parameter() {
        let code = "my_func <- function(x, ...) { list(x, ...) }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have FunctionScope event with parameters including ellipsis
        let function_scope_event = artifacts.timeline.iter().find(|event| {
            matches!(event, ScopeEvent::FunctionScope { .. })
        });
        assert!(function_scope_event.is_some());

        if let Some(ScopeEvent::FunctionScope { parameters, .. }) = function_scope_event {
            assert_eq!(parameters.len(), 2);
            let param_names: Vec<&str> = parameters.iter().map(|p| p.name.as_str()).collect();
            assert!(param_names.contains(&"x"));
            assert!(param_names.contains(&"..."));
        }

        // Parameters should be available within function body
        let scope_in_body = scope_at_position(&artifacts, 0, 40);
        assert!(scope_in_body.symbols.contains_key("x"));
        assert!(scope_in_body.symbols.contains_key("..."));
    }

    #[test]
    fn test_function_with_no_parameters() {
        let code = "my_func <- function() { 42 }";
        let tree = parse_r(code);
        let artifacts = compute_artifacts(&test_uri(), &tree, code);

        // Should have FunctionScope event with empty parameters
        let function_scope_event = artifacts.timeline.iter().find(|event| {
            matches!(event, ScopeEvent::FunctionScope { .. })
        });
        assert!(function_scope_event.is_some());

        if let Some(ScopeEvent::FunctionScope { parameters, .. }) = function_scope_event {
            assert_eq!(parameters.len(), 0);
        }

        // Function name should still be available within body
        let scope_in_body = scope_at_position(&artifacts, 0, 25);
        assert!(scope_in_body.symbols.contains_key("my_func"));
    }
}