//! Flag trailing spaces/tabs at end of line.
//!
//! Mirrors `lintr::trailing_whitespace_linter` defaults
//! (`allow_empty_lines = FALSE`, `allow_in_strings = TRUE`): whitespace-only
//! lines are flagged, but trailing whitespace that lies *inside* a string
//! literal (e.g. the interior lines of a multi-line string) is not — that
//! whitespace is part of the string's value.

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
        // `lines()` strips the terminator; step past `\n` (and a `\r` that
        // `lines()` also strips) when computing the next line's offset.
        let next_line_start =
            line_start + line.len() + terminator_len(text, line_start + line.len());
        if suppressions.is_suppressed_code(line_no, rule_ids::TRAILING_WHITESPACE) {
            line_start = next_line_start;
            continue;
        }
        let trimmed = line.trim_end_matches([' ', '\t']);
        if trimmed.len() == line.len() {
            line_start = next_line_start;
            continue;
        }
        // lintr's `allow_in_strings = TRUE` default: skip when the trailing
        // run is inside a string literal.
        let ws_start_byte = line_start + trimmed.len();
        if super::inside_any_range(&string_ranges, ws_start_byte) {
            line_start = next_line_start;
            continue;
        }
        let start_col = byte_offset_to_utf16_column(line, trimmed.len());
        let end_col = byte_offset_to_utf16_column(line, line.len());
        out.push(Diagnostic {
            range: Range {
                start: Position::new(line_no, start_col),
                end: Position::new(line_no, end_col),
            },
            severity: Some(severity),
            source: Some(LINT_SOURCE.to_string()),
            code: Some(NumberOrString::String(
                rule_ids::TRAILING_WHITESPACE.to_string(),
            )),
            message: "Trailing whitespace.".to_string(),
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
