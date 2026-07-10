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

    // Space before comma: look at the byte immediately before. Only flag a
    // *single-line* space — a comma that is the first non-whitespace token on
    // its line is a multi-line continuation, not a style violation.
    if start > 0 {
        let prev = bytes[start - 1];
        let line_start = text[..start].rfind('\n').map_or(0, |offset| offset + 1);
        let comma_is_first_token = text[line_start..start]
            .bytes()
            .all(|byte| byte == b' ' || byte == b'\t');
        if (prev == b' ' || prev == b'\t') && !comma_is_first_token {
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
