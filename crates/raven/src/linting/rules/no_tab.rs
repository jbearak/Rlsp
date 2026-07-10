//! Flag tab characters used for indentation.
//!
//! Mirrors the no-tab portion of `lintr::whitespace_linter()`: tabs after the
//! first non-whitespace character (for example in comments or string content)
//! are not indentation and are ignored. Reports one diagnostic per affected
//! line, covering the first contiguous run of indentation tabs.

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
    let mut string_interior_lines = HashSet::new();
    collect_string_interior_lines(root, &mut string_interior_lines);

    for (idx, line) in text.lines().enumerate() {
        let line_no = idx as u32;
        if suppressions.is_suppressed_code(line_no, rule_ids::NO_TAB) {
            continue;
        }
        if string_interior_lines.contains(&line_no) {
            continue;
        }
        let bytes = line.as_bytes();
        let leading_len = bytes
            .iter()
            .take_while(|&&byte| byte == b' ' || byte == b'\t')
            .count();
        let Some(start) = bytes[..leading_len].iter().position(|&b| b == b'\t') else {
            continue;
        };
        let mut end = start;
        while end < bytes.len() && bytes[end] == b'\t' {
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
    }
}

fn collect_string_interior_lines(node: Node<'_>, out: &mut HashSet<u32>) {
    if node.kind() == "string" {
        let start = node.start_position().row as u32;
        let end = node.end_position().row as u32;
        for line in (start + 1)..=end {
            out.insert(line);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_string_interior_lines(child, out);
    }
}
