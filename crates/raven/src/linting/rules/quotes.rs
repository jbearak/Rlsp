//! Enforce a single string-literal delimiter (`"` or `'`).
//!
//! Mirrors `lintr::quotes_linter` / `lintr::single_quotes_linter` — the
//! configured delimiter is required for every string literal, *including* raw
//! strings (`R'(...)'` is flagged under the double-quote default, matching
//! lintr). A literal whose source text contains the preferred quote character
//! anywhere (e.g. `'he said "hi"'`, `'\"hi\"'`, `r'(")'`) is exempt — the
//! author could not switch delimiters without escaping, and lintr's regex
//! (`^'([^"]|\\')*'$`) skips exactly those.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::linting::LINT_SOURCE;
use crate::linting::config::StringDelimiter;
use crate::linting::nolint::Suppressions;
use crate::linting::rule_ids;
use crate::utf16::byte_offset_to_utf16_column;

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    delimiter: StringDelimiter,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    visit(root, text, delimiter, severity, suppressions, out);
}

fn visit(
    node: Node<'_>,
    text: &str,
    delimiter: StringDelimiter,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    if node.kind() == "string" {
        check_string(node, text, delimiter, severity, suppressions, out);
        // Strings have no relevant descendants for this rule.
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, delimiter, severity, suppressions, out);
    }
}

fn check_string(
    node: Node<'_>,
    text: &str,
    delimiter: StringDelimiter,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let lit = match text.get(node.start_byte()..node.end_byte()) {
        Some(s) => s,
        None => return,
    };
    let bytes = lit.as_bytes();
    // The delimiter quote is the first byte, or the second for raw strings
    // (`r"(...)"` / `R'(...)'`).
    let quote = match (bytes.first(), bytes.get(1)) {
        (Some(b'r') | Some(b'R'), Some(&q)) if matches!(q, b'"' | b'\'') => q,
        (Some(&q), _) => q,
        (None, _) => return,
    };
    let (wanted, got) = match (delimiter, quote) {
        (StringDelimiter::Double, b'\'') => ('"', '\''),
        (StringDelimiter::Single, b'"') => ('\'', '"'),
        _ => return,
    };
    // Content exemption: if the literal's raw source contains the preferred
    // quote character anywhere, switching delimiters would force escaping —
    // lintr leaves these alone. (The literal's own delimiters are the `got`
    // character, so scanning the whole text is equivalent to scanning the
    // content.)
    if lit.contains(wanted) {
        return;
    }
    let line_no = node.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::QUOTES) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, node.start_position().column);
    // End column is on the start line only if the string is single-line; for
    // multi-line strings the end position spans rows, which the LSP supports
    // natively via `Range::end.line`.
    let end_line = node.end_position().row as u32;
    let end_line_text = text.lines().nth(end_line as usize).unwrap_or("");
    let end_col = byte_offset_to_utf16_column(end_line_text, node.end_position().column);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(end_line, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::QUOTES.to_string())),
        message: format!("String uses `{got}`; configured delimiter is `{wanted}`."),
        ..Default::default()
    });
}
