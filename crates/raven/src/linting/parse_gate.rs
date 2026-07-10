//! Shared "does this text parse as real R code?" gate.
//!
//! Used by both the `commented_code` rule and the inline-`# nolint`
//! fallback in `nolint`, so the definition of "parsable R code" lives in
//! one place. The gate is intentionally conservative: bare identifiers,
//! literals, and parse errors are treated as prose.

use tree_sitter::Node;

use crate::parser_pool::with_parser;

/// Try-parse `text` and decide whether it looks like real R code.
///
/// Requirements:
/// 1. After stripping a trailing `,` / `%>%` / `|>` and a leading `,` (lintr
///    does the same — commented-out fragments of argument lists and pipe
///    chains are still code), the parsed tree contains no `ERROR` nodes
///    (`Node::has_error()` covers both syntax errors and `MISSING`
///    placeholders).
/// 2. No two top-level expressions sit on one line without a `;` between
///    them. R's parser rejects juxtaposition (`something like i + 1`), but
///    tree-sitter tolerates it — without this check, any prose containing an
///    operator or call would count as code.
/// 3. The tree contains at least one node whose kind is in the "code-like"
///    set: function calls, binary/unary operators, assignment, function
///    definition, control flow, formula, or extract/namespace operators.
///    Pure identifiers, literals, and strings on their own do not qualify —
///    nor do binary `-` or unary `-`/`+`/`?` alone (`1-a`, `?data.frame`):
///    lintr deliberately dropped `-` from its candidate set because such
///    comments are usually prose, and `?` is a help invocation.
pub(crate) fn looks_like_code(stripped: &str) -> bool {
    let mut trimmed = stripped.trim();
    if trimmed.is_empty() {
        return false;
    }
    for suffix in [",", "%>%", "|>"] {
        if let Some(rest) = trimmed.strip_suffix(suffix) {
            trimmed = rest.trim_end();
            break;
        }
    }
    trimmed = trimmed.strip_prefix(',').unwrap_or(trimmed).trim_start();
    if trimmed.is_empty() {
        return false;
    }

    let text = trimmed.to_string();
    let tree = match with_parser(|p| p.parse(&text, None)) {
        Some(t) => t,
        None => return false,
    };
    let root = tree.root_node();
    if root.has_error() {
        return false;
    }
    if has_same_line_juxtaposition(root, &text) {
        return false;
    }

    contains_code_like(root, &text)
}

/// True when two consecutive expressions in a statement sequence (the
/// program root or a braced block) share a line with no `;` between them —
/// valid to tree-sitter, a syntax error to R.
fn has_same_line_juxtaposition(node: Node<'_>, text: &str) -> bool {
    if matches!(node.kind(), "program" | "braced_expression") {
        let mut cursor = node.walk();
        let mut prev: Option<Node<'_>> = None;
        for child in node.children(&mut cursor) {
            if child.kind() == "comment" || !child.is_named() {
                continue;
            }
            if let Some(prev) = prev
                && prev.end_position().row == child.start_position().row
                && !text
                    .get(prev.end_byte()..child.start_byte())
                    .is_some_and(|gap| gap.contains(';'))
            {
                return true;
            }
            prev = Some(child);
        }
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| has_same_line_juxtaposition(child, text))
}

fn contains_code_like(node: Node<'_>, text: &str) -> bool {
    if is_code_like_kind(node, text) {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if contains_code_like(child, text) {
            return true;
        }
    }
    false
}

fn is_code_like_kind(node: Node<'_>, text: &str) -> bool {
    let op_text = |node: Node<'_>| {
        node.child_by_field_name("operator")
            .and_then(|op| text.get(op.start_byte()..op.end_byte()))
            .unwrap_or("")
    };
    match node.kind() {
        // Binary `-` alone is weak evidence (`1-a` is usually prose ranges
        // or hyphenation); lintr dropped it from its candidate set too.
        "binary_operator" => op_text(node) != "-",
        // Unary `-`/`+` are signs, `?` is a help lookup — not evidence.
        "unary_operator" => !matches!(op_text(node), "-" | "+" | "?"),
        "call"
        | "function_definition"
        | "if_statement"
        | "for_statement"
        | "while_statement"
        | "repeat_statement"
        | "extract_operator"
        | "namespace_operator"
        | "subset"
        | "subset2"
        | "braced_expression" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_obvious_call() {
        assert!(looks_like_code("foo(bar, baz)"));
        assert!(looks_like_code("x <- 1"));
        assert!(looks_like_code("x + y"));
        assert!(looks_like_code("function(x) x + 1"));
    }

    #[test]
    fn skips_prose() {
        assert!(!looks_like_code("foo"));
        assert!(!looks_like_code("returns NULL"));
        assert!(!looks_like_code("x in {1, 2, 3}"));
        assert!(!looks_like_code(""));
        assert!(!looks_like_code("   "));
        assert!(!looks_like_code("42"));
    }

    #[test]
    fn skips_juxtaposed_prose_containing_code_shapes() {
        // tree-sitter tolerates juxtaposition; R's parser does not. Prose
        // like this must never count as code (lintr is clean on all).
        assert!(!looks_like_code("something like i + 1"));
        assert!(!looks_like_code("use foo(x) instead"));
        assert!(!looks_like_code("non-code comment"));
        // `;`-separated expressions on one line are real R.
        assert!(looks_like_code("x <- 1; y <- 2"));
        // Juxtaposition nested inside braces is still invalid R.
        assert!(!looks_like_code("{ use foo(x) instead }"));
        assert!(looks_like_code("{ foo(x) }"));
    }

    #[test]
    fn weak_operators_are_not_evidence() {
        assert!(!looks_like_code("1-a"));
        assert!(!looks_like_code("?data.frame"));
        assert!(!looks_like_code("-1"));
        // ...but they don't block real evidence elsewhere.
        assert!(looks_like_code("x <- 1 - a"));
    }

    #[test]
    fn strips_dangling_commas_and_pipes() {
        // Commented-out argument-list lines and pipe-chain fragments.
        assert!(looks_like_code("var.equal = TRUE,"));
        assert!(looks_like_code(", var.equal = TRUE"));
        assert!(looks_like_code("f() %>%"));
        assert!(looks_like_code("f() |>"));
    }
}
