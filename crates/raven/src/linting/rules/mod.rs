//! Individual lint rules.
//!
//! Each rule is a small module exposing a `collect(...)` function that pushes
//! diagnostics into a `&mut Vec<Diagnostic>`. Rules consult the shared
//! [`crate::linting::nolint::Suppressions`] before producing output.

pub(crate) mod assignment_operator;
pub(crate) mod commas;
pub(crate) mod commented_code;
pub(crate) mod condition_assignment;
pub(crate) mod equals_na;
pub(crate) mod function_left_parentheses;
pub(crate) mod indentation;
pub(crate) mod infix_spaces;
pub(crate) mod line_length;
pub(crate) mod mixed_logical;
pub(crate) mod no_tab;
pub(crate) mod object_length;
pub(crate) mod object_name;
pub(crate) mod quotes;
pub(crate) mod semicolon;
pub(crate) mod spaces_inside;
pub(crate) mod t_and_f_symbol;
pub(crate) mod trailing_blank_lines;
pub(crate) mod trailing_whitespace;
pub(crate) mod vector_logic;

/// Sorted byte ranges of every `string` node in the tree. Shared by the
/// text-scanning rules (`trailing_whitespace`, `no_tab`) that need lintr's
/// "not inside a string literal" exemptions.
pub(crate) fn string_byte_ranges(root: tree_sitter::Node<'_>) -> Vec<(usize, usize)> {
    fn walk(node: tree_sitter::Node<'_>, out: &mut Vec<(usize, usize)>) {
        if node.kind() == "string" {
            out.push((node.start_byte(), node.end_byte()));
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort_unstable();
    out
}

/// True when `pos` lies inside one of the sorted, non-overlapping `ranges`.
pub(crate) fn inside_any_range(ranges: &[(usize, usize)], pos: usize) -> bool {
    match ranges.binary_search_by(|&(s, _)| s.cmp(&pos)) {
        Ok(_) => true,
        Err(i) => i > 0 && pos < ranges[i - 1].1,
    }
}
