//! Flag whitespace immediately inside `(...)`, `[...]`, `[[...]]` groupings.
//!
//! Mirrors `lintr::spaces_inside_linter`. `f( x )`, `df[ 1 ]`, `mat[[ i ]]`
//! all have stray space against the brackets. The community convention is
//! tight: `f(x)`, `df[1]`, `mat[[i]]`. Truly empty groupings (`f()`, `mat[]`)
//! pass, while whitespace-only single-line groupings (`f( )`, `mat[ ]`) are
//! flagged on both sides, matching lintr.
//! For `subset` and `subset2` only, a trailing comma that marks an omitted
//! dimension keeps its before-close space (`x[i, ]`, `x[[i, ]]`) so this rule
//! does not conflict with `commas_linter`; `call` nodes such as `f(a, )` are
//! intentionally not carved out, keeping the exception scoped to R subsetting.
//!
//! Applies to calls, subsetting, parenthesized expressions, formal parameter
//! lists, and control-flow conditions. The `open` and `close` field positions
//! anchor the check.

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
    match node.kind() {
        "call" => {
            if let Some(args) = node.child_by_field_name("arguments") {
                check_bracketed(args, false, text, severity, suppressions, out);
            }
        }
        "subset" | "subset2" => {
            if let Some(args) = node.child_by_field_name("arguments") {
                check_bracketed(args, true, text, severity, suppressions, out);
            }
        }
        "parenthesized_expression" => {
            check_bracketed(node, false, text, severity, suppressions, out);
        }
        "function_definition" => {
            if let Some(parameters) = node.child_by_field_name("parameters") {
                check_bracketed(parameters, false, text, severity, suppressions, out);
            }
        }
        "if_statement" | "while_statement" | "for_statement" => {
            check_bracketed(node, false, text, severity, suppressions, out);
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

fn check_bracketed(
    node: Node<'_>,
    is_subset: bool,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(open) = node.child_by_field_name("open") else {
        return;
    };
    let Some(close) = node.child_by_field_name("close") else {
        return;
    };
    let open_end = open.end_byte();
    let close_start = close.start_byte();
    if close_start <= open_end {
        return;
    }
    // A truly empty grouping is allowed. A whitespace-only single-line
    // grouping is not: lintr emits both after-open and before-close lints.
    let interior = match text.get(open_end..close_start) {
        Some(s) => s,
        None => return,
    };
    if interior.is_empty() {
        return;
    }
    if interior.chars().all(|c| c.is_whitespace()) {
        if !interior.contains('\n') {
            let open_text = text.get(open.start_byte()..open.end_byte()).unwrap_or("");
            emit_after_open(
                open,
                open_text,
                interior.len(),
                text,
                severity,
                suppressions,
                out,
            );
            let close_text = text.get(close.start_byte()..close.end_byte()).unwrap_or("");
            emit_before_close(
                close,
                close_text,
                interior.len(),
                text,
                severity,
                suppressions,
                out,
            );
        }
        return;
    }

    // Find the byte offset of the first non-whitespace character (anchor for
    // the "after open" check) and the last non-whitespace character (anchor
    // for the "before close" check).
    let first_non_ws_rel = interior
        .char_indices()
        .find(|&(_, c)| !c.is_whitespace())
        .map(|(i, _)| i);
    let last_non_ws_rel = interior
        .char_indices()
        .rev()
        .find(|&(_, c)| !c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8());

    if let Some(first_rel) = first_non_ws_rel {
        let after_open = &interior[..first_rel];
        if !after_open.is_empty() && !after_open.contains('\n') {
            // Open bracket has *single-line* whitespace before the first
            // real token — that's the violation. Multi-line wrapping is fine.
            let open_text = text.get(open.start_byte()..open.end_byte()).unwrap_or("");
            emit_after_open(
                open,
                open_text,
                after_open.len(),
                text,
                severity,
                suppressions,
                out,
            );
        }
    }
    if let Some(last_rel) = last_non_ws_rel {
        let before_close = &interior[last_rel..];
        if !before_close.is_empty() && !before_close.contains('\n') {
            if is_subset
                && close
                    .prev_sibling()
                    .is_some_and(|prev| prev.kind() == "comma")
            {
                // Accept canonical omitted-dimension subsets like `x[i, ]` so
                // `spaces_inside` does not conflict with `commas_linter`,
                // which flags the alternative `x[i,]`. This applies only to
                // subset/subset2 nodes when the last token before `]` is a
                // comma. Comment-before-`]` cases such as `x[i, # c\n]` stay
                // safe because the pre-existing newline guard above suppresses
                // them, so that guard must not be removed.
                return;
            }
            let close_text = text.get(close.start_byte()..close.end_byte()).unwrap_or("");
            emit_before_close(
                close,
                close_text,
                before_close.len(),
                text,
                severity,
                suppressions,
                out,
            );
        }
    }
}

fn emit_after_open(
    open: Node<'_>,
    open_text: &str,
    ws_len: usize,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let line_no = open.end_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::SPACES_INSIDE) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, open.end_position().column);
    let end_col = byte_offset_to_utf16_column(line_text, open.end_position().column + ws_len);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(line_no, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::SPACES_INSIDE.to_string())),
        message: format!("Remove whitespace after `{open_text}`."),
        ..Default::default()
    });
}

fn emit_before_close(
    close: Node<'_>,
    close_text: &str,
    ws_len: usize,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let line_no = close.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::SPACES_INSIDE) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let close_col = close.start_position().column;
    let start_col_bytes = close_col.saturating_sub(ws_len);
    let start_col = byte_offset_to_utf16_column(line_text, start_col_bytes);
    let end_col = byte_offset_to_utf16_column(line_text, close_col);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(line_no, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::SPACES_INSIDE.to_string())),
        message: format!("Remove whitespace before `{close_text}`."),
        ..Default::default()
    });
}
