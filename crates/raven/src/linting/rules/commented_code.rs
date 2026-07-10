//! Flag comments that appear to contain commented-out R code.
//!
//! Mirrors `lintr::commented_code_linter`. A comment is flagged when:
//!
//! 1. After stripping the leading `#` characters and whitespace, the remaining
//!    text parses cleanly as R (no `ERROR` nodes) and contains at least one
//!    "code-like" construct — a call, an assignment, a binary or unary
//!    operator, or a function definition. Bare identifiers, literals, and
//!    parse errors are treated as prose.
//!
//! Multi-line comment blocks (consecutive standalone comment lines) are
//! grouped and parsed as a unit, so commented-out code that spans several
//! lines is detected even when no single line is by itself a valid expression.
//!
//! The following are always skipped:
//!
//! * **Roxygen** lines (`#'`). Documentation, not code.
//! * **Shebangs** (`#!/usr/bin/env Rscript`) on the first line.
//! * **Annotation comments** prefixed with one of `TODO`, `FIXME`, `NOTE`,
//!   `XXX`, `HACK`, `BUG`, `WARNING`, or `OPTIMIZE` (case-insensitive,
//!   followed by `:`, `(`, or `-`). `TODO` happens to be a valid identifier,
//!   so without this gate `# TODO: rewrite parse_args` would parse as code.
//! * **Suppression / directive markers** — `# nolint`, `# nolint start`,
//!   `# nolint end`, `# nolint: rule`, `# raven: ignore`, `# raven: ignore-next`,
//!   and any other `# raven:` directive (the `# @lsp-…` forms are permanent
//!   aliases that parse identically). These wouldn't pass the code-like
//!   heuristic anyway, but skipping them up front keeps the rule from
//!   competing with the suppression infrastructure.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::linting::LINT_SOURCE;
use crate::linting::nolint::Suppressions;
use crate::linting::parse_gate::looks_like_code;
use crate::linting::rule_ids;
use crate::utf16::{byte_offset_to_utf16_column, strip_leading_bom_for_scan};

/// Annotation prefixes (case-insensitive). `TODO`, `FIXME`, etc. — these are
/// almost always prose even though `TODO` happens to be a syntactically valid
/// R identifier.
const ANNOTATION_PREFIXES: &[&str] = &[
    "TODO", "FIXME", "NOTE", "XXX", "HACK", "BUG", "WARNING", "OPTIMIZE",
];

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let lines: Vec<&str> = text.lines().collect();

    // Collect every standalone comment node — comments where the only thing
    // preceding the `#` on its line is whitespace. End-of-line comments next
    // to code (`x <- 1 # explain`) are intentionally left alone.
    let mut standalone: Vec<StandaloneComment> = Vec::new();
    let mut inline: Vec<InlineComment> = Vec::new();
    collect_comments(root, &lines, &mut standalone, &mut inline);

    // lintr also checks code-shaped end-of-line comments. Ordinary prose
    // annotations still fail `looks_like_code` and remain silent.
    for comment in inline {
        let line_text = lines.get(comment.line as usize).copied().unwrap_or("");
        let comment_text = line_text.get(comment.start_byte_on_line..).unwrap_or("");
        if is_skip_line(comment_text, comment.line == 0)
            || suppressions.is_suppressed_code(comment.line, rule_ids::COMMENTED_CODE)
        {
            continue;
        }
        let stripped = strip_and_join(&[comment_text]);
        if !looks_like_code(&stripped) {
            continue;
        }
        let start_col = byte_offset_to_utf16_column(line_text, comment.start_byte_on_line);
        let end_col = byte_offset_to_utf16_column(line_text, line_text.len());
        out.push(Diagnostic {
            range: Range {
                start: Position::new(comment.line, start_col),
                end: Position::new(comment.line, end_col),
            },
            severity: Some(severity),
            source: Some(LINT_SOURCE.to_string()),
            code: Some(NumberOrString::String(rule_ids::COMMENTED_CODE.to_string())),
            message: "Commented code should be removed.".to_string(),
            ..Default::default()
        });
    }

    // Group consecutive standalone comments (adjacent lines, nothing else in
    // between), then *split* each group on any line that is itself a skip
    // marker (roxygen, shebang on line 0, directive, annotation, mode line)
    // or that the suppression infrastructure has marked. Splitting (rather
    // than discarding the whole group) keeps unrelated commented-out code on
    // adjacent lines from being silently swallowed by a single nearby
    // directive — e.g. `# raven: ignore-next\n# x <- 1\n# y <- 2` must still
    // flag line 2.
    let groups = group_contiguous(&standalone);

    for raw_group in groups {
        for sub in split_on_skip_lines(&raw_group, &lines, suppressions) {
            let first = sub.first().expect("sub-group is non-empty");
            let last = sub.last().expect("sub-group is non-empty");

            let sub_lines: Vec<&str> = sub
                .iter()
                .map(|c| lines.get(c.line as usize).copied().unwrap_or(""))
                .collect();

            let stripped = strip_and_join(&sub_lines);
            if !looks_like_code(&stripped) {
                continue;
            }

            let start_line_text = lines.get(first.line as usize).copied().unwrap_or("");
            let end_line_text = lines.get(last.line as usize).copied().unwrap_or("");
            let start_col = byte_offset_to_utf16_column(start_line_text, first.start_byte_on_line);
            let end_col = byte_offset_to_utf16_column(end_line_text, end_line_text.len());

            out.push(Diagnostic {
                range: Range {
                    start: Position::new(first.line, start_col),
                    end: Position::new(last.line, end_col),
                },
                severity: Some(severity),
                source: Some(LINT_SOURCE.to_string()),
                code: Some(NumberOrString::String(rule_ids::COMMENTED_CODE.to_string())),
                message: "Commented code should be removed.".to_string(),
                ..Default::default()
            });
        }
    }
}

