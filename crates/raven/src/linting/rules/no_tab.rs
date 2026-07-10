//! Flag tab characters used for indentation.
//!
//! Mirrors `lintr::whitespace_linter`, which only lints tabs in a line's
//! *leading* whitespace (`^\s*\t+`) and skips lines that start inside a
//! string literal (a multi-line string's interior is part of its value).
//! Tabs elsewhere on a line — in comments, strings, or between tokens — are
//! not flagged. The diagnostic range covers the contiguous run of tabs so
//! that "fix" actions or selection-aware tooling can target it cleanly.

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
    let string_ranges = super::string_byte_ranges(root);
    let mut line_start = 0usize;
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx as u32;
        let next_line_start =
            line_start + line.len() + terminator_len(text, line_start + line.len());
        if suppressions.is_suppressed_code(line_no, rule_ids::NO_TAB) {
            line_start = next_line_start;
            continue;
        }
        let bytes = line.as_bytes();
        // Leading whitespace only (lintr: `^\s*\t+`).
        let leading_len = bytes
            .iter()
            .position(|&b| b != b' ' && b != b'\t')
            .unwrap_or(bytes.len());
        let Some(start) = bytes[..leading_len].iter().position(|&b| b == b'\t') else {
            line_start = next_line_start;
            continue;
        };
        // A line that begins inside a string literal is part of the string's
        // value, not indentation.
        if super::inside_any_range(&string_ranges, line_start + start) {
            line_start = next_line_start;
            continue;
        }
        let mut end = start;
        while end < leading_len && bytes[end] == b'\t' {
            end += 1;
        }
        let start_col = byte_offset_to_utf16_column(line, start);
        let end_col = byte_offset_to_utf16_column(line, end);
        out.push(Diagnostic {
            range: Range {
                start: Position::new(line_no, start_col),
                end: Position::new(line_no, end_col),
            },
            severity: Some(severity),
            source: Some(LINT_SOURCE.to_string()),
            code: Some(NumberOrString::String(rule_ids::NO_TAB.to_string())),
            message: "Tab character; use spaces.".to_string(),
            ..Default::default()
        });
        line_start = next_line_start;
    }
}

/// Length of the line terminator at `pos` (`\r\n`, `\n`, or end of text).
fn terminator_len(text: &str, pos: usize) -> usize {
    match text.as_bytes().get(pos) {
        Some(b'\r') if text.as_bytes().get(pos + 1) == Some(&b'\n') => 2,
        Some(b'\n') => 1,
        _ => 0,
    }
}
