//! Flag trailing spaces/tabs at end of line.
//!
//! Matching `lintr::trailing_whitespace_linter()` defaults, whitespace at a
//! line ending that is inside a multi-line string is allowed. Whitespace after
//! the string's closing delimiter is still checked normally.

use std::collections::HashSet;

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
    let mut string_continuation_lines = HashSet::new();
    collect_string_continuation_lines(root, &mut string_continuation_lines);

    for (idx, line) in text.lines().enumerate() {
        let line_no = idx as u32;
        if suppressions.is_suppressed_code(line_no, rule_ids::TRAILING_WHITESPACE) {
            continue;
        }
        let trimmed = line.trim_end_matches([' ', '\t']);
        if trimmed.len() == line.len() {
            continue;
        }
        if string_continuation_lines.contains(&line_no) {
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
    }
}

/// Record rows whose terminating newline occurs inside a multi-line string.
/// For a string spanning rows `start..=end`, those are `start..end`; the end
/// row is deliberately excluded because any whitespace after the closing
/// delimiter belongs to source, not the string.
fn collect_string_continuation_lines(node: Node<'_>, out: &mut HashSet<u32>) {
    if node.kind() == "string" {
        let start = node.start_position().row as u32;
        let end = node.end_position().row as u32;
        for line in start..end {
            out.insert(line);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_string_continuation_lines(child, out);
    }
}