/// Split a contiguous comment group at every line that is itself a skip
/// marker (roxygen, directive, annotation, mode line, shebang on line 0) or
/// a suppressed line. Returns the remaining runs of standalone comment lines
/// that should be try-parsed as code.
fn split_on_skip_lines<'a>(
    group: &'a [StandaloneComment],
    lines: &[&str],
    suppressions: &Suppressions,
) -> Vec<&'a [StandaloneComment]> {
    let mut out: Vec<&'a [StandaloneComment]> = Vec::new();
    let mut start = 0usize;
    for (idx, c) in group.iter().enumerate() {
        let line_text = lines.get(c.line as usize).copied().unwrap_or("");
        let is_first_line = c.line == 0;
        let skip = is_skip_line(line_text, is_first_line)
            || suppressions.is_suppressed_code(c.line, rule_ids::COMMENTED_CODE);
        if skip {
            if idx > start {
                out.push(&group[start..idx]);
            }
            start = idx + 1;
        }
    }
    if start < group.len() {
        out.push(&group[start..]);
    }
    out
}

/// Comment node that is the only non-whitespace content on its line.
#[derive(Debug, Clone, Copy)]
struct StandaloneComment {
    line: u32,
    /// Byte offset of the `#` within its line (used to build the diagnostic
    /// start position).
    start_byte_on_line: usize,
}

#[derive(Debug, Clone, Copy)]
struct InlineComment {
    line: u32,
    start_byte_on_line: usize,
}

