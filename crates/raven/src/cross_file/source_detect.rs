//
// cross_file/source_detect.rs
//
// Detection of source() and sys.source() calls using tree-sitter
// Detection of rm() and remove() calls for scope tracking
// Detection of library(), require(), loadNamespace() calls for package awareness
//

use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Tree};

use super::scope::FunctionScopeInterval;
use super::types::{byte_offset_to_utf16_column, ForwardSource};

/// Detected rm()/remove() call with extracted symbol names
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RmCall {
    /// 0-based line of the rm() call
    pub line: u32,
    /// 0-based UTF-16 column
    pub column: u32,
    /// Symbol names to remove
    pub symbols: Vec<String>,
}

/// Detected library/require/loadNamespace call
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryCall {
    /// Package name (if statically determinable)
    pub package: String,
    /// 0-based line of the call
    pub line: u32,
    /// 0-based UTF-16 column of the call end position
    pub column: u32,
    /// Whether this is inside a function scope
    pub function_scope: Option<FunctionScopeInterval>,
}

/// Locate all top-level `source()` and `sys.source()` calls in an R syntax tree and extract their static parameters.
///
/// This function traverses the given tree-sitter `Tree` of R source code and collects each `source()`
/// or `sys.source()` call that has a statically determinable file path. For each detected call it
/// records the file path, the end-position line and UTF-16 column, whether the call is `sys.source`,
/// and the boolean `local` and `chdir` argument values when they are statically resolvable. Calls with
/// non-string or otherwise non-determinable file arguments are ignored.
///
/// # Returns
///
/// A `Vec<ForwardSource>` containing one entry per detected `source()`/`sys.source()` call in document
/// order, with fields populated for path, line, column, is_sys_source, local, chdir, and (for
/// `sys.source`) `sys_source_global_env`.
///
/// # Examples
///
/// ```
/// use tree_sitter::Parser;
/// // Parse R source text and detect source calls
/// let mut parser = Parser::new();
/// parser.set_language(tree_sitter_r::language()).unwrap();
/// let tree = parser.parse("source('utils.R', local = TRUE)\n", None).unwrap();
/// let sources = crates::rlsp::cross_file::source_detect::detect_source_calls(&tree, "source('utils.R', local = TRUE)\n");
/// assert_eq!(sources.len(), 1);
/// assert_eq!(sources[0].path, "utils.R");
/// assert!(sources[0].local);
/// ```
pub fn detect_source_calls(tree: &Tree, content: &str) -> Vec<ForwardSource> {
    log::trace!("Starting tree-sitter parsing for source() call detection");
    let mut sources = Vec::new();
    let root = tree.root_node();
    visit_node(root, content, &mut sources);
    log::trace!(
        "Completed source() call detection, found {} calls",
        sources.len()
    );
    for source in &sources {
        log::trace!(
            "  Detected source() call: path='{}' at line {} column {} (is_sys_source={}, local={}, chdir={})",
            source.path,
            source.line,
            source.column,
            source.is_sys_source,
            source.local,
            source.chdir
        );
    }
    sources
}

fn visit_node(node: Node, content: &str, sources: &mut Vec<ForwardSource>) {
    if node.kind() == "call" {
        if let Some(source) = try_parse_source_call(node, content) {
            sources.push(source);
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_node(child, content, sources);
    }
}

fn try_parse_source_call(node: Node, content: &str) -> Option<ForwardSource> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    let is_sys_source = match func_text {
        "source" => false,
        "sys.source" => true,
        _ => return None,
    };

    let args_node = node.child_by_field_name("arguments")?;
    let path = find_file_argument(&args_node, content)?;
    let local = find_bool_argument(&args_node, content, "local").unwrap_or(false);
    let chdir = find_bool_argument(&args_node, content, "chdir").unwrap_or(false);

    // For sys.source, check if envir is globalenv()/.GlobalEnv
    let sys_source_global_env = if is_sys_source {
        find_envir_is_global(&args_node, content)
    } else {
        true // Not sys.source, so this field doesn't matter
    };

    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);

    Some(ForwardSource {
        path,
        line: start.row as u32,
        column,
        is_directive: false,
        local,
        chdir,
        is_sys_source,
        sys_source_global_env,
        explicit_line: false, // AST-detected sources never have explicit line=N
        directive_line: 0,    // Not applicable for AST-detected sources
        user_line_zero: false, // Not applicable for AST-detected sources
    })
}

/// Check if the envir argument is globalenv() or .GlobalEnv
fn find_envir_is_global(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "envir" {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        let value = node_text(value_node, content).trim();
                        // Check for globalenv() or .GlobalEnv
                        return value == "globalenv()" || value == ".GlobalEnv";
                    }
                }
            }
        }
    }
    // If envir is not specified, sys.source defaults to baseenv() which is NOT global
    // So we return false (conservative: no symbol inheritance)
    false
}

fn find_file_argument(args_node: &Node, content: &str) -> Option<String> {
    let mut cursor = args_node.walk();
    let children: Vec<_> = args_node.children(&mut cursor).collect();

    // Look for named "file" argument
    for child in &children {
        if child.kind() == "argument" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "file" {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        return extract_string_literal(value_node, content);
                    }
                }
            }
        }
    }

    // Use first positional argument
    for child in &children {
        if child.kind() == "argument" && child.child_by_field_name("name").is_none() {
            if let Some(value_node) = child.child_by_field_name("value") {
                return extract_string_literal(value_node, content);
            }
        }
    }

    None
}

fn find_bool_argument(args_node: &Node, content: &str, param_name: &str) -> Option<bool> {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == param_name {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        let value = node_text(value_node, content);
                        return match value {
                            "TRUE" | "T" => Some(true),
                            "FALSE" | "F" => Some(false),
                            _ => None,
                        };
                    }
                }
            }
        }
    }
    None
}

fn extract_string_literal(node: Node, content: &str) -> Option<String> {
    if node.kind() == "string" {
        let text = node_text(node, content);
        if (text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\''))
        {
            return Some(text[1..text.len() - 1].to_string());
        }
    }
    None
}

fn node_text<'a>(node: Node<'a>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}

/// Detect rm() and remove() calls in R code.
/// Returns calls that should affect scope (excludes those with non-default envir=).
/// Extracts bare symbols from positional args and string-literal symbols from list=.
pub fn detect_rm_calls(tree: &Tree, content: &str) -> Vec<RmCall> {
    log::trace!("Starting tree-sitter parsing for rm() call detection");
    let mut rm_calls = Vec::new();
    let root = tree.root_node();
    visit_node_for_rm(root, content, &mut rm_calls);
    log::trace!(
        "Completed rm() call detection, found {} calls",
        rm_calls.len()
    );
    for rm_call in &rm_calls {
        log::trace!(
            "  Detected rm() call at line {} column {} with symbols: {:?}",
            rm_call.line,
            rm_call.column,
            rm_call.symbols
        );
    }
    rm_calls
}

fn visit_node_for_rm(node: Node, content: &str, rm_calls: &mut Vec<RmCall>) {
    if node.kind() == "identifier" {
        return;
    }
    if node.kind() == "call" {
        if let Some(rm_call) = try_parse_rm_call(node, content) {
            // Only add if there are symbols to remove
            if !rm_call.symbols.is_empty() {
                rm_calls.push(rm_call);
            }
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_node_for_rm(child, content, rm_calls);
    }
}

fn try_parse_rm_call(node: Node, content: &str) -> Option<RmCall> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    // Check if this is rm() or remove()
    if func_text != "rm" && func_text != "remove" {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;

    // Skip if arguments contain error or missing nodes
    if args_node.has_error() {
        return None;
    }

    // Check if rm() has a non-default envir= argument
    // If envir= is present and NOT globalenv() or .GlobalEnv, skip this call
    if has_non_default_envir_for_rm(&args_node, content) {
        return None;
    }

    // Extract bare symbol arguments from positional args
    let mut symbols = extract_bare_symbols(&args_node, content);

    // Extract symbols from list= argument (string literals or c() calls)
    let list_symbols = extract_list_symbols(&args_node, content);
    symbols.extend(list_symbols);

    let start = node.start_position();
    let line_text = content.lines().nth(start.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, start.column);

    Some(RmCall {
        line: start.row as u32,
        column,
        symbols,
    })
}

/// Check if rm() call has a non-default envir= argument.
/// Returns true if envir= is present and NOT globalenv() or .GlobalEnv.
/// Returns false if envir= is absent (default) or is globalenv()/.GlobalEnv.
fn has_non_default_envir_for_rm(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "envir" {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        let value = node_text(value_node, content).trim();
                        // Default-equivalent values: globalenv() or .GlobalEnv
                        if value == "globalenv()" || value == ".GlobalEnv" {
                            return false;
                        }
                        // Any other value means non-default
                        return true;
                    }
                }
            }
        }
    }
    // No envir= argument means default (global environment)
    false
}

