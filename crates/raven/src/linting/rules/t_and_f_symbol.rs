//! Flag bare `T` / `F` identifiers used as references to `TRUE` / `FALSE`.
//!
//! Mirrors `lintr::T_and_F_symbol_linter`. `T` and `F` are normal identifiers
//! in R, not reserved words, so `T <- 0` silently flips the meaning of any
//! later code that reads `T`. Idiomatic R uses the reserved literals `TRUE`
//! and `FALSE` instead.
//!
//! The rule walks `identifier` nodes whose text is exactly `T` or `F` and
//! reports them, except in positions where the identifier doesn't actually
//! reference the value:
//!
//! * **Extraction/subsetting object names** (`T[1]`, `T[[1]]`, `T$field`,
//!   `obj$T`). These spell objects or fields rather than the Boolean alias.
//! * **Named arguments** (`foo(T = TRUE)`) and **formal parameters**
//!   (`function(T) ...`). The `T` here is a name in the local syntax, not a
//!   reference to the boolean.
//! * **Formula symbols** (`y ~ T + F`) are model terms, not Boolean aliases.
//!   A `T`/`F` used as a named-argument value inside a formula call is still a
//!   value and remains checked.
//!
//! Assignment targets are deliberately reported: rebinding `T` or `F` is
//! exactly what makes later code relying on the aliases unsafe.

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
    if node.kind() == "identifier" {
        let name = text.get(node.start_byte()..node.end_byte()).unwrap_or("");
        if (name == "T" || name == "F") && !is_excluded_position(node) {
            emit(node, name, text, severity, suppressions, out);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

/// True if the identifier is in a non-reference position — somewhere a `T`/`F`
/// spelling doesn't read the boolean value.
fn is_excluded_position(ident: Node<'_>) -> bool {
    if is_inside_formula(ident) && !is_named_argument_value(ident) {
        return true;
    }
    let Some(parent) = ident.parent() else {
        return false;
    };
    match parent.kind() {
        "extract_operator" => true,
        "subset" | "subset2" => parent
            .child_by_field_name("function")
            .is_some_and(|function| function.id() == ident.id()),
        "argument" => {
            // Named argument: `foo(T = TRUE)` — `T` here is a parameter label.
            parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == ident.id())
        }
        "parameter" => {
            // Formal parameter name: `function(T) ...` — declaring, not reading.
            parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == ident.id())
        }
        _ => false,
    }
}

fn is_inside_formula(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "binary_operator"
            && parent
                .child_by_field_name("operator")
                .is_some_and(|operator| operator.kind() == "~")
        {
            return true;
        }
        node = parent;
    }
    false
}

fn is_named_argument_value(ident: Node<'_>) -> bool {
    ident.parent().is_some_and(|parent| {
        parent.kind() == "argument"
            && parent.child_by_field_name("name").is_some()
            && parent
                .child_by_field_name("value")
                .is_some_and(|value| value.id() == ident.id())
    })
}

fn emit(
    node: Node<'_>,
    name: &str,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let line_no = node.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::T_AND_F_SYMBOL) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, node.start_position().column);
    let end_col = byte_offset_to_utf16_column(line_text, node.end_position().column);
    let preferred = if name == "T" { "TRUE" } else { "FALSE" };
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(node.end_position().row as u32, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::T_AND_F_SYMBOL.to_string())),
        message: format!("Use `{preferred}` instead of the symbol `{name}`."),
        ..Default::default()
    });
}
