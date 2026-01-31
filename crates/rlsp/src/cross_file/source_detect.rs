//
// cross_file/source_detect.rs
//
// Detection of source() and sys.source() calls using tree-sitter
// Detection of rm() and remove() calls for scope tracking
//

use tree_sitter::{Node, Tree};

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

/// Detect source() and sys.source() calls in R code.
/// Parses AST to find source calls with their parameters (local, chdir, envir).
pub fn detect_source_calls(tree: &Tree, content: &str) -> Vec<ForwardSource> {
    log::trace!("Starting tree-sitter parsing for source() call detection");
    let mut sources = Vec::new();
    let root = tree.root_node();
    visit_node(root, content, &mut sources);
    log::trace!("Completed source() call detection, found {} calls", sources.len());
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

/// Extract string arguments from a c() call.
/// Only extracts string literals, skips other argument types.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_r(code: &str) -> Tree {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
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
}