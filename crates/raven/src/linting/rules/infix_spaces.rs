//! Enforce conventional spacing around infix operators.
//!
//! Walks the tree-sitter AST and flags whitespace that disagrees with R's
//! community style (matching `lintr::infix_spaces_linter` semantics):
//!
//! * Most low-precedence binary operators (`+`, `-`, `*`, `/`, comparison, logical,
//!   assignment, pipe, formula `~`, `%any%` user-defined operators) require at
//!   least one space on each side.
//! * Exponentiation (`^`) is not checked. Both `x^2` and `x ^ 2` are accepted,
//!   matching `lintr::infix_spaces_linter`, which excludes high-precedence
//!   operators from this rule.
//! * Tight-binding operators (`:` sequence, `::`/`:::` namespace, `$`/`@`
//!   member access) take no spaces on either side.
//! * Unary `-`, `+`, `!`, and unary `?` take no space between the operator and
//!   its operand.
//!
//! The rule is conservative: it only flags clearly wrong cases. It does *not*
//! flag "extra" spaces around operators that require spaces — alignment whitespace
//! is common (`x   <- 1`) and collapsing it would be more annoying than helpful.
//! Line continuations — operator at end of line, operand on the next line — are
//! left alone since the line break itself supplies the separation.
//!
//! Disambiguation between unary and binary forms is handled by tree-sitter:
//! `-x` parses as a `unary_operator`, `a - b` as a `binary_operator`. Named
//! arguments and formal defaults are separate `argument` / `parameter` nodes;
//! their `=` tokens are checked explicitly, as lintr does.

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
    match node.kind() {
        "binary_operator" => check_binary(node, text, severity, suppressions, out),
        "namespace_operator" | "extract_operator" => {
            check_tight_binary(node, text, severity, suppressions, out)
        }
        "unary_operator" => check_unary(node, text, severity, suppressions, out),
        "argument" | "parameter" => check_named_equals(node, text, severity, suppressions, out),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

fn check_named_equals(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(lhs) = node.child_by_field_name("name") else {
        return;
    };
    let Some(rhs) = node
        .child_by_field_name("value")
        .or_else(|| node.child_by_field_name("default"))
    else {
        return;
    };
    let mut operator = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "=" {
            operator = Some(child);
            break;
        }
    }
    let Some(operator) = operator else {
        return;
    };

    if gap_text(text, lhs.end_byte(), operator.start_byte()).is_some_and(str::is_empty) {
        report(
            text,
            operator,
            "missing space before `",
            "=",
            "`",
            severity,
            suppressions,
            out,
        );
    }
    if gap_text(text, operator.end_byte(), rhs.start_byte()).is_some_and(str::is_empty) {
        report(
            text,
            operator,
            "missing space after `",
            "=",
            "`",
            severity,
            suppressions,
            out,
        );
    }
}

/// Required-spaces vs. no-spaces classification for `binary_operator` tokens.
enum BinaryStyle {
    /// At least one whitespace character on each side.
    RequireSpaces,
    /// No whitespace on either side.
    NoSpaces,
    /// Skip — handled by another rule or context-dependent.
    Skip,
}

fn classify_binary(op_text: &str) -> BinaryStyle {
    // The `%...%` family of user-defined infix operators always requires
    // spaces. tree-sitter-r reports the operator text as the literal `%...%`
    // form, so a simple prefix/suffix test is sufficient.
    if op_text.starts_with('%') && op_text.ends_with('%') && op_text.len() >= 2 {
        return BinaryStyle::RequireSpaces;
    }

    match op_text {
        "+" | "-" | "*" | "/" | "<" | ">" | "<=" | ">=" | "==" | "!=" | "&" | "|" | "&&" | "||"
        | "<-" | "<<-" | "->" | "->>" | "=" | "|>" | "~" => BinaryStyle::RequireSpaces,
        ":" => BinaryStyle::NoSpaces,
        "^" => BinaryStyle::Skip,
        _ => BinaryStyle::Skip,
    }
}