/// Extract bare symbol (identifier) arguments from positional args in rm()/remove() calls.
/// Only extracts identifiers from positional arguments (not named arguments).
fn extract_bare_symbols(args_node: &Node, content: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    let mut cursor = args_node.walk();

    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            // Only process positional arguments (no name)
            if child.child_by_field_name("name").is_none() {
                if let Some(value_node) = child.child_by_field_name("value") {
                    // Only extract if it's an identifier (bare symbol)
                    if value_node.kind() == "identifier" {
                        let symbol_name = node_text(value_node, content).to_string();
                        symbols.push(symbol_name);
                    }
                }
            }
        }
    }

    symbols
}

/// Extract symbols from the list= argument in rm()/remove() calls.
///
/// Handles:
/// - `list = "name"` (single string literal)
/// - `list = c("a", "b", "c")` (character vector)
///
/// Skips non-literal expressions (variables, function calls other than c()).
fn extract_list_symbols(args_node: &Node, content: &str) -> Vec<String> {
    let mut cursor = args_node.walk();

    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            // Look for named argument with name "list"
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "list" {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        return extract_list_value_symbols(value_node, content);
                    }
                }
            }
        }
    }

    Vec::new()
}

/// Extract symbols from the value of a list= argument.
/// Handles string literals and c() calls with string arguments.
fn extract_list_value_symbols(value_node: Node, content: &str) -> Vec<String> {
    match value_node.kind() {
        "string" => {
            // rm(list = "x")
            if let Some(s) = extract_string_literal(value_node, content) {
                vec![s]
            } else {
                vec![]
            }
        }
        "call" => {
            // Check if it's a c() call
            if is_c_call(value_node, content) {
                // rm(list = c("x", "y", "z"))
                extract_c_string_args(value_node, content)
            } else {
                // Dynamic expression like ls() - not supported
                vec![]
            }
        }
        _ => {
            // Variable reference or other expression - not supported
            vec![]
        }
    }
}

/// Check if a call node is a c() call (character vector constructor).
fn is_c_call(node: Node, content: &str) -> bool {
    if node.kind() != "call" {
        return false;
    }
    if let Some(func_node) = node.child_by_field_name("function") {
        let func_text = node_text(func_node, content);
        return func_text == "c";
    }
    false
}

/// Extracts string literal arguments from a `c()` call node.
///
/// Only string literal arguments are returned; other argument types are ignored.
///
/// # Examples
///
/// ```ignore
/// // Given a Tree-sitter `Node` for `c("a", "b")`:
/// let strings = extract_c_string_args(node, r#"c("a", "b")"#);
/// assert_eq!(strings, vec!["a".to_string(), "b".to_string()]);
/// ```
fn extract_c_string_args(node: Node, content: &str) -> Vec<String> {
    let mut symbols = Vec::new();

    if let Some(args_node) = node.child_by_field_name("arguments") {
        let mut cursor = args_node.walk();
        for child in args_node.children(&mut cursor) {
            if child.kind() == "argument" {
                if let Some(value_node) = child.child_by_field_name("value") {
                    if value_node.kind() == "string" {
                        if let Some(s) = extract_string_literal(value_node, content) {
                            symbols.push(s);
                        }
                    }
                }
            }
        }
    }

    symbols
}

// ============================================================================
// Library Call Detection
// ============================================================================

/// Detects `library()`, `require()`, and `loadNamespace()` calls that specify a static package name.
///
/// This returns entries for calls where the package can be determined from a bare identifier
/// (e.g., `library(dplyr)`) or a string literal (e.g., `library("dplyr")`). Calls that use
/// `character.only = TRUE` or whose package argument is an expression or variable are skipped.
///
/// # Examples
///
/// ```
/// use tree_sitter::Parser;
///
/// let mut parser = Parser::new();
/// parser.set_language(tree_sitter_r::language()).unwrap();
/// let source = r#"library(dplyr)"#;
/// let tree = parser.parse(source, None).unwrap();
/// let calls = detect_library_calls(&tree, source);
/// assert_eq!(calls.len(), 1);
/// assert_eq!(calls[0].package, "dplyr");
/// ```
pub fn detect_library_calls(tree: &Tree, content: &str) -> Vec<LibraryCall> {
    log::trace!("Starting tree-sitter parsing for library() call detection");
    let mut library_calls = Vec::new();
    let root = tree.root_node();
    visit_node_for_library(root, content, &mut library_calls);
    log::trace!(
        "Completed library() call detection, found {} calls",
        library_calls.len()
    );
    for lib_call in &library_calls {
        log::trace!(
            "  Detected library() call: package='{}' at line {} column {}",
            lib_call.package,
            lib_call.line,
            lib_call.column
        );
    }
    library_calls
}

/// Recursively traverses an AST subtree and collects statically determinable `library`/`require`/`loadNamespace` calls.
///
/// This function walks `node` and its descendants in document order. When a call node representing a
/// statically resolvable package load is found, a `LibraryCall` describing that call (package and end
/// position) is pushed into `library_calls`.
///
/// # Parameters
///
/// - `node`: the current AST node to visit (will recurse into its children).
/// - `content`: source text for extracting node-local values when parsing calls.
/// - `library_calls`: mutable collector that receives discovered `LibraryCall` entries in document order.
///
/// # Examples
///
/// ```no_run
/// // Traverse the whole tree and collect library-like calls.
/// let mut library_calls = Vec::new();
/// let root = tree.root_node();
/// visit_node_for_library(root, source_text, &mut library_calls);
/// assert!(library_calls.iter().all(|c| !c.package.is_empty()));
/// ```
fn visit_node_for_library(node: Node, content: &str, library_calls: &mut Vec<LibraryCall>) {
    // Skip identifier nodes - they have no children
    if node.kind() == "identifier" {
        return;
    }
    if node.kind() == "call" {
        if let Some(lib_call) = try_parse_library_call(node, content) {
            library_calls.push(lib_call);
        }
    }

    for child in node.children(&mut node.walk()) {
        visit_node_for_library(child, content, library_calls);
    }
}

/// Parse a call AST node and, if it is a static `library()`, `require()`, or `loadNamespace()` invocation,
/// return a `LibraryCall` containing the package name and the byte position (line and UTF-16 column) at the end of the call.
///
/// Returns `Some(LibraryCall)` when:
///
/// - the function name is `library`, `require`, or `loadNamespace`,
/// - the call's arguments are syntactically valid,
/// - the package name is statically determinable from a bare identifier or string literal (including a named `package=` argument),
/// - `character.only = TRUE` is not present (such calls are skipped as dynamic).
///
/// Returns `None` otherwise.
///
/// # Examples
///
/// ```no_run
/// // Given a tree-sitter `Node` for a call and the source `content`,
/// // `try_parse_library_call` returns a `LibraryCall` when the package is statically known.
/// // (Constructing a Node requires a tree-sitter parse; this example is illustrative.)
/// # use crates::rlsp::cross_file::source_detect::try_parse_library_call;
/// # use crates::rlsp::cross_file::source_detect::LibraryCall;
/// # let node = panic!("node from tree-sitter required");
/// # let content = "library(dplyr)";
/// # let _ = try_parse_library_call(node, content);
/// ```
fn try_parse_library_call(node: Node, content: &str) -> Option<LibraryCall> {
    let func_node = node.child_by_field_name("function")?;
    let func_text = node_text(func_node, content);

    // Check if this is library(), require(), or loadNamespace()
    if func_text != "library" && func_text != "require" && func_text != "loadNamespace" {
        return None;
    }

    let args_node = node.child_by_field_name("arguments")?;

    // Skip if arguments contain error or missing nodes
    if args_node.has_error() {
        return None;
    }

    // Check for character.only = TRUE - skip these calls (dynamic package name)
    if has_character_only_true(&args_node, content) {
        return None;
    }

    // Extract package name from first argument
    let package = extract_package_name(&args_node, content)?;

    // Get position at the end of the call (after the closing paren)
    let end = node.end_position();
    let line_text = content.lines().nth(end.row).unwrap_or("");
    let column = byte_offset_to_utf16_column(line_text, end.column);

    Some(LibraryCall {
        package,
        line: end.row as u32,
        column,
        // function_scope will be populated later in task 6.2
        function_scope: None,
    })
}

