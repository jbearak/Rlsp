//! Flag lines wider than the configured maximum.
//!
//! Width is measured in characters (Unicode scalar values), matching lintr's
//! `nchar()` — an emoji counts as 1 even though it spans two UTF-16 units in
//! LSP positions. Tabs
//! count as one unit, matching `lintr::line_length_linter`'s convention.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

use crate::linting::LINT_SOURCE;
use crate::linting::nolint::Suppressions;
use crate::linting::rule_ids;

pub(crate) fn collect(
    text: &str,
    max_len: u32,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx as u32;
        if suppressions.is_suppressed_code(line_no, rule_ids::LINE_LENGTH) {
            continue;
        }
        // Don't count a raw leading U+FEFF toward the width (see
        // `strip_leading_bom_for_scan`). Only line 0 can carry a BOM; a later
        // U+FEFF is a zero-width no-break space that must still count, so the
        // guard is load-bearing, not defensive. Issue #346.
        let line = if line_no == 0 {
            crate::utf16::strip_leading_bom_for_scan(line)
        } else {
            line
        };
        let width: u32 = line.chars().count() as u32;
        if width <= max_len {
            continue;
        }
        out.push(Diagnostic {
            range: Range {
                start: Position::new(line_no, max_len),
                end: Position::new(line_no, width),
            },
            severity: Some(severity),
            source: Some(LINT_SOURCE.to_string()),
            code: Some(NumberOrString::String(rule_ids::LINE_LENGTH.to_string())),
            message: format!("Line is {width} characters long; limit is {max_len}."),
            ..Default::default()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(text: &str, max_len: u32) -> Vec<u32> {
        let suppressions = Suppressions::from_text(text);
        let mut out = Vec::new();
        collect(
            text,
            max_len,
            DiagnosticSeverity::WARNING,
            &suppressions,
            &mut out,
        );
        out.into_iter().map(|d| d.range.end.character).collect()
    }

    #[test]
    fn flags_line_over_limit() {
        // "abcde" is 5 chars; limit 4 → flagged with width 5.
        assert_eq!(widths("abcde\n", 4), vec![5]);
    }

    #[test]
    fn line_exactly_at_limit_is_not_flagged() {
        assert!(widths("abcd\n", 4).is_empty());
    }

    // Issue #346: a raw leading U+FEFF on the first line must not count toward
    // the measured width. tree-sitter and disk reads (decode_source) drop the
    // BOM, so an in-memory line of exactly the limit must not be flagged just
    // because a non-VS-Code client left the BOM in the buffer.
    #[test]
    fn leading_bom_does_not_inflate_first_line_width() {
        assert!(widths("\u{FEFF}abcd\n", 4).is_empty());
    }

    #[test]
    fn leading_bom_first_line_over_limit_reports_bomless_width() {
        // "abcde" past the BOM is 5 chars; the reported width excludes the BOM.
        assert_eq!(widths("\u{FEFF}abcde\n", 4), vec![5]);
    }
}
