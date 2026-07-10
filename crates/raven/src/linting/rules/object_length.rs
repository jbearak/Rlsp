//! Flag identifier names longer than the configured maximum.
//!
//! Mirrors `lintr::object_length_linter`. The raw token is normalized with
//! `object_name`'s `strip_name` (backticks, quotes, `%`, trailing `<-`), a
//! leading `<generic>.` prefix is removed when `<generic>` is a known base
//! S3 generic or a generic declared in this file (lintr: "only lints S3
//! implementations if the class names are too long"), and the remainder is
//! measured in characters. Unlike `object_name`, there is no leading-dot or
//! non-ASCII carve-out — lintr counts those characters, and so do we. Where
//! lintr's generic-prefix removal is order-dependent (its regex alternation
//! follows the vector order of `.base_s3_generics`), Raven deterministically
//! removes the *longest* matching generic prefix.
//!
//! Only positions that introduce a new symbol are checked:
//! assignment targets (`<-`, `<<-`, top-level `=`, `->`, `->>`) and formal
//! parameters of `function_definition`. Compound assignment targets like
//! `obj$field <- ...` are skipped (the assignment doesn't introduce a new
//! symbol name — only the LHS field does, and `object_name` already won't
//! flag those for the same reason).

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::linting::LINT_SOURCE;
use crate::linting::nolint::Suppressions;
use crate::linting::rule_ids;
use crate::linting::rules::object_name::{
    collect_declared_s3_generics, is_known_s3_generic, strip_name,
};
use crate::utf16::byte_offset_to_utf16_column;

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    max_length: u32,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let declared_generics = collect_declared_s3_generics(root, text);
    let cx = CheckContext {
        declared_generics: &declared_generics,
        severity,
        suppressions,
    };
    visit(root, text, max_length, &cx, out);
}

/// Immutable per-run inputs threaded through the AST walk.
struct CheckContext<'a> {
    /// Same-file `UseMethod` generics (see `object_name`), so methods of
    /// locally declared generics get the generic-prefix removal too.
    declared_generics: &'a std::collections::HashSet<String>,
    severity: DiagnosticSeverity,
    suppressions: &'a Suppressions,
}

fn visit(
    node: Node<'_>,
    text: &str,
    max_length: u32,
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) {
    match node.kind() {
        "binary_operator" => check_assignment(node, text, max_length, cx, out),
        "function_definition" => check_parameters(node, text, max_length, cx, out),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, max_length, cx, out);
    }
}

fn check_assignment(
    node: Node<'_>,
    text: &str,
    max_length: u32,
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(op) = node.child_by_field_name("operator") else {
        return;
    };
    let op_text = text.get(op.start_byte()..op.end_byte()).unwrap_or("");
    let target = match op_text {
        "<-" | "<<-" | "=" => node.child_by_field_name("lhs"),
        "->" | "->>" => node.child_by_field_name("rhs"),
        _ => return,
    };
    // Note: tree-sitter-r parses `f(name = value)` as an `argument` node
    // whose `=` is an internal token, not as a `binary_operator`. So named
    // arguments never reach this branch and need no explicit guard.
    let Some(target) = target else {
        return;
    };
    if target.kind() != "identifier" {
        return;
    }
    let name = text
        .get(target.start_byte()..target.end_byte())
        .unwrap_or("");
    check_name(target, name, max_length, text, cx, out);
}

fn check_parameters(
    node: Node<'_>,
    text: &str,
    max_length: u32,
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(params) = node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        // Tree-sitter-r exposes formal parameters as `parameter` nodes (whether
        // or not they carry a default value), so this is the only kind we
        // need to match. The `dots` token (`...`) is not a user-chosen name.
        if child.kind() != "parameter" {
            continue;
        }
        let Some(ident) = child.child_by_field_name("name") else {
            continue;
        };
        if ident.kind() != "identifier" {
            continue;
        }
        let name = text.get(ident.start_byte()..ident.end_byte()).unwrap_or("");
        check_name(ident, name, max_length, text, cx, out);
    }
}

fn check_name(
    name_node: Node<'_>,
    name: &str,
    max_length: u32,
    text: &str,
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let name = strip_name(name);
    if name.is_empty() {
        return;
    }
    // Remove the longest `<generic>.` prefix so S3 methods are only flagged
    // when the class-name remainder is itself too long (lintr behavior).
    let body = strip_generic_prefix(name, cx.declared_generics);
    let len = body.chars().count() as u32;
    if len <= max_length {
        return;
    }
    let severity = cx.severity;
    let suppressions = cx.suppressions;
    let line_no = name_node.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::OBJECT_LENGTH) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, name_node.start_position().column);
    let end_col = byte_offset_to_utf16_column(line_text, name_node.end_position().column);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(name_node.end_position().row as u32, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::OBJECT_LENGTH.to_string())),
        message: format!("Identifier `{name}` is {len} characters long; maximum is {max_length}."),
        ..Default::default()
    });
}

/// Remove the longest leading `<generic>.` prefix whose generic is a known
/// base S3 generic or one declared in this file. Returns the remainder (the
/// class part for S3 methods), or the whole name when no prefix matches.
fn strip_generic_prefix<'a>(
    name: &'a str,
    declared_generics: &std::collections::HashSet<String>,
) -> &'a str {
    let mut best: Option<usize> = None;
    for (i, c) in name.char_indices() {
        if c == '.'
            && i + 1 < name.len()
            && (is_known_s3_generic(&name[..i]) || declared_generics.contains(&name[..i]))
        {
            best = Some(i);
        }
    }
    match best {
        Some(i) => &name[i + 1..],
        None => name,
    }
}