fn check_binary(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(op) = node.child_by_field_name("operator") else {
        return;
    };
    let op_text = match text.get(op.start_byte()..op.end_byte()) {
        Some(s) => s,
        None => return,
    };
    let style = classify_binary(op_text);
    let Some((lhs, rhs)) = lhs_and_rhs(node) else {
        return;
    };

    let left_gap = gap_text(text, lhs.end_byte(), op.start_byte());
    let right_gap = gap_text(text, op.end_byte(), rhs.start_byte());

    match style {
        BinaryStyle::RequireSpaces => {
            if left_gap.is_some_and(|g| g.is_empty()) {
                report(
                    text,
                    op,
                    "missing space before `",
                    op_text,
                    "`",
                    severity,
                    suppressions,
                    out,
                );
            }
            if right_gap.is_some_and(|g| g.is_empty()) {
                report(
                    text,
                    op,
                    "missing space after `",
                    op_text,
                    "`",
                    severity,
                    suppressions,
                    out,
                );
            }
        }
        BinaryStyle::NoSpaces => {
            if left_gap.is_some_and(|g| !g.is_empty()) {
                report(
                    text,
                    op,
                    "unexpected whitespace before `",
                    op_text,
                    "`",
                    severity,
                    suppressions,
                    out,
                );
            }
            if right_gap.is_some_and(|g| !g.is_empty()) {
                report(
                    text,
                    op,
                    "unexpected whitespace after `",
                    op_text,
                    "`",
                    severity,
                    suppressions,
                    out,
                );
            }
        }
        BinaryStyle::Skip => {}
    }
}

fn check_tight_binary(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(op) = node.child_by_field_name("operator") else {
        return;
    };
    let op_text = match text.get(op.start_byte()..op.end_byte()) {
        Some(s) => s,
        None => return,
    };
    let Some((lhs, rhs)) = lhs_and_rhs(node) else {
        return;
    };

    let left_gap = gap_text(text, lhs.end_byte(), op.start_byte());
    let right_gap = gap_text(text, op.end_byte(), rhs.start_byte());

    if left_gap.is_some_and(|g| !g.is_empty()) {
        report(
            text,
            op,
            "unexpected whitespace before `",
            op_text,
            "`",
            severity,
            suppressions,
            out,
        );
    }
    if right_gap.is_some_and(|g| !g.is_empty()) {
        report(
            text,
            op,
            "unexpected whitespace after `",
            op_text,
            "`",
            severity,
            suppressions,
            out,
        );
    }
}

fn check_unary(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(op) = node.child_by_field_name("operator") else {
        return;
    };
    let op_text = match text.get(op.start_byte()..op.end_byte()) {
        Some(s) => s,
        None => return,
    };
    // Only the no-space unary operators are flagged. Unary `~` (formula head,
    // e.g. `~ x`) is left alone — both `~x` and `~ x` are idiomatic.
    if !matches!(op_text, "-" | "+" | "!" | "?") {
        return;
    }
    let Some(operand) = node.child_by_field_name("rhs") else {
        return;
    };
    let gap = gap_text(text, op.end_byte(), operand.start_byte());
    if gap.is_some_and(|g| !g.is_empty()) {
        report(
            text,
            op,
            "unexpected whitespace after unary `",
            op_text,
            "`",
            severity,
            suppressions,
            out,
        );
    }
}

/// Return the text between byte offsets `start` and `end` only if it stays on
/// a single line — i.e. contains no `\n`. Returning `None` for cross-line gaps
/// lets callers skip the check (line-continuation case).
fn gap_text(text: &str, start: usize, end: usize) -> Option<&str> {
    let slice = text.get(start..end)?;
    if slice.as_bytes().contains(&b'\n') {
        None
    } else {
        Some(slice)
    }
}

/// Resolve the left- and right-hand-side nodes of a `binary_operator`,
/// `namespace_operator`, or `extract_operator`. Tree-sitter-r exposes both as
/// the `lhs` and `rhs` fields, matching the pattern used elsewhere in the
/// codebase (`object_name.rs`, `extract_op.rs`, `qualified_resolve.rs`).
fn lhs_and_rhs<'tree>(node: Node<'tree>) -> Option<(Node<'tree>, Node<'tree>)> {
    let lhs = node.child_by_field_name("lhs")?;
    let rhs = node.child_by_field_name("rhs")?;
    Some((lhs, rhs))
}

#[allow(clippy::too_many_arguments)]
fn report(
    text: &str,
    op: Node<'_>,
    prefix: &str,
    op_text: &str,
    suffix: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let line_no = op.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::INFIX_SPACES) {
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
        code: Some(NumberOrString::String(rule_ids::INFIX_SPACES.to_string())),
        message: format!("{prefix}{op_text}{suffix}."),
        ..Default::default()
    });
}