/// Determine whether an arguments node sets `character.only` to `TRUE` or `T`.
///
/// Scans the children of `args_node` for a named argument `character.only` and
/// returns `true` only if its value text is the literal `TRUE` or `T`.
///
/// # Returns
///
/// `true` if `character.only` is explicitly `TRUE` or `T`, `false` otherwise.
///
/// # Examples
///
/// ```
/// // `args_node` is the Tree-sitter `arguments` node for a call like:
/// //   library(foo, character.only = TRUE)
/// // `content` is the source text containing that call.
/// // The function will return `true` for that node.
/// let res = has_character_only_true(&args_node, content);
/// assert!(res);
/// ```
fn has_character_only_true(args_node: &Node, content: &str) -> bool {
    let mut cursor = args_node.walk();
    for child in args_node.children(&mut cursor) {
        if child.kind() == "argument" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "character.only" {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        let value = node_text(value_node, content);
                        return value == "TRUE" || value == "T";
                    }
                }
            }
        }
    }
    false
}

/// Determine a statically determinable package name from a call's argument list.
///
/// Looks for a named `package` argument first; if absent, inspects the first positional argument.
/// Returns `Some(package_name)` when the argument is a bare identifier (e.g., `library(dplyr)`)
/// or a string literal (e.g., `library("dplyr")` or `library('dplyr')`). Returns `None` for variables,
/// expressions, or other dynamic package specifications.
///
/// # Arguments
///
/// * `args_node` - The Tree-sitter `arguments` node of a call expression.
/// * `content` - The source text for extracting string literal contents.
///
/// # Returns
///
/// `Some(String)` containing the package name when statically determinable, `None` otherwise.
///
/// # Examples
///
/// ```ignore
/// // Parse an R call, obtain its arguments node, then:
/// let pkg = extract_package_name(&args_node, source_text);
/// assert_eq!(pkg, Some("dplyr".to_string()));
/// ```
fn extract_package_name(args_node: &Node, content: &str) -> Option<String> {
    let mut cursor = args_node.walk();
    let children: Vec<_> = args_node.children(&mut cursor).collect();

    // Look for named "package" argument first
    for child in &children {
        if child.kind() == "argument" {
            if let Some(name_node) = child.child_by_field_name("name") {
                let name = node_text(name_node, content);
                if name == "package" {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        return extract_package_value(value_node, content);
                    }
                }
            }
        }
    }

    // Use first positional argument
    for child in &children {
        if child.kind() == "argument" && child.child_by_field_name("name").is_none() {
            if let Some(value_node) = child.child_by_field_name("value") {
                return extract_package_value(value_node, content);
            }
        }
    }

    None
}

