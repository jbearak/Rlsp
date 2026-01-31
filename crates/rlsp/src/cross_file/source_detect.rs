//
// cross_file/source_detect.rs
//
// Detection of source() and sys.source() calls using tree-sitter
//

use tree_sitter::{Node, Tree};

use super::types::{byte_offset_to_utf16_column, ForwardSource};

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
}