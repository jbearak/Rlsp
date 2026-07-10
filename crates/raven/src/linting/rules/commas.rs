//! Enforce conventional spacing around `,` separators.
//!
//! Mirrors `lintr::commas_linter` defaults: every comma must be tight on the
//! left (no whitespace before) and must be followed by whitespace (space, tab,
//! or newline). Tree-sitter-r exposes commas as named `comma` children of
//! `arguments` and `parameters` nodes, so the rule walks those parents and
//! inspects each comma's neighbours in the raw text.

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
    if node.kind() == "comma" {
        check_comma(node, text, severity, suppressions, out);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

fn check_comma(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let bytes = text.as_bytes();
    let start = node.start_byte();
    let end = node.end_byte();

    // Space before comma: look at the byte immediately before. lintr's
    // exemptions (all verified against lintr 3.3.0.1):
    // * The check only applies when the preceding token sits on the *same
    //   line* — a comma that starts its own line (leading-comma continuation
    //   style, `fun(1\n  , 2)`) is exempt at any indentation.
    // * A preceding comma is exempt: `a[1, , 2]` marks a missing argument.
    // * A preceding `=` is exempt: `switch(op, x = , y = bar)`.
    if start > 0 {
        let prev = bytes[start - 1];
        if (prev == b' ' || prev == b'\t')
            && !is_first_token_on_line(text, node)
            && !prev_token_exempts_space_before(node)
        {
            emit(
                node,
                text,
                severity,
                suppressions,
                "Unexpected whitespace before `,`.",
                out,
            );
        }
    }

    // Missing space after comma: the next byte (if any) must be whitespace or
    // newline. lintr's default `allow_trailing = FALSE` also flags a comma
    // followed by a closing bracket (`a[1,]`), so we don't carve that out.
    if end < bytes.len() {
        let next = bytes[end];
        let is_ws = matches!(next, b' ' | b'\t' | b'\n' | b'\r');
        if !is_ws {
            emit(
                node,
                text,
                severity,
                suppressions,
                "Missing space after `,`.",
                out,
            );
        }
    }
}

/// True when only whitespace precedes `node` on its line.
fn is_first_token_on_line(text: &str, node: Node<'_>) -> bool {
    let line_no = node.start_position().row;
    let col = node.start_position().column;
    text.lines()
        .nth(line_no)
        .and_then(|line| line.get(..col))
        .is_some_and(|prefix| prefix.bytes().all(|b| b == b' ' || b == b'\t'))
}

/// True when the token before the comma is another comma or a value-less
/// named argument's `=` — lintr exempts whitespace-before-comma in both.
fn prev_token_exempts_space_before(node: Node<'_>) -> bool {
    let Some(prev) = node.prev_sibling() else {
        return false;
    };
    match prev.kind() {
        "comma" => true,
        // `switch(op, x = , y = bar)` — the missing-value argument node ends
        // with its `=` token.
        "argument" => prev
            .child(prev.child_count().saturating_sub(1) as u32)
            .is_some_and(|last| last.kind() == "="),
        _ => false,
    }
}

fn emit(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    message: &str,
    out: &mut Vec<Diagnostic>,
) {
    let line_no = node.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::COMMAS) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, node.start_position().column);
    let end_col = byte_offset_to_utf16_column(line_text, node.end_position().column);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(node.end_position().row as u32, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::COMMAS.to_string())),
        message: message.to_string(),
        ..Default::default()
    });
}