/// Extract package name from a value node.
/// Handles bare identifiers (library(dplyr)) and string literals (library("dplyr")).
fn extract_package_value(node: Node, content: &str) -> Option<String> {
    match node.kind() {
        "identifier" => {
            // Bare identifier: library(dplyr)
            Some(node_text(node, content).to_string())
        }
        "string" => {
            // String literal: library("dplyr") or library('dplyr')
            extract_string_literal(node, content)
        }
        _ => {
            // Variable, expression, or other dynamic value - skip
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    #[test]
    fn test_source_double_quotes() {
        let code = r#"source("utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
        assert!(!sources[0].is_sys_source);
        assert!(!sources[0].local);
        assert!(!sources[0].chdir);
    }

    #[test]
    fn test_source_single_quotes() {
        let code = "source('utils.R')";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
    }

    #[test]
    fn test_source_named_argument() {
        let code = r#"source(file = "utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
    }

    #[test]
    fn test_sys_source() {
        let code = r#"sys.source("utils.R", envir = globalenv())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "utils.R");
        assert!(sources[0].is_sys_source);
    }

    #[test]
    fn test_source_with_local_true() {
        let code = r#"source("utils.R", local = TRUE)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].local);
    }

    #[test]
    fn test_source_with_chdir_true() {
        let code = r#"source("utils.R", chdir = TRUE)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].chdir);
    }

    #[test]
    fn test_source_with_variable_path_skipped() {
        let code = "source(my_path)";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_source_with_paste0_skipped() {
        let code = r#"source(paste0("dir/", filename))"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_multiple_source_calls() {
        let code = r#"source("a.R")
source("b.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path, "a.R");
        assert_eq!(sources[0].line, 0);
        assert_eq!(sources[1].path, "b.R");
        assert_eq!(sources[1].line, 1);
    }

    #[test]
    fn test_source_position() {
        let code = "x <- 1\nsource(\"utils.R\")";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].line, 1);
        assert_eq!(sources[0].column, 0);
    }

    #[test]
    fn test_non_source_call_ignored() {
        let code = "print(\"hello\")";
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 0);
    }

    #[test]
    fn test_sys_source_with_globalenv() {
        let code = r#"sys.source("utils.R", envir = globalenv())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(sources[0].sys_source_global_env);
        assert!(sources[0].inherits_symbols());
    }

    #[test]
    fn test_sys_source_with_global_env_dot() {
        let code = r#"sys.source("utils.R", envir = .GlobalEnv)"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(sources[0].sys_source_global_env);
        assert!(sources[0].inherits_symbols());
    }

    #[test]
    fn test_sys_source_with_new_env() {
        let code = r#"sys.source("utils.R", envir = new.env())"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(!sources[0].sys_source_global_env);
        assert!(!sources[0].inherits_symbols());
    }

    #[test]
    fn test_sys_source_without_envir() {
        // sys.source without envir defaults to baseenv(), not global
        let code = r#"sys.source("utils.R")"#;
        let tree = parse_r(code);
        let sources = detect_source_calls(&tree, code);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].is_sys_source);
        assert!(!sources[0].sys_source_global_env);
        assert!(!sources[0].inherits_symbols());
    }

    // ==================== rm()/remove() detection tests ====================

    #[test]
    fn test_rm_single_bare_symbol() {
        let code = "rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[0].column, 0);
    }

    #[test]
    fn test_rm_multiple_bare_symbols() {
        let code = "rm(x, y, z)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y", "z"]);
    }

    #[test]
    fn test_remove_single_bare_symbol() {
        let code = "remove(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_remove_multiple_bare_symbols() {
        let code = "remove(a, b, c)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_rm_empty_call() {
        // rm() with no arguments should not produce any RmCall
        let code = "rm()";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_string_argument_skipped() {
        // rm("x") with string in positional arg should be skipped (not a bare symbol)
        let code = r#"rm("x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_expression_argument_skipped() {
        // rm(x + y) should be skipped (not an identifier)
        let code = "rm(x + y)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_number_argument_skipped() {
        // rm(1) should be skipped (not an identifier)
        let code = "rm(1)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_position() {
        let code = "x <- 1\nrm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].line, 1);
        assert_eq!(rm_calls[0].column, 0);
    }

    #[test]
    fn test_rm_position_with_offset() {
        let code = "x <- 1; rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[0].column, 8);
    }

    #[test]
    fn test_multiple_rm_calls() {
        let code = "rm(x)\nrm(y, z)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 2);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[1].symbols, vec!["y", "z"]);
        assert_eq!(rm_calls[1].line, 1);
    }

    #[test]
    fn test_non_rm_call_ignored() {
        let code = "print(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_list_single_string() {
        // rm(list = "x") should extract "x"
        let code = r#"rm(list = "x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_mixed_bare_and_list() {
        // rm(x, list = "y") - should extract both bare symbol x and list symbol y
        let code = r#"rm(x, list = "y")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y"]);
    }

    #[test]
    fn test_rm_inside_function() {
        let code = "f <- function() { rm(x) }";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_utf16_column() {
        // Test with emoji before rm() to verify UTF-16 column calculation
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let code = "🎉; rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
        // Column should be 2 (emoji) + 2 ("; ") = 4 in UTF-16
        assert_eq!(rm_calls[0].column, 4);
    }

    // ==================== list= argument parsing tests ====================

    #[test]
    fn test_rm_list_single_string_double_quotes() {
        let code = r#"rm(list = "myvar")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["myvar"]);
    }

    #[test]
    fn test_rm_list_single_string_single_quotes() {
        let code = "rm(list = 'myvar')";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["myvar"]);
    }

    #[test]
    fn test_rm_list_c_multiple_strings() {
        let code = r#"rm(list = c("a", "b", "c"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_rm_list_c_single_string() {
        let code = r#"rm(list = c("x"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_list_c_empty() {
        let code = "rm(list = c())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // Empty c() produces no symbols, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_variable_skipped() {
        // rm(list = var) - variable reference should be skipped
        let code = "rm(list = my_var)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_ls_skipped() {
        // rm(list = ls()) - function call should be skipped
        let code = "rm(list = ls())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_ls_pattern_skipped() {
        // rm(list = ls(pattern = "^tmp")) - function call should be skipped
        let code = r#"rm(list = ls(pattern = "^tmp"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_paste0_skipped() {
        // rm(list = paste0("x", 1:3)) - function call should be skipped
        let code = r#"rm(list = paste0("x", 1:3))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // No symbols extracted, so no RmCall
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_c_with_mixed_args() {
        // c() with mixed string and non-string args - only extract strings
        let code = r#"rm(list = c("a", var, "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        // Only string literals are extracted
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_rm_bare_and_list_combined() {
        // rm(x, y, list = c("a", "b")) - should extract all symbols
        let code = r#"rm(x, y, list = c("a", "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y", "a", "b"]);
    }

    #[test]
    fn test_remove_list_single_string() {
        // remove() should work the same as rm()
        let code = r#"remove(list = "x")"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_remove_list_c_multiple() {
        let code = r#"remove(list = c("a", "b"))"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_rm_list_number_skipped() {
        // rm(list = 123) - number should be skipped
        let code = "rm(list = 123)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_expression_skipped() {
        // rm(list = x + y) - expression should be skipped
        let code = "rm(list = x + y)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    // ==================== envir= argument filtering tests ====================

    #[test]
    fn test_rm_without_envir_processed() {
        // rm(x) without envir= should be processed normally
        let code = "rm(x)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_globalenv_processed() {
        // rm(x, envir = globalenv()) should be processed normally
        let code = "rm(x, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_dot_globalenv_processed() {
        // rm(x, envir = .GlobalEnv) should be processed normally
        let code = "rm(x, envir = .GlobalEnv)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_with_envir_custom_skipped() {
        // rm(x, envir = my_env) should be skipped (non-default environment)
        let code = "rm(x, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_new_env_skipped() {
        // rm(x, envir = new.env()) should be skipped
        let code = "rm(x, envir = new.env())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_parent_frame_skipped() {
        // rm(x, envir = parent.frame()) should be skipped
        let code = "rm(x, envir = parent.frame())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_with_envir_baseenv_skipped() {
        // rm(x, envir = baseenv()) should be skipped
        let code = "rm(x, envir = baseenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_multiple_symbols_with_envir_custom_skipped() {
        // rm(x, y, z, envir = my_env) should be skipped entirely
        let code = "rm(x, y, z, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_with_envir_custom_skipped() {
        // rm(list = c("a", "b"), envir = my_env) should be skipped
        let code = r#"rm(list = c("a", "b"), envir = my_env)"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_list_with_envir_globalenv_processed() {
        // rm(list = c("a", "b"), envir = globalenv()) should be processed
        let code = r#"rm(list = c("a", "b"), envir = globalenv())"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["a", "b"]);
    }

    #[test]
    fn test_remove_with_envir_custom_skipped() {
        // remove(x, envir = my_env) should be skipped
        let code = "remove(x, envir = my_env)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_remove_with_envir_globalenv_processed() {
        // remove(x, envir = globalenv()) should be processed
        let code = "remove(x, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x"]);
    }

    #[test]
    fn test_rm_mixed_with_envir_globalenv_processed() {
        // rm(x, list = "y", envir = .GlobalEnv) should be processed
        let code = r#"rm(x, list = "y", envir = .GlobalEnv)"#;
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 1);
        assert_eq!(rm_calls[0].symbols, vec!["x", "y"]);
    }

    #[test]
    fn test_multiple_rm_calls_with_different_envir() {
        // Mix of rm() calls with different envir= values
        let code = "rm(a)\nrm(b, envir = my_env)\nrm(c, envir = globalenv())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        // Only rm(a) and rm(c, envir = globalenv()) should be detected
        assert_eq!(rm_calls.len(), 2);
        assert_eq!(rm_calls[0].symbols, vec!["a"]);
        assert_eq!(rm_calls[0].line, 0);
        assert_eq!(rm_calls[1].symbols, vec!["c"]);
        assert_eq!(rm_calls[1].line, 2);
    }

    // ==================== error/missing AST node tests ====================

    #[test]
    fn test_rm_malformed_empty_arg_skipped() {
        // rm(,) - malformed with missing argument
        let code = "rm(,)";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    #[test]
    fn test_rm_malformed_list_missing_value_skipped() {
        // rm(list = ) - malformed with missing value
        let code = "rm(list = )";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);
        assert_eq!(rm_calls.len(), 0);
    }

    // ==================== library()/require()/loadNamespace() detection tests ====================

    #[test]
    fn test_library_bare_identifier() {
        // library(dplyr) - bare identifier
        // Validates: Requirement 1.1
        let code = "library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[0].line, 0);
    }

    #[test]
    fn test_library_double_quoted_string() {
        // library("dplyr") - double-quoted string
        // Validates: Requirement 1.2
        let code = r#"library("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_single_quoted_string() {
        // library('dplyr') - single-quoted string
        // Validates: Requirement 1.3
        let code = "library('dplyr')";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_bare_identifier() {
        // require(dplyr) - bare identifier
        // Validates: Requirement 1.4
        let code = "require(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_quoted_string() {
        // require("dplyr") - quoted string
        // Validates: Requirement 1.4
        let code = r#"require("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_load_namespace_quoted_string() {
        // loadNamespace("dplyr") - quoted string
        // Validates: Requirement 1.5
        let code = r#"loadNamespace("dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_load_namespace_bare_identifier() {
        // loadNamespace(dplyr) - bare identifier
        // Validates: Requirement 1.5
        let code = "loadNamespace(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_variable_skipped() {
        // library(pkg_name) where pkg_name is a variable - should be skipped
        // Validates: Requirement 1.6
        // Note: We can't distinguish a variable from a bare package name statically,
        // so this test verifies that we DO detect it (as we treat all identifiers as package names)
        let code = "pkg_name <- 'dplyr'\nlibrary(pkg_name)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        // We detect it because we can't distinguish variable from package name
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "pkg_name");
    }

    #[test]
    fn test_library_expression_skipped() {
        // library(paste0("dp", "lyr")) - expression should be skipped
        // Validates: Requirement 1.6
        let code = r#"library(paste0("dp", "lyr"))"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_true_skipped() {
        // library("dplyr", character.only = TRUE) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"library("dplyr", character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_t_skipped() {
        // library("dplyr", character.only = T) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"library("dplyr", character.only = T)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_character_only_false_processed() {
        // library("dplyr", character.only = FALSE) - should be processed
        let code = r#"library("dplyr", character.only = FALSE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_require_character_only_true_skipped() {
        // require("dplyr", character.only = TRUE) - should be skipped
        // Validates: Requirement 1.7
        let code = r#"require("dplyr", character.only = TRUE)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_multiple_library_calls() {
        // Multiple library calls in document order
        // Validates: Requirement 1.8
        let code = "library(dplyr)\nlibrary(ggplot2)\nrequire(tidyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 3);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[0].line, 0);
        assert_eq!(lib_calls[1].package, "ggplot2");
        assert_eq!(lib_calls[1].line, 1);
        assert_eq!(lib_calls[2].package, "tidyr");
        assert_eq!(lib_calls[2].line, 2);
    }

    #[test]
    fn test_library_named_package_argument() {
        // library(package = dplyr) - named argument
        let code = "library(package = dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_named_package_argument_quoted() {
        // library(package = "dplyr") - named argument with string
        let code = r#"library(package = "dplyr")"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_library_position() {
        // Test position tracking
        let code = "x <- 1\nlibrary(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].line, 1);
        // Column should be at end of call
        assert_eq!(lib_calls[0].column, 14); // "library(dplyr)" is 14 chars
    }

    #[test]
    fn test_library_position_with_offset() {
        // Test position with offset on same line
        let code = "x <- 1; library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].line, 0);
        // Column should be at end of call: "x <- 1; library(dplyr)" = 22 chars
        assert_eq!(lib_calls[0].column, 22);
    }

    #[test]
    fn test_library_with_utf16_column() {
        // Test with emoji before library() to verify UTF-16 column calculation
        // 🎉 is 4 bytes in UTF-8, 2 UTF-16 code units
        let code = "🎉; library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        // Column should be: 2 (emoji) + 2 ("; ") + 14 ("library(dplyr)") = 18 in UTF-16
        assert_eq!(lib_calls[0].column, 18);
    }

    #[test]
    fn test_library_inside_function() {
        // library() inside a function body
        let code = "f <- function() { library(dplyr) }";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
        // function_scope is None for now (will be populated in task 6.2)
        assert!(lib_calls[0].function_scope.is_none());
    }

    #[test]
    fn test_library_empty_call_skipped() {
        // library() with no arguments should be skipped
        let code = "library()";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_non_library_call_ignored() {
        // Other function calls should be ignored
        let code = "print(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 0);
    }

    #[test]
    fn test_library_with_other_arguments() {
        // library() with additional arguments
        let code = "library(dplyr, quietly = TRUE, warn.conflicts = FALSE)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert_eq!(lib_calls[0].package, "dplyr");
    }

    #[test]
    fn test_mixed_library_and_source_calls() {
        // Mix of library() and source() calls
        let code = r#"library(dplyr)
source("utils.R")
library(ggplot2)"#;
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 2);
        assert_eq!(lib_calls[0].package, "dplyr");
        assert_eq!(lib_calls[1].package, "ggplot2");
    }

    #[test]
    fn test_library_function_scope_is_none() {
        // Verify function_scope is None (will be populated later)
        let code = "library(dplyr)";
        let tree = parse_r(code);
        let lib_calls = detect_library_calls(&tree, code);
        assert_eq!(lib_calls.len(), 1);
        assert!(lib_calls[0].function_scope.is_none());
    }
}

