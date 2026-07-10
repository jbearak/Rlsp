//! Flag equality comparisons against `NA`.
//!
//! Mirrors `lintr::equals_na_linter`. `x == NA` is silently wrong: the result
//! is itself `NA`, never `TRUE`/`FALSE`. The idiomatic check is `is.na(x)`.
//! Applies to every typed `NA` token tree-sitter-r recognises (`NA`,
//! `NA_integer_`, `NA_real_`, `NA_character_`, `NA_complex_`), as `==` and
//! `!=` on either side, and as `x %in% NA` (RHS only — `NA %in% x` is a
//! legitimate membership test, and lintr leaves it alone).

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
    if node.kind() == "binary_operator" {
        check(node, text, severity, suppressions, out);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

fn check(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(op) = node.child_by_field_name("operator") else {
        return;
    };
    let op_text = text.get(op.start_byte()..op.end_byte()).unwrap_or("");
    if op_text != "==" && op_text != "!=" && op_text != "%in%" {
        return;
    }
    let lhs = node.child_by_field_name("lhs");
    let rhs = node.child_by_field_name("rhs");
    let na_side = if op_text != "%in%" && lhs.is_some_and(is_na_literal) {
        lhs
    } else if rhs.is_some_and(is_na_literal) {
        rhs
    } else {
        return;
    };
    let Some(na) = na_side else {
        return;
    };
    let line_no = op.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::EQUALS_NA) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, op.start_position().column);
    let end_col = byte_offset_to_utf16_column(line_text, op.end_position().column);
    let na_text = text.get(na.start_byte()..na.end_byte()).unwrap_or("NA");
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(op.end_position().row as u32, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::EQUALS_NA.to_string())),
        message: format!(
            "Use `is.na(x)` instead of `x {op_text} {na_text}`; comparison with `NA` returns `NA`."
        ),
        ..Default::default()
    });
}

/// True when the node is an `na` literal (`NA`, `NA_integer_`, …).
fn is_na_literal(node: Node<'_>) -> bool {
    node.kind() == "na"
}