fn collect_comments<'a>(
    node: Node<'a>,
    lines: &[&str],
    standalone: &mut Vec<StandaloneComment>,
    inline: &mut Vec<InlineComment>,
) {
    if node.kind() == "comment" {
        let start = node.start_position();
        let line_idx = start.row;
        // O(1) line lookup against the materialized slice. Comments can be
        // nested anywhere in the tree, so we still walk every node.
        if let Some(line) = lines.get(line_idx).copied() {
            let col_bytes = start.column;
            // tree-sitter counts a leading U+FEFF in its byte column (the BOM is
            // an `extras`/whitespace token), so on a BOM-prefixed first line the
            // prefix before the `#` is the 3-byte BOM. Strip it before the
            // all-whitespace test so a first-line standalone comment is still
            // recognised; `col_bytes` stays tree-sitter-aligned. Issue #346.
            if col_bytes <= line.len()
                && strip_leading_bom_for_scan(&line[..col_bytes])
                    .bytes()
                    .all(|b| b == b' ' || b == b'\t')
            {
                standalone.push(StandaloneComment {
                    line: line_idx as u32,
                    start_byte_on_line: col_bytes,
                });
            } else if col_bytes <= line.len() {
                inline.push(InlineComment {
                    line: line_idx as u32,
                    start_byte_on_line: col_bytes,
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comments(child, lines, standalone, inline);
    }
}

/// Group standalone comments that occupy consecutive lines.
fn group_contiguous(comments: &[StandaloneComment]) -> Vec<Vec<StandaloneComment>> {
    let mut groups: Vec<Vec<StandaloneComment>> = Vec::new();
    for c in comments {
        if let Some(last_group) = groups.last_mut() {
            let prev_line = last_group.last().expect("group is non-empty").line;
            if c.line == prev_line + 1 {
                last_group.push(*c);
                continue;
            }
        }
        groups.push(vec![*c]);
    }
    groups
}

/// Decide whether a single comment line should be excluded from try-parsing.
///
/// Used by [`split_on_skip_lines`] to break a contiguous group at each
/// skip-line boundary, so that one stray `# TODO:` or `# raven: source ...`
/// doesn't silently swallow neighbouring commented-out code.
fn is_skip_line(line: &str, is_first_line_of_file: bool) -> bool {
    // Skip a raw leading U+FEFF so a BOM-prefixed first line is classified the
    // same as a bare one (e.g. a line-0 shebang still counts as a skip). #346.
    let trimmed = strip_leading_bom_for_scan(line).trim_start();
    if is_first_line_of_file && trimmed.starts_with("#!") {
        return true;
    }
    if trimmed.starts_with("#'") {
        return true;
    }
    if is_directive_marker(trimmed) {
        return true;
    }
    if is_mode_line(trimmed) {
        return true;
    }
    is_annotation_comment(trimmed)
}

/// `# nolint`, `# nolint start`, `# nolint end`, `# nolint: rule`, and any
/// `# raven:` directive (`# raven: ignore`, `# raven: source`, `# raven: var`,
/// etc.) — including the permanent `@lsp-` aliases the matcher still accepts
/// (`@lsp-ignore`, `@lsp-source`, `@lsp-var`, etc.).
fn is_directive_marker(trimmed_line: &str) -> bool {
    let body = match strip_hash_prefix(trimmed_line) {
        Some(b) => b,
        None => return false,
    };
    let body_lower = body.to_ascii_lowercase();
    if let Some(after) = body_lower.strip_prefix("nolint") {
        // Accept the bare keyword followed by EOL, whitespace, `:`, or `-`.
        // Same rule used by [`crate::linting::nolint::matches_keyword`].
        return match after.chars().next() {
            None => true,
            Some(c) => c.is_whitespace() || c == ':' || c == '-',
        };
    }
    if body.starts_with("@lsp-") {
        return true;
    }
    // `# raven:` primary suppression namespace (F2), including the next-line
    // forms (`ignore-next` / `expect-next`) that never suppress their own line.
    // Treat the directive line itself as a marker so the `commented_code` rule
    // never parses it as commented-out code or glues it into a comment block.
    if let Some(after) = body_lower.strip_prefix("raven") {
        return after.trim_start().starts_with(':');
    }
    false
}

/// Emacs-style mode line, e.g. `# -*- coding: utf-8 -*-`. R code rarely uses
/// these, but they parse as a chain of unary minuses and `:` and would trip
/// the code-like heuristic.
fn is_mode_line(trimmed_line: &str) -> bool {
    let body = match strip_hash_prefix(trimmed_line) {
        Some(b) => b,
        None => return false,
    };
    let first = body.find("-*-");
    let last = body.rfind("-*-");
    matches!((first, last), (Some(s), Some(e)) if s != e)
}

/// `# TODO:` / `# fixme(...)` and friends. The marker keyword must be followed
/// by `:`, `(`, `-`, or whitespace so we don't false-positive on identifiers
/// that happen to share a prefix (`TODOS`, `FIXMETOO`).
fn is_annotation_comment(trimmed_line: &str) -> bool {
    let body = match strip_hash_prefix(trimmed_line) {
        Some(b) => b,
        None => return false,
    };
    for prefix in ANNOTATION_PREFIXES {
        // `str::get` is char-boundary-safe — a multibyte character whose
        // bytes straddle `prefix.len()` returns `None` instead of panicking
        // like a plain `&body[..prefix.len()]` would.
        let head = match body.get(..prefix.len()) {
            Some(h) => h,
            None => continue,
        };
        if !head.eq_ignore_ascii_case(prefix) {
            continue;
        }
        let tail = &body[prefix.len()..];
        if tail.is_empty() {
            return true;
        }
        if let Some(c) = tail.chars().next()
            && (c.is_whitespace() || c == ':' || c == '(' || c == '-')
        {
            return true;
        }
    }
    false
}

/// Strip leading `#` characters and following whitespace, returning the
/// comment body. Returns `None` if the input is not a comment line.
fn strip_hash_prefix(line: &str) -> Option<&str> {
    // Tolerate a raw leading U+FEFF so a BOM-prefixed first-line comment body is
    // still extracted (the BOM survives into in-memory text — see #346).
    let mut rest = strip_leading_bom_for_scan(line).trim_start();
    if !rest.starts_with('#') {
        return None;
    }
    while let Some(stripped) = rest.strip_prefix('#') {
        rest = stripped;
    }
    Some(rest.trim_start())
}

/// Join the bodies of a comment block (one stripped line per row) into a
/// single string suitable for try-parsing.
fn strip_and_join(lines: &[&str]) -> String {
    let mut out = String::new();
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if let Some(body) = strip_hash_prefix(line) {
            out.push_str(body);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_hash_prefix_handles_double_hash() {
        assert_eq!(strip_hash_prefix("## hello"), Some("hello"));
        assert_eq!(strip_hash_prefix("   # foo"), Some("foo"));
        assert_eq!(strip_hash_prefix("not a comment"), None);
    }

    #[test]
    fn annotation_detector_recognizes_common_prefixes() {
        assert!(is_annotation_comment("# TODO: write tests"));
        assert!(is_annotation_comment("# fixme(123)"));
        assert!(is_annotation_comment("## NOTE - check this"));
        assert!(is_annotation_comment("# XXX"));
        // Not annotations:
        assert!(!is_annotation_comment("# todoist <- list()"));
        assert!(!is_annotation_comment("# x <- 1"));
    }

    #[test]
    fn annotation_detector_safe_on_multibyte_prefix() {
        // `body.get(prefix.len()..)` (boundary-safe) must replace any naked
        // `&body[..prefix.len()]` slice — otherwise comments whose body
        // starts with a multibyte UTF-8 character whose bytes straddle a
        // prefix-length boundary will panic at runtime.
        assert!(!is_annotation_comment("# €€ note"));
        assert!(!is_annotation_comment("# ö€: foo"));
        // Non-ASCII inside the prefix slot must not match an ASCII annotation
        // keyword — and must not panic.
        assert!(!is_annotation_comment("# TÖDO"));
    }

    #[test]
    fn directive_detector_recognizes_nolint_and_lsp() {
        assert!(is_directive_marker("# nolint"));
        assert!(is_directive_marker("# nolint: line_length"));
        assert!(is_directive_marker("# nolint start"));
        assert!(is_directive_marker("# @lsp-ignore"));
        assert!(is_directive_marker("# @lsp-source ../helpers.R"));
        assert!(!is_directive_marker("# nolinter"));
        assert!(!is_directive_marker("# lsp-ignore"));
    }

    #[test]
    fn directive_detector_recognizes_raven_namespace() {
        // `# raven:` suppression directives (incl. next-line forms) are markers,
        // not commented-out code.
        assert!(is_directive_marker("# raven: ignore-next"));
        assert!(is_directive_marker("# raven: expect-next"));
        // The structural families also live under `# raven:` (#421) and must be
        // treated as directive markers, not parsed as commented-out code. These
        // all exercise the one keyword-agnostic `raven:` branch above; they are
        // enumerated as documentation of the families that now apply, not as
        // independent code paths.
        assert!(is_directive_marker("# raven: source ../helpers.R"));
        assert!(is_directive_marker("# raven: sourced-by ../main.R"));
        assert!(is_directive_marker("# raven: cd /data"));
        assert!(is_directive_marker("# raven: var myvar"));
        assert!(is_directive_marker("# raven: func myfunc"));
        // Near-misses must NOT be treated as markers.
        assert!(!is_directive_marker("# raven-ignore-next"));
        assert!(!is_directive_marker("# ravene: ignore-next"));
    }
}