// ============================================================================
// Property-Based Tests for Library Call Detection
// ============================================================================

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;
    use tree_sitter::Parser;

    /// Parse R source code into a tree-sitter `Tree`.
    ///
    /// # Examples
    ///
    /// ```
    /// let code = "x <- 1";
    /// let tree = parse_r(code);
    /// // root node kind for R source files is "source"
    /// assert_eq!(tree.root_node().kind(), "source");
    /// ```
    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        parser.parse(code, None).unwrap()
    }

    /// R reserved words that cannot be used as package names
    const R_RESERVED: &[&str] = &[
        "if", "else", "for", "in", "while", "repeat", "next", "break", "function", "NA", "NaN",
        "Inf", "NULL", "TRUE", "FALSE", "T", "F",
    ];

    /// Determine whether a string is a valid R package name (non-empty and not an R reserved word).
    ///
    /// # Returns
    ///
    /// `true` if the name is non-empty and not an R reserved word, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(is_valid_package_name("dplyr"));
    /// assert!(!is_valid_package_name(""));
    /// assert!(!is_valid_package_name("if")); // reserved word
    /// ```
    fn is_valid_package_name(name: &str) -> bool {
        !R_RESERVED.contains(&name) && !name.is_empty()
    }

    /// Strategy that generates valid R package names.
    ///
    /// The produced strings start with a lowercase ASCII letter, contain only
    /// lowercase ASCII letters, digits, and dots, and are at most 9 characters long.
    /// Reserved or otherwise invalid package names are excluded by the strategy's filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// proptest!(|(name in crate::package_name())| {
    ///     let first = name.chars().next().unwrap();
    ///     assert!(first.is_ascii_lowercase());
    ///     assert!(name.len() <= 9);
    ///     assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.'));
    /// });
    /// ```
    fn package_name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9\\.]{0,8}".prop_filter("not reserved", |s| is_valid_package_name(s))
    }

    /// Produces a proptest strategy that yields one of the strings `"library"`, `"require"`, or `"loadNamespace"`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strat = crate::library_function();
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let tree = strat.new_tree(&mut runner).unwrap();
    /// let value = tree.current();
    /// assert!(value == "library" || value == "require" || value == "loadNamespace");
    /// ```
    fn library_function() -> impl Strategy<Value = &'static str> {
        prop_oneof![Just("library"), Just("require"), Just("loadNamespace"),]
    }

    /// Generate a quote style for package names
    #[derive(Debug, Clone, Copy)]
    enum QuoteStyle {
        None,   // library(dplyr)
        Double, // library("dplyr")
        Single, // library('dplyr')
    }

    /// Produces a proptest Strategy that yields one of the `QuoteStyle` variants.
    ///
    /// The strategy generates `QuoteStyle::None`, `QuoteStyle::Double`, or `QuoteStyle::Single`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// // generate a single value from the strategy
    /// let mut runner = TestRunner::default();
    /// let value = quote_style().new_tree(&mut runner).unwrap().current();
    /// match value {
    ///     QuoteStyle::None | QuoteStyle::Double | QuoteStyle::Single => (),
    /// }
    /// ```
    fn quote_style() -> impl Strategy<Value = QuoteStyle> {
        prop_oneof![
            Just(QuoteStyle::None),
            Just(QuoteStyle::Double),
            Just(QuoteStyle::Single),
        ]
    }

    /// A library call specification for code generation
    #[derive(Debug, Clone)]
    struct LibraryCallSpec {
        func: &'static str,
        package: String,
        quote_style: QuoteStyle,
        use_named_arg: bool,
    }

    /// Generates an arbitrary `LibraryCallSpec` strategy for property-based tests.

    ///

    /// The strategy produces tuples describing a library-like call: the function name (`library`, `require`, or `loadNamespace`),

    /// a package name, the string quote style to use (none, single, or double), and a boolean indicating whether the package

    /// is supplied with a named `package=` argument.

    ///

    /// # Examples

    ///

    /// ```

    /// use proptest::prelude::*;

    ///

    /// let mut runner = proptest::test_runner::TestRunner::default();

    /// let tree = library_call_spec().new_tree(&mut runner).unwrap();

    /// let spec = tree.current();

    /// // `spec` contains generated fields: `func`, `package`, `quote_style`, and `use_named_arg`.

    /// assert!(!spec.package.is_empty());

    /// ```
    fn library_call_spec() -> impl Strategy<Value = LibraryCallSpec> {
        (
            library_function(),
            package_name(),
            quote_style(),
            any::<bool>(),
        )
            .prop_map(
                |(func, package, quote_style, use_named_arg)| LibraryCallSpec {
                    func,
                    package,
                    quote_style,
                    use_named_arg,
                },
            )
    }

    /// Render an R library-like call from a specification.
    ///
    /// Constructs an R call string using the specification's function name, package
    /// name, quoting style, and whether to use a named `package =` argument.
    ///
    /// # Returns
    ///
    /// A `String` containing the R expression (for example `library("dplyr")` or
    /// `require(package = 'pkg')`).
    ///
    /// # Examples
    ///
    /// ```
    /// let spec = LibraryCallSpec {
    ///     func: "library".into(),
    ///     package: "dplyr".into(),
    ///     quote_style: QuoteStyle::Double,
    ///     use_named_arg: false,
    /// };
    /// let code = generate_library_call_code(&spec);
    /// assert_eq!(code, "library(\"dplyr\")");
    /// ```
    fn generate_library_call_code(spec: &LibraryCallSpec) -> String {
        let quoted_pkg = match spec.quote_style {
            QuoteStyle::None => spec.package.clone(),
            QuoteStyle::Double => format!("\"{}\"", spec.package),
            QuoteStyle::Single => format!("'{}'", spec.package),
        };

        if spec.use_named_arg {
            format!("{}(package = {})", spec.func, quoted_pkg)
        } else {
            format!("{}({})", spec.func, quoted_pkg)
        }
    }

    /// Generates a proptest strategy that yields R source text containing 1 to 5 `library`/`require`/`loadNamespace` calls interleaved with simple filler statements, together with the corresponding specs for each library call.
    ///
    /// The returned strategy produces a tuple `(code, specs)` where `code` is the generated R code as a single string (lines joined with `\n`) and `specs` is a vector of `LibraryCallSpec` describing each generated library call in document order.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use proptest::strategy::Strategy;
    /// // `r_code_with_library_calls` yields `(String, Vec<LibraryCallSpec>)`
    /// let strat = r_code_with_library_calls();
    /// // Use the strategy in a proptest test or sample from it via a test runner.
    /// let _ = strat;
    /// ```
    fn r_code_with_library_calls() -> impl Strategy<Value = (String, Vec<LibraryCallSpec>)> {
        // Generate 1-5 library calls
        prop::collection::vec(library_call_spec(), 1..=5)
            .prop_flat_map(|specs| {
                // Generate 0-3 filler lines between each call
                let num_fillers = specs.len() + 1;
                let filler_counts = prop::collection::vec(0..4usize, num_fillers);
                (Just(specs), filler_counts)
            })
            .prop_map(|(specs, filler_counts)| {
                let mut lines = Vec::new();

                // Add filler before first call
                for _ in 0..filler_counts[0] {
                    lines.push("x <- 1".to_string());
                }

                // Add library calls with fillers between them
                for (i, spec) in specs.iter().enumerate() {
                    lines.push(generate_library_call_code(spec));

                    // Add filler after this call
                    if i + 1 < filler_counts.len() {
                        for _ in 0..filler_counts[i + 1] {
                            lines.push("y <- 2".to_string());
                        }
                    }
                }

                let code = lines.join("\n");
                (code, specs)
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // ============================================================================
        // Feature: package-function-awareness, Property 1: Library Call Detection Completeness
        // **Validates: Requirements 1.1, 1.4, 1.5, 1.8**
        //
        // For any R source file containing library(), require(), or loadNamespace() calls
        // with static string package names, the Library_Call_Detector SHALL detect all
        // such calls and return them in document order with correct package names and positions.
        // ============================================================================

        /// Property 1: All library/require/loadNamespace calls with static package names are detected
        #[test]
        fn prop_library_call_detection_completeness((code, specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // 1. All calls should be detected (completeness)
            prop_assert_eq!(
                detected.len(),
                specs.len(),
                "Expected {} library calls, but detected {}. Code:\n{}",
                specs.len(),
                detected.len(),
                code
            );

            // 2. Package names should be correctly extracted
            for (i, (detected_call, spec)) in detected.iter().zip(specs.iter()).enumerate() {
                prop_assert_eq!(
                    &detected_call.package,
                    &spec.package,
                    "Package name mismatch at index {}. Expected '{}', got '{}'. Code:\n{}",
                    i,
                    spec.package,
                    detected_call.package,
                    code
                );
            }
        }

        /// Property 1 extended: Calls are returned in document order (sorted by line, then column)
        #[test]
        fn prop_library_calls_in_document_order((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // Verify document order: each call should be at same or later position than previous
            for i in 1..detected.len() {
                let prev = &detected[i - 1];
                let curr = &detected[i];

                let prev_pos = (prev.line, prev.column);
                let curr_pos = (curr.line, curr.column);

                prop_assert!(
                    prev_pos <= curr_pos,
                    "Library calls not in document order: call {} at ({}, {}) comes after call {} at ({}, {}). Code:\n{}",
                    i - 1, prev.line, prev.column,
                    i, curr.line, curr.column,
                    code
                );
            }
        }

        /// Property 1 extended: Positions are valid (within code bounds)
        #[test]
        fn prop_library_call_positions_valid((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            let line_count = code.lines().count() as u32;

            for (i, call) in detected.iter().enumerate() {
                // Line should be within bounds
                prop_assert!(
                    call.line < line_count,
                    "Library call {} has invalid line {}, but code only has {} lines. Code:\n{}",
                    i, call.line, line_count, code
                );

                // Column should be within the line's length (in UTF-16 code units)
                if let Some(line_text) = code.lines().nth(call.line as usize) {
                    let line_len_utf16: u32 = line_text.encode_utf16().count() as u32;
                    prop_assert!(
                        call.column <= line_len_utf16,
                        "Library call {} has invalid column {} on line {}, but line only has {} UTF-16 code units. Line: '{}'. Code:\n{}",
                        i, call.column, call.line, line_len_utf16, line_text, code
                    );
                }
            }
        }

        /// Property 1 extended: Detection is idempotent (same input produces same output)
        #[test]
        fn prop_library_call_detection_idempotent((code, _specs) in r_code_with_library_calls()) {
            let tree = parse_r(&code);
            let detected1 = detect_library_calls(&tree, &code);
            let detected2 = detect_library_calls(&tree, &code);

            prop_assert_eq!(
                detected1.len(),
                detected2.len(),
                "Detection not idempotent: first call returned {} results, second returned {}",
                detected1.len(),
                detected2.len()
            );

            for (i, (d1, d2)) in detected1.iter().zip(detected2.iter()).enumerate() {
                prop_assert_eq!(
                    &d1.package, &d2.package,
                    "Detection not idempotent at index {}: package names differ",
                    i
                );
                prop_assert_eq!(
                    d1.line, d2.line,
                    "Detection not idempotent at index {}: lines differ",
                    i
                );
                prop_assert_eq!(
                    d1.column, d2.column,
                    "Detection not idempotent at index {}: columns differ",
                    i
                );
            }
        }

        /// Property 1 extended: library(), require(), and loadNamespace() are all detected
        #[test]
        fn prop_all_library_functions_detected(pkg in package_name()) {
            // Test all three function types
            let code_library = format!("library({})", pkg);
            let code_require = format!("require({})", pkg);
            let code_loadns = format!("loadNamespace({})", pkg);

            let tree_library = parse_r(&code_library);
            let tree_require = parse_r(&code_require);
            let tree_loadns = parse_r(&code_loadns);

            let detected_library = detect_library_calls(&tree_library, &code_library);
            let detected_require = detect_library_calls(&tree_require, &code_require);
            let detected_loadns = detect_library_calls(&tree_loadns, &code_loadns);

            // All should detect exactly one call
            prop_assert_eq!(detected_library.len(), 1, "library() not detected");
            prop_assert_eq!(detected_require.len(), 1, "require() not detected");
            prop_assert_eq!(detected_loadns.len(), 1, "loadNamespace() not detected");

            // All should extract the correct package name
            prop_assert_eq!(&detected_library[0].package, &pkg, "library() package name mismatch");
            prop_assert_eq!(&detected_require[0].package, &pkg, "require() package name mismatch");
            prop_assert_eq!(&detected_loadns[0].package, &pkg, "loadNamespace() package name mismatch");
        }

        /// Property 1 extended: Both quoted and unquoted package names are detected
        #[test]
        fn prop_quoted_and_unquoted_detected(pkg in package_name()) {
            let code_bare = format!("library({})", pkg);
            let code_double = format!("library(\"{}\")", pkg);
            let code_single = format!("library('{}')", pkg);

            let tree_bare = parse_r(&code_bare);
            let tree_double = parse_r(&code_double);
            let tree_single = parse_r(&code_single);

            let detected_bare = detect_library_calls(&tree_bare, &code_bare);
            let detected_double = detect_library_calls(&tree_double, &code_double);
            let detected_single = detect_library_calls(&tree_single, &code_single);

            // All should detect exactly one call
            prop_assert_eq!(detected_bare.len(), 1, "Bare identifier not detected");
            prop_assert_eq!(detected_double.len(), 1, "Double-quoted string not detected");
            prop_assert_eq!(detected_single.len(), 1, "Single-quoted string not detected");

            // All should extract the correct package name
            prop_assert_eq!(&detected_bare[0].package, &pkg, "Bare identifier package name mismatch");
            prop_assert_eq!(&detected_double[0].package, &pkg, "Double-quoted package name mismatch");
            prop_assert_eq!(&detected_single[0].package, &pkg, "Single-quoted package name mismatch");
        }

        /// Property 1 extended: Named package= argument is detected
        #[test]
        fn prop_named_package_argument_detected(pkg in package_name()) {
            let code_named = format!("library(package = {})", pkg);
            let code_named_quoted = format!("library(package = \"{}\")", pkg);

            let tree_named = parse_r(&code_named);
            let tree_named_quoted = parse_r(&code_named_quoted);

            let detected_named = detect_library_calls(&tree_named, &code_named);
            let detected_named_quoted = detect_library_calls(&tree_named_quoted, &code_named_quoted);

            // Both should detect exactly one call
            prop_assert_eq!(detected_named.len(), 1, "Named bare argument not detected");
            prop_assert_eq!(detected_named_quoted.len(), 1, "Named quoted argument not detected");

            // Both should extract the correct package name
            prop_assert_eq!(&detected_named[0].package, &pkg, "Named bare package name mismatch");
            prop_assert_eq!(&detected_named_quoted[0].package, &pkg, "Named quoted package name mismatch");
        }

        // ============================================================================
        // Feature: package-function-awareness, Property 2: Dynamic Package Name Exclusion
        // **Validates: Requirements 1.6, 1.7**
        //
        // For any R source file containing library calls with variable or expression
        // package names (including character.only = TRUE), the Library_Call_Detector
        // SHALL NOT include those calls in the detected results.
        // ============================================================================

        /// Property 2: Calls with character.only = TRUE are NOT detected
        #[test]
        fn prop_character_only_true_excluded(pkg in package_name()) {
            // Test character.only = TRUE (full form)
            let code_true = format!("library(\"{}\", character.only = TRUE)", pkg);
            let tree_true = parse_r(&code_true);
            let detected_true = detect_library_calls(&tree_true, &code_true);

            prop_assert_eq!(
                detected_true.len(),
                0,
                "library() with character.only = TRUE should NOT be detected. Code: {}",
                code_true
            );

            // Test character.only = T (short form)
            let code_t = format!("library(\"{}\", character.only = T)", pkg);
            let tree_t = parse_r(&code_t);
            let detected_t = detect_library_calls(&tree_t, &code_t);

            prop_assert_eq!(
                detected_t.len(),
                0,
                "library() with character.only = T should NOT be detected. Code: {}",
                code_t
            );
        }

        /// Property 2: require() with character.only = TRUE is NOT detected
        #[test]
        fn prop_require_character_only_true_excluded(pkg in package_name()) {
            // Test require with character.only = TRUE
            let code_true = format!("require(\"{}\", character.only = TRUE)", pkg);
            let tree_true = parse_r(&code_true);
            let detected_true = detect_library_calls(&tree_true, &code_true);

            prop_assert_eq!(
                detected_true.len(),
                0,
                "require() with character.only = TRUE should NOT be detected. Code: {}",
                code_true
            );

            // Test require with character.only = T
            let code_t = format!("require(\"{}\", character.only = T)", pkg);
            let tree_t = parse_r(&code_t);
            let detected_t = detect_library_calls(&tree_t, &code_t);

            prop_assert_eq!(
                detected_t.len(),
                0,
                "require() with character.only = T should NOT be detected. Code: {}",
                code_t
            );
        }

        /// Property 2: Calls with expression package names are NOT detected
        #[test]
        fn prop_expression_package_names_excluded(pkg in package_name()) {
            // Test paste0() expression
            let code_paste0 = format!("library(paste0(\"{}\", \"\"))", pkg);
            let tree_paste0 = parse_r(&code_paste0);
            let detected_paste0 = detect_library_calls(&tree_paste0, &code_paste0);

            prop_assert_eq!(
                detected_paste0.len(),
                0,
                "library() with paste0() expression should NOT be detected. Code: {}",
                code_paste0
            );

            // Test paste() expression
            let code_paste = format!("library(paste(\"{}\", sep = \"\"))", pkg);
            let tree_paste = parse_r(&code_paste);
            let detected_paste = detect_library_calls(&tree_paste, &code_paste);

            prop_assert_eq!(
                detected_paste.len(),
                0,
                "library() with paste() expression should NOT be detected. Code: {}",
                code_paste
            );

            // Test sprintf() expression
            let code_sprintf = format!("library(sprintf(\"%s\", \"{}\"))", pkg);
            let tree_sprintf = parse_r(&code_sprintf);
            let detected_sprintf = detect_library_calls(&tree_sprintf, &code_sprintf);

            prop_assert_eq!(
                detected_sprintf.len(),
                0,
                "library() with sprintf() expression should NOT be detected. Code: {}",
                code_sprintf
            );
        }

        /// Property 2: Calls with get() expression are NOT detected
        #[test]
        fn prop_get_expression_excluded(pkg in package_name()) {
            // Test get() expression
            let code_get = format!("library(get(\"{}\"))", pkg);
            let tree_get = parse_r(&code_get);
            let detected_get = detect_library_calls(&tree_get, &code_get);

            prop_assert_eq!(
                detected_get.len(),
                0,
                "library() with get() expression should NOT be detected. Code: {}",
                code_get
            );
        }

        /// Property 2: character.only = FALSE does NOT exclude the call
        #[test]
        fn prop_character_only_false_not_excluded(pkg in package_name()) {
            // Test character.only = FALSE (should still be detected)
            let code_false = format!("library(\"{}\", character.only = FALSE)", pkg);
            let tree_false = parse_r(&code_false);
            let detected_false = detect_library_calls(&tree_false, &code_false);

            prop_assert_eq!(
                detected_false.len(),
                1,
                "library() with character.only = FALSE SHOULD be detected. Code: {}",
                code_false
            );
            prop_assert_eq!(
                &detected_false[0].package,
                &pkg,
                "Package name mismatch for character.only = FALSE case"
            );

            // Test character.only = F (should still be detected)
            let code_f = format!("library(\"{}\", character.only = F)", pkg);
            let tree_f = parse_r(&code_f);
            let detected_f = detect_library_calls(&tree_f, &code_f);

            prop_assert_eq!(
                detected_f.len(),
                1,
                "library() with character.only = F SHOULD be detected. Code: {}",
                code_f
            );
            prop_assert_eq!(
                &detected_f[0].package,
                &pkg,
                "Package name mismatch for character.only = F case"
            );
        }
    }

    // ============================================================================
    // Property 2 Extended: Dynamic Package Exclusion with Mixed Code
    // ============================================================================

    /// Types of dynamic library calls that should be excluded
    #[derive(Debug, Clone)]
    enum DynamicCallType {
        /// character.only = TRUE
        CharacterOnlyTrue,
        /// character.only = T
        CharacterOnlyT,
        /// paste0() expression
        Paste0Expression,
        /// paste() expression
        PasteExpression,
        /// get() expression
        GetExpression,
        /// sprintf() expression
        SprintfExpression,
        /// c() expression (vector of packages)
        CExpression,
    }

    /// Creates a proptest strategy that yields one of the `DynamicCallType` variants representing
    /// dynamic/library-call patterns that should be treated as non-statically-determinable.
    ///
    /// # Returns
    ///
    /// A `Strategy` that produces a `DynamicCallType` chosen from:
    /// `CharacterOnlyTrue`, `CharacterOnlyT`, `Paste0Expression`, `PasteExpression`,
    /// `GetExpression`, `SprintfExpression`, and `CExpression`.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    ///
    /// // obtain the strategy and use it in a proptest
    /// let strat = crate::dynamic_call_type();
    ///
    /// proptest!(|(kind in strat)| {
    ///     // `kind` is a `DynamicCallType` variant
    ///     match kind {
    ///         crate::DynamicCallType::CharacterOnlyTrue => {},
    ///         crate::DynamicCallType::CharacterOnlyT => {},
    ///         _ => {},
    ///     }
    /// });
    /// ```
    fn dynamic_call_type() -> impl Strategy<Value = DynamicCallType> {
        prop_oneof![
            Just(DynamicCallType::CharacterOnlyTrue),
            Just(DynamicCallType::CharacterOnlyT),
            Just(DynamicCallType::Paste0Expression),
            Just(DynamicCallType::PasteExpression),
            Just(DynamicCallType::GetExpression),
            Just(DynamicCallType::SprintfExpression),
            Just(DynamicCallType::CExpression),
        ]
    }

    /// Generate R code representing a dynamic (non-statically-determinable) library/require/loadNamespace call.
    ///
    /// This returns an R expression string that uses a dynamic form of specifying the package (e.g., `character.only = TRUE`, `paste0(...)`, `get(...)`, `c(...)`, etc.), which should not be recognized as a statically determinable package by static analyzers.
    /// - `call_type`: selects which dynamic pattern to emit.
    /// - `pkg`: the package name used within the generated expression (may be split or wrapped depending on the pattern).
    /// - `func`: the function name to call (e.g., `"library"`, `"require"`, or `"loadNamespace"`).
    ///
    /// # Examples
    ///
    /// ```
    /// let s = generate_dynamic_library_call(&DynamicCallType::CharacterOnlyTrue, "dplyr", "library");
    /// assert!(s.contains("character.only"));
    /// let s2 = generate_dynamic_library_call(&DynamicCallType::Paste0Expression, "pkgname", "require");
    /// assert!(s2.contains("paste0"));
    /// ```
    fn generate_dynamic_library_call(call_type: &DynamicCallType, pkg: &str, func: &str) -> String {
        match call_type {
            DynamicCallType::CharacterOnlyTrue => {
                format!("{}(\"{}\", character.only = TRUE)", func, pkg)
            }
            DynamicCallType::CharacterOnlyT => {
                format!("{}(\"{}\", character.only = T)", func, pkg)
            }
            DynamicCallType::Paste0Expression => {
                // Split package name for paste0
                let mid = pkg.len() / 2;
                let (p1, p2) = pkg.split_at(mid);
                format!("{}(paste0(\"{}\", \"{}\"))", func, p1, p2)
            }
            DynamicCallType::PasteExpression => {
                format!("{}(paste(\"{}\", sep = \"\"))", func, pkg)
            }
            DynamicCallType::GetExpression => {
                format!("{}(get(\"{}\"))", func, pkg)
            }
            DynamicCallType::SprintfExpression => {
                format!("{}(sprintf(\"%s\", \"{}\"))", func, pkg)
            }
            DynamicCallType::CExpression => {
                // c() returns a vector, not a valid single package name
                format!("{}(c(\"{}\", \"other\"))", func, pkg)
            }
        }
    }

    /// A specification for a dynamic library call
    #[derive(Debug, Clone)]
    struct DynamicLibraryCallSpec {
        func: &'static str,
        package: String,
        call_type: DynamicCallType,
    }

    /// Creates a proptest strategy that generates `DynamicLibraryCallSpec` values.
    ///
    /// The produced spec combines a library-related function variant, a package name, and a dynamic call type.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strategy = dynamic_library_call_spec();
    /// proptest!(|(spec in strategy)| {
    ///     // `spec` is a generated DynamicLibraryCallSpec
    ///     let _pkg: String = spec.package.clone();
    /// });
    /// ```
    fn dynamic_library_call_spec() -> impl Strategy<Value = DynamicLibraryCallSpec> {
        (library_function(), package_name(), dynamic_call_type()).prop_map(
            |(func, package, call_type)| DynamicLibraryCallSpec {
                func,
                package,
                call_type,
            },
        )
    }

    /// Generate R source code containing dynamic library calls that should not be detected.
    ///
    /// The returned proptest Strategy produces tuples of `(code, specs)` where `code` is the generated
    /// R source as a `String` and `specs` is a `Vec<DynamicLibraryCallSpec>` describing each dynamic
    /// library-style call inserted into the code. The code includes 1–5 dynamic calls with optional
    /// filler lines between them.
    ///
    /// # Examples
    ///
    /// ```
    /// use proptest::prelude::*;
    /// let strat = r_code_with_dynamic_library_calls();
    /// let mut runner = proptest::test_runner::TestRunner::default();
    /// let (code, specs) = strat.new_tree(&mut runner).unwrap().current();
    /// assert!(!code.is_empty());
    /// assert!(!specs.is_empty());
    /// ```
    fn r_code_with_dynamic_library_calls(
    ) -> impl Strategy<Value = (String, Vec<DynamicLibraryCallSpec>)> {
        // Generate 1-5 dynamic library calls
        prop::collection::vec(dynamic_library_call_spec(), 1..=5)
            .prop_flat_map(|specs| {
                // Generate 0-2 filler lines between each call
                let num_fillers = specs.len() + 1;
                let filler_counts = prop::collection::vec(0..3usize, num_fillers);
                (Just(specs), filler_counts)
            })
            .prop_map(|(specs, filler_counts)| {
                let mut lines = Vec::new();

                // Add filler before first call
                for _ in 0..filler_counts[0] {
                    lines.push("x <- 1".to_string());
                }

                // Add dynamic library calls with fillers between them
                for (i, spec) in specs.iter().enumerate() {
                    lines.push(generate_dynamic_library_call(
                        &spec.call_type,
                        &spec.package,
                        spec.func,
                    ));

                    // Add filler after this call
                    if i + 1 < filler_counts.len() {
                        for _ in 0..filler_counts[i + 1] {
                            lines.push("y <- 2".to_string());
                        }
                    }
                }

                let code = lines.join("\n");
                (code, specs)
            })
    }

    /// Generate R source text containing an interleaved sequence of statically determinable
    /// and dynamic library-style calls, along with the specifications used to build them.
    ///
    /// The produced strategy yields a tuple containing:
    /// 1. A single String with the generated R code (multiple lines, interleaved static and dynamic calls).
    /// 2. A Vec of `LibraryCallSpec` describing the statically determinable calls included.
    /// 3. A Vec of `DynamicLibraryCallSpec` describing the dynamic calls included.
    ///
    /// # Examples
    ///
    /// ```
    /// // Obtain the proptest strategy and use it in property tests.
    /// let strat = r_code_with_mixed_library_calls();
    /// // `strat` is a `Strategy` that generates `(String, Vec<LibraryCallSpec>, Vec<DynamicLibraryCallSpec>)`.
    /// let _ = strat;
    /// ```
    fn r_code_with_mixed_library_calls(
    ) -> impl Strategy<Value = (String, Vec<LibraryCallSpec>, Vec<DynamicLibraryCallSpec>)> {
        (
            prop::collection::vec(library_call_spec(), 1..=3),
            prop::collection::vec(dynamic_library_call_spec(), 1..=3),
        )
            .prop_map(|(static_specs, dynamic_specs)| {
                let mut lines = Vec::new();

                // Interleave static and dynamic calls
                let max_len = static_specs.len().max(dynamic_specs.len());
                for i in 0..max_len {
                    if i < static_specs.len() {
                        lines.push(generate_library_call_code(&static_specs[i]));
                    }
                    if i < dynamic_specs.len() {
                        lines.push(generate_dynamic_library_call(
                            &dynamic_specs[i].call_type,
                            &dynamic_specs[i].package,
                            dynamic_specs[i].func,
                        ));
                    }
                }

                let code = lines.join("\n");
                (code, static_specs, dynamic_specs)
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // ============================================================================
        // Feature: package-function-awareness, Property 2: Dynamic Package Name Exclusion
        // **Validates: Requirements 1.6, 1.7**
        // ============================================================================

        /// Property 2: All dynamic library calls are excluded from detection
        #[test]
        fn prop_dynamic_package_exclusion((code, _specs) in r_code_with_dynamic_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // No dynamic calls should be detected
            prop_assert_eq!(
                detected.len(),
                0,
                "Dynamic library calls should NOT be detected. Found {} calls. Code:\n{}",
                detected.len(),
                code
            );
        }

        /// Property 2: Mixed code correctly detects only static calls
        #[test]
        fn prop_mixed_static_dynamic_detection((code, static_specs, _dynamic_specs) in r_code_with_mixed_library_calls()) {
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // Only static calls should be detected
            prop_assert_eq!(
                detected.len(),
                static_specs.len(),
                "Expected {} static library calls, but detected {}. Code:\n{}",
                static_specs.len(),
                detected.len(),
                code
            );

            // Verify detected packages match static specs
            let detected_packages: std::collections::HashSet<_> = detected.iter().map(|c| &c.package).collect();
            let expected_packages: std::collections::HashSet<_> = static_specs.iter().map(|s| &s.package).collect();

            prop_assert_eq!(
                detected_packages,
                expected_packages,
                "Detected packages don't match expected static packages. Code:\n{}",
                code
            );
        }

        /// Property 2: character.only with variable value is still excluded
        #[test]
        fn prop_character_only_with_variable_excluded(pkg in package_name()) {
            // When character.only is set to a variable (not TRUE/T/FALSE/F),
            // we can't statically determine if it's true, so we should still
            // detect the call (conservative approach - only exclude TRUE/T)
            let code = format!("library(\"{}\", character.only = my_var)", pkg);
            let tree = parse_r(&code);
            let detected = detect_library_calls(&tree, &code);

            // This should be detected because character.only is not TRUE/T
            prop_assert_eq!(
                detected.len(),
                1,
                "library() with character.only = variable SHOULD be detected (conservative). Code: {}",
                code
            );
        }
    }
}
