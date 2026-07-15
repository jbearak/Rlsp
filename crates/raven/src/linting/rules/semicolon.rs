//! Flag `;` separators in source.
//!
//! Mirrors `lintr::semicolon_linter`. Tree-sitter-r does not emit `;` as a
//! named or anonymous node — it's simply a separator the parser consumes
//! between statements. To find them we scan the raw source byte-by-byte while
//! using the tree to skip complete string ranges and every other leaf-token
//! range. Strings need explicit whole-node handling because escape sequences
//! are child nodes, leaving the surrounding string content outside the leaf
//! ranges. A separator `;` always lies in the gaps *between* tokens, so any
//! `;` inside an excluded span (a string, comment, backtick-quoted identifier
//! like `` `a;b` ``, or user infix operator like `%;%`) is part of that token,
//! never a separator.
//!
//! Diagnostic anchor: the column of the `;` character itself, on the line it
//! appears on. We emit one diagnostic per `;` so a line with two semicolons
//! produces two.

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
    let exclusions = collect_exclusions(root);
    scan(text, &exclusions, severity, suppressions, out);
}

/// Sorted, non-overlapping byte ranges of complete strings and other leaf
/// tokens — places where a `;` is part of a token (string, comment, backticked
/// identifier, `%…%` operator), not a statement separator.
fn collect_exclusions(root: Node<'_>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort_by_key(|&(s, _)| s);
    out
}

fn walk(node: Node<'_>, out: &mut Vec<(usize, usize)>) {
    // Escapes make `string` a non-leaf node. Exclude the complete literal so
    // the un-noded content around an escape cannot be mistaken for source.
    if node.kind() == "string" {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    if node.child_count() == 0 {
        if node.end_byte() > node.start_byte() {
            out.push((node.start_byte(), node.end_byte()));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, out);
    }
}

fn scan(
    text: &str,
    exclusions: &[(usize, usize)],
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let bytes = text.as_bytes();
    let mut excl_idx = 0;
    let mut line_starts = vec![0usize];
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    for (offset, &b) in bytes.iter().enumerate() {
        if b != b';' {
            continue;
        }
        // Advance past any exclusion ranges ending before this offset.
        while excl_idx < exclusions.len() && exclusions[excl_idx].1 <= offset {
            excl_idx += 1;
        }
        // If this `;` lies inside an exclusion range, skip it.
        if excl_idx < exclusions.len() {
            let (s, e) = exclusions[excl_idx];
            if s <= offset && offset < e {
                continue;
            }
        }
        // Map byte offset → (line, column).
        let line_no = match line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = line_starts[line_no];
        let line_end = if line_no + 1 < line_starts.len() {
            line_starts[line_no + 1] - 1
        } else {
            bytes.len()
        };
        let line_text = text.get(line_start..line_end).unwrap_or("");
        let col_byte = offset - line_start;
        let line_no_u32 = line_no as u32;
        if suppressions.is_suppressed_code(line_no_u32, rule_ids::SEMICOLON) {
            continue;
        }
        let start_col = byte_offset_to_utf16_column(line_text, col_byte);
        let end_col = byte_offset_to_utf16_column(line_text, col_byte + 1);
        out.push(Diagnostic {
            range: Range {
                start: Position::new(line_no_u32, start_col),
                end: Position::new(line_no_u32, end_col),
            },
            severity: Some(severity),
            source: Some(LINT_SOURCE.to_string()),
            code: Some(NumberOrString::String(rule_ids::SEMICOLON.to_string())),
            message: "Put each statement on its own line instead of separating them with `;`."
                .to_string(),
            ..Default::default()
        });
    }
}
