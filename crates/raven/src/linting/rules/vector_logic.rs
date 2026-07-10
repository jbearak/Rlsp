//! Flag `&` / `|` in `if` / `while` conditions, where `&&` / `||` is expected.
//!
//! Mirrors `lintr::vector_logic_linter`. `if (x & y)` triggers a warning in
//! R 4.3+ (`condition has length > 1`) and silently does the wrong thing on
//! older R when either side returns a vector. Scalar short-circuit operators
//! (`&&` / `||`) are the correct choice in scalar contexts.
//!
//! The scan walks the condition expression of each `if_statement` /
//! `while_statement` (and of `expect_true()` / `expect_false()`, which
//! testthat evaluates as scalar conditions) and reports every `&` / `|`
//! operator inside, recursively. Function-call boundaries stop the
//! recursion: `if (any(x & y))` is fine because the `&` is evaluated inside
//! `any()` on a vector, not on the condition itself. lintr applies the same
//! carve-out. An operator with a string-literal or
//! `as.raw()`/`as.octmode()`/`as.hexmode()` operand is skipped — that is
//! bitwise arithmetic, not boolean logic (`if (info & as.raw(12еко))` is fine;
//! lintr has the same exemption).
//!
//! The mirror half: `&&` / `||` inside `subset()` / `filter()` arguments
//! (bare, `pkg::`-qualified except `stats::filter`, or as a pipe RHS) are
//! flagged the other way — subsetting is a vector context, so the scalar
//! operators are wrong there. Nested function definitions inside those
//! arguments are skipped (matching lintr's development branch; lintr 3.3.0
//! flagged them).

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
        match callee_name(node, text) {
            // testthat conditions behave like `if`/`while` conditions.
            Some("expect_true") | Some("expect_false") => {
                if let Some(first) = first_argument_value(node) {
                    scan_condition(first, text, severity, suppressions, out);
                }
            }
            // Subsetting contexts: scalar `&&`/`||` are wrong. `stats::filter`
            // is linear filtering, not subsetting — lintr exempts it too.
            Some("filter") | Some("subset") if !is_stats_qualified(node, text) => {
                if let Some(args) = node.child_by_field_name("arguments") {
                    scan_subset_args(args, text, severity, suppressions, out);
                }
            }
            _ => {}
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

/// The called function's bare name: `filter` for both `filter(...)` and
/// `dplyr::filter(...)`.
fn callee_name<'a>(call: Node<'_>, text: &'a str) -> Option<&'a str> {
    let function = call.child_by_field_name("function")?;
    match function.kind() {
        "identifier" => text.get(function.start_byte()..function.end_byte()),
        "namespace_operator" => {
            let rhs = function.child_by_field_name("rhs")?;
            text.get(rhs.start_byte()..rhs.end_byte())
        }
        _ => None,
    }
}

/// True for `stats::filter(...)` / `stats:::filter(...)`.
fn is_stats_qualified(call: Node<'_>, text: &str) -> bool {
    call.child_by_field_name("function")
        .filter(|f| f.kind() == "namespace_operator")
        .and_then(|f| f.child_by_field_name("lhs"))
        .and_then(|lhs| text.get(lhs.start_byte()..lhs.end_byte()))
        == Some("stats")
}

/// The value of the call's first argument, if any.
fn first_argument_value<'t>(call: Node<'t>) -> Option<Node<'t>> {
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let first = args
        .children(&mut cursor)
        .find(|child| child.kind() == "argument")?;
    first.child_by_field_name("value")
}

/// Flag scalar `&&` / `||` inside `subset()` / `filter()` arguments. Recurses
/// through nested expressions but not into function definitions (a lambda's
/// body is its own scope, not part of the subsetting expression).
fn scan_subset_args(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    if node.kind() == "function_definition" {
        return;
    }
    // A nested `subset()`/`filter()` call is its own subsetting context and
    // gets scanned when the outer AST walk reaches it — descending into it
    // here would emit duplicate diagnostics (`filter(x, subset(y, a && b))`
    // is one lint in lintr, not two).
    if node.kind() == "call"
        && matches!(callee_name(node, text), Some("filter") | Some("subset"))
        && !is_stats_qualified(node, text)
    {
        return;
    }
    if node.kind() == "binary_operator"
        && let Some(op) = node.child_by_field_name("operator")
    {
        let op_text = text.get(op.start_byte()..op.end_byte()).unwrap_or("");
        if op_text == "&&" || op_text == "||" {
            let preferred = if op_text == "&&" { "&" } else { "|" };
            let message = format!(
                "Use `{preferred}` in subsetting expressions; `{op_text}` is the scalar form."
            );
            emit(op, &message, text, severity, suppressions, out);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_subset_args(child, text, severity, suppressions, out);
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
        if (op_text == "&" || op_text == "|") && !has_bitwise_operand(node, text) {
            let preferred = if op_text == "&" { "&&" } else { "||" };
            let message = format!(
                "Use `{preferred}` in `if` / `while` conditions; `{op_text}` is the vectorised form."
            );
            emit(op, &message, text, severity, suppressions, out);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_condition(child, text, severity, suppressions, out);
    }
}

/// True when either direct operand marks bitwise arithmetic: a string
/// literal (`info & "111"`) or a call to `as.raw` / `as.octmode` /
/// `as.hexmode`. lintr exempts these from the condition check.
fn has_bitwise_operand(binop: Node<'_>, text: &str) -> bool {
    ["lhs", "rhs"].iter().any(|field| {
        binop.child_by_field_name(field).is_some_and(|operand| {
            operand.kind() == "string"
                || (operand.kind() == "call"
                    && matches!(
                        callee_name(operand, text),
                        Some("as.raw") | Some("as.octmode") | Some("as.hexmode")
                    ))
        })
    })
}

fn emit(
    op: Node<'_>,
    message: &str,
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
        message: message.to_string(),
        ..Default::default()
    });
}
