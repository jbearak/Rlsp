//! Flag vector/scalar logical operators used in the opposite context.
//!
//! Mirrors `lintr::vector_logic_linter`. `if (x & y)` triggers a warning in
//! R 4.3+ (`condition has length > 1`) and silently does the wrong thing on
//! older R when either side returns a vector. Scalar short-circuit operators
//! (`&&` / `||`) are the correct choice in scalar contexts.
//!
//! The scan walks `if` / `while` conditions and `expect_true()` /
//! `expect_false()` assertions, reporting `&` / `|` where scalar `&&` / `||`
//! is expected. It also checks `filter()` / `subset()` predicates in the other
//! direction, reporting `&&` / `||` where vectorized `&` / `|` is expected.
//! Function-call boundaries stop the recursion: `if (any(x & y))` is fine
//! because the `&` is evaluated inside `any()` on a vector, not on the
//! condition itself. lintr applies the same carve-out.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::linting::LINT_SOURCE;
use crate::linting::nolint::Suppressions;
use crate::linting::rule_ids;
use crate::utf16::byte_offset_to_utf16_column;

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    visit(root, text, severity, suppressions, out);
}

fn visit(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    if matches!(node.kind(), "if_statement" | "while_statement")
        && let Some(cond) = node.child_by_field_name("condition")
    {
        scan_condition(cond, text, severity, suppressions, out);
    }
    if node.kind() == "call" {
        check_call_context(node, text, severity, suppressions, out);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

fn check_call_context(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let function_text = text
        .get(function.start_byte()..function.end_byte())
        .unwrap_or("");
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };

    if matches_function(function_text, "expect_true")
        || matches_function(function_text, "expect_false")
    {
        if let Some(value) = first_argument_value(arguments) {
            scan_condition(value, text, severity, suppressions, out);
        }
        return;
    }

    let is_filter = matches_function(function_text, "filter") && function_text != "stats::filter";
    let is_subset = matches_function(function_text, "subset");
    if !is_filter && !is_subset {
        return;
    }

    let mut skipped_data_argument = false;
    let mut cursor = arguments.walk();
    for argument in arguments
        .children(&mut cursor)
        .filter(|child| child.kind() == "argument")
    {
        let name = argument
            .child_by_field_name("name")
            .and_then(|name| text.get(name.start_byte()..name.end_byte()));
        if name == Some("circular") {
            continue;
        }
        let is_named_data_argument =
            (is_filter && name == Some(".data")) || (is_subset && name == Some("x"));
        if !skipped_data_argument && (name.is_none() || is_named_data_argument) {
            skipped_data_argument = true;
            continue;
        }
        if let Some(value) = argument.child_by_field_name("value") {
            scan_filter_predicate(value, text, severity, suppressions, out);
        }
    }
}

fn matches_function(actual: &str, name: &str) -> bool {
    actual == name || actual.rsplit(':').next() == Some(name)
}

fn first_argument_value(arguments: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = arguments.walk();
    arguments
        .children(&mut cursor)
        .find(|child| child.kind() == "argument")?
        .child_by_field_name("value")
}

fn scan_filter_predicate(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    if matches!(
        node.kind(),
        "call" | "subset" | "subset2" | "function_definition"
    ) {
        return;
    }
    if node.kind() == "binary_operator"
        && let Some(op) = node.child_by_field_name("operator")
    {
        let op_text = text.get(op.start_byte()..op.end_byte()).unwrap_or("");
        if op_text == "&&" || op_text == "||" {
            let preferred = if op_text == "&&" { "&" } else { "|" };
            emit(op, op_text, preferred, text, severity, suppressions, out);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_filter_predicate(child, text, severity, suppressions, out);
    }
}

fn scan_condition(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    // Stop at call boundaries — the operands of a call are evaluated as a
    // vector context independently of the surrounding scalar condition.
    if matches!(node.kind(), "call" | "subset" | "subset2") {
        return;
    }
    if node.kind() == "binary_operator"
        && let Some(op) = node.child_by_field_name("operator")
    {
        let op_text = text.get(op.start_byte()..op.end_byte()).unwrap_or("");
        if op_text == "&" || op_text == "|" {
            let preferred = if op_text == "&" { "&&" } else { "||" };
            emit(op, op_text, preferred, text, severity, suppressions, out);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_condition(child, text, severity, suppressions, out);
    }
}

fn emit(
    op: Node<'_>,
    op_text: &str,
    preferred: &str,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let line_no = op.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::VECTOR_LOGIC) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, op.start_position().column);
    let end_col = byte_offset_to_utf16_column(line_text, op.end_position().column);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(op.end_position().row as u32, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::VECTOR_LOGIC.to_string())),
        message: format!("Use `{preferred}` for this logical context instead of `{op_text}`."),
        ..Default::default()
    });
}
