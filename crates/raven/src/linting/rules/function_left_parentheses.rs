//! Flag whitespace between a function and its `(` — in definitions and calls.
//!
//! Mirrors `lintr::function_left_parentheses_linter`:
//!
//! * Definitions: `function (x)` / `\ (x)` are valid R but the tight form is
//!   the convention. Tree-sitter-r exposes both as `function_definition`
//!   with a `name` field (the `function` keyword or `\`) and `parameters`.
//! * Calls: `blah (1)`, `base::print (x)`, `` `+` (1, 1) ``, and `x$foo (1)`
//!   have stray whitespace between the callee and the argument list; a
//!   newline gets the "left parenthesis should be on the same line as the
//!   function's symbol" message. Matching lintr, only symbol-like callees
//!   are checked: identifiers, `::`/`:::`-qualified names, and `$` access;
//!   `@` slot access gets only the cross-line check. String "callees"
//!   (`"print"(x)`, `base::"mean"(x)`) and computed callees — IIFEs
//!   `(function() 1)()`, chained `f(x)(y)` — are left alone.

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
    if node.kind() == "function_definition" {
        check_definition(node, text, severity, suppressions, out);
    }
    if node.kind() == "call" {
        check_call(node, text, severity, suppressions, out);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

/// Which checks apply to a call's function node. lintr's same-line-spaces
/// XPath covers SYMBOL_FUNCTION_CALL and `$` access, while its wrong-line
/// XPath additionally covers `@` slot access; string callees (bare or via
/// `::`) and computed callees get neither.
enum CalleeChecks {
    None,
    /// Same-line whitespace and cross-line `(` both flagged.
    Full,
    /// Only a `(` on a later line is flagged (`@` slot calls).
    CrossLineOnly,
}

fn callee_checks(function: Node<'_>, text: &str) -> CalleeChecks {
    match function.kind() {
        "identifier" => CalleeChecks::Full,
        // `pkg::name(...)` — but `pkg::"name"(...)` is a string callee.
        "namespace_operator" => match function.child_by_field_name("rhs") {
            Some(rhs) if rhs.kind() == "identifier" => CalleeChecks::Full,
            _ => CalleeChecks::None,
        },
        "extract_operator" => {
            // A string member callee (`obj$"foo"(1)`) is exempt like any
            // other string callee.
            if function
                .child_by_field_name("rhs")
                .is_none_or(|rhs| rhs.kind() != "identifier")
            {
                return CalleeChecks::None;
            }
            match function
                .child_by_field_name("operator")
                .and_then(|op| text.get(op.start_byte()..op.end_byte()))
            {
                Some("$") => CalleeChecks::Full,
                Some("@") => CalleeChecks::CrossLineOnly,
                _ => CalleeChecks::None,
            }
        }
        _ => CalleeChecks::None,
    }
}

fn check_call(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let Some(args) = node.child_by_field_name("arguments") else {
        return;
    };
    let checks = callee_checks(function, text);
    if matches!(checks, CalleeChecks::None) {
        return;
    }
    // `@` slot calls: lintr only flags a `(` on a later line, not same-line
    // whitespace.
    if matches!(checks, CalleeChecks::CrossLineOnly)
        && function.end_position().row == args.start_position().row
    {
        return;
    }
    emit_gap_between(
        node,
        function,
        args,
        "Remove whitespace between a function's name and its `(`.",
        "A function call's `(` should be on the same line as the function's name.",
        text,
        severity,
        suppressions,
        out,
    );
}

fn check_definition(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let Some(params) = node.child_by_field_name("parameters") else {
        return;
    };
    let keyword = text
        .get(name.start_byte()..name.end_byte())
        .unwrap_or("function");
    let same_line = format!("Remove whitespace between `{keyword}` and `(`.");
    let cross_line = format!("The `(` should be on the same line as `{keyword}`.");
    emit_gap_between(
        node,
        name,
        params,
        &same_line,
        &cross_line,
        text,
        severity,
        suppressions,
        out,
    );
}

/// Flag the whitespace gap between `left` (callee / keyword) and `right`
/// (the `(...)` node), choosing the same-line or cross-line message.
// One flat leaf helper shared by the call and definition checks; a params
// struct would just re-window the same values.
#[allow(clippy::too_many_arguments)]
fn emit_gap_between(
    _node: Node<'_>,
    name: Node<'_>,
    params: Node<'_>,
    same_line_message: &str,
    cross_line_message: &str,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let gap_start = name.end_byte();
    let gap_end = params.start_byte();
    if gap_end <= gap_start {
        return;
    }
    let gap = match text.get(gap_start..gap_end) {
        Some(s) => s,
        None => return,
    };
    if gap.is_empty() {
        return;
    }
    // Any whitespace at all (spaces, tabs, or even newlines) is reported —
    // the rule wants tight `function(`. A gap may also carry comments
    // (`foo # note\n(x)` is still a wrong-line call in lintr); anything else
    // in the gap would be a parse anomaly we shouldn't pretend to handle.
    let gap_is_whitespace_and_comments = gap
        .lines()
        .all(|line| line.trim_start().is_empty() || line.trim_start().starts_with('#'));
    if !gap_is_whitespace_and_comments {
        return;
    }
    let line_no = name.end_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::FUNCTION_LEFT_PARENTHESES) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, name.end_position().column);
    let end_line = params.start_position().row as u32;
    let end_line_text = text.lines().nth(end_line as usize).unwrap_or("");
    let end_col = byte_offset_to_utf16_column(end_line_text, params.start_position().column);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(end_line, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(
            rule_ids::FUNCTION_LEFT_PARENTHESES.to_string(),
        )),
        message: if line_no == end_line {
            same_line_message.to_string()
        } else {
            cross_line_message.to_string()
        },
        ..Default::default()
    });
}
