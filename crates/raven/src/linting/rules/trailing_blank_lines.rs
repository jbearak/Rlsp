//! Flag trailing blank lines at end of file.
//!
//! lintr also flags a missing terminal newline. We match that: a file that
//! doesn't end with `\n` is reported, and so is one with one-or-more blank
//! lines after the last non-blank line.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

use crate::linting::LINT_SOURCE;
use crate::linting::nolint::Suppressions;
use crate::linting::rule_ids;

pub(crate) fn collect(
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    if text.is_empty() {
        return;
    }

    // Detect a missing trailing newline.
    if !text.ends_with('\n') {
        let last_line_idx = text.lines().count().saturating_sub(1) as u32;
        if !suppressions.is_suppressed_code(last_line_idx, rule_ids::TRAILING_BLANK_LINES) {
            let last = text.lines().next_back().unwrap_or("");
            let col: u32 = last.chars().map(|c| c.len_utf16() as u32).sum();
            out.push(Diagnostic {
                range: Range {
                    start: Position::new(last_line_idx, col),
                    end: Position::new(last_line_idx, col),
                },
                severity: Some(severity),
                source: Some(LINT_SOURCE.to_string()),
                code: Some(NumberOrString::String(
                    rule_ids::TRAILING_BLANK_LINES.to_string(),
                )),
                message: "File should end with a newline.".to_string(),
                ..Default::default()
            });
        }
        // Fall through: a file can be missing its terminal newline AND have
        // trailing blank lines (e.g. `x <- 1\n\n `) — lintr reports both.
    }

    // Count trailing blank lines.
    let lines: Vec<&str> = text.lines().collect();
    let mut trailing = 0usize;
    for line in lines.iter().rev() {
        if line.trim().is_empty() {
            trailing += 1;
        } else {
            break;
        }
    }
    if trailing == 0 {
        return;
    }
    let first_blank = lines.len() - trailing;
    let line_no = first_blank as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::TRAILING_BLANK_LINES) {
        return;
    }
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, 0),
            end: Position::new(lines.len() as u32, 0),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(
            rule_ids::TRAILING_BLANK_LINES.to_string(),
        )),
        message: if trailing == 1 {
            "Trailing blank line at end of file.".to_string()
        } else {
            format!("{trailing} trailing blank lines at end of file.")
        },
        ..Default::default()
    });
}
