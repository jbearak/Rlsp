//! Flag identifier names longer than the configured maximum.
//!
//! Mirrors `lintr::object_length_linter`. Length is measured in characters
//! after stripping the same decorative leading-dot that `object_name` accepts
//! ("hidden identifier" convention). Quoted names are measured without their
//! delimiters, and Unicode names are measured in characters.
//!
//! Only positions that introduce a new symbol are checked:
//! assignment targets (`<-`, `<<-`, top-level `=`, `->`, `->>`), formal
//! parameters, and literal binding names passed to `assign()` / `setGeneric()`.
//! Compound assignment targets like
//! `obj$field <- ...` check only the leftmost object name (`obj`).

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::linting::LINT_SOURCE;
use crate::linting::nolint::Suppressions;
use crate::linting::rule_ids;
use crate::utf16::byte_offset_to_utf16_column;

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    max_length: u32,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    visit(root, text, max_length, severity, suppressions, out);
}

fn visit(
    node: Node<'_>,
    text: &str,
    max_length: u32,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    match node.kind() {
        "binary_operator" => check_assignment(node, text, max_length, severity, suppressions, out),
        "function_definition" => {
            check_parameters(node, text, max_length, severity, suppressions, out)
        }
        "call" => check_binding_call(node, text, max_length, severity, suppressions, out),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, max_length, severity, suppressions, out);
    }
}

fn check_binding_call(
    node: Node<'_>,
    text: &str,
    max_length: u32,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let function_text = text
        .get(function.start_byte()..function.end_byte())
        .unwrap_or("");
    let formal = if function_text == "assign" || function_text.ends_with("::assign") {
        "x"
    } else if function_text == "setGeneric" || function_text.ends_with("::setGeneric") {
        "name"
    } else {
        return;
    };
    let Some(name_node) = binding_name_argument(node, formal, text) else {
        return;
    };
    let name = text
        .get(name_node.start_byte()..name_node.end_byte())
        .unwrap_or("");
    check_name(
        name_node,
        name,
        max_length,
        text,
        severity,
        suppressions,
        out,
    );
}

fn check_assignment(
    node: Node<'_>,
    text: &str,
    max_length: u32,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
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
    let Some(target) = assignment_name_target(target) else {
        return;
    };
    let name = text
        .get(target.start_byte()..target.end_byte())
        .unwrap_or("");
    check_name(target, name, max_length, text, severity, suppressions, out);
}

fn check_parameters(
    node: Node<'_>,
    text: &str,
    max_length: u32,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
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
        check_name(ident, name, max_length, text, severity, suppressions, out);
    }
}

fn check_name(
    name_node: Node<'_>,
    name: &str,
    max_length: u32,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let name = strip_name_delimiters(name);
    if name.is_empty() {
        return;
    }
    // Strip the optional leading `.` (hidden identifier convention) before
    // measuring, matching `object_name`'s carve-out.
    let body = match name.strip_prefix('.') {
        Some(rest) if !rest.starts_with('.') && !rest.is_empty() => rest,
        Some(_) => name,
        None => name,
    };
    let len = body.chars().count() as u32;
    if len <= max_length {
        return;
    }
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

fn strip_name_delimiters(name: &str) -> &str {
    for delimiter in ['`', '\'', '"'] {
        if let Some(inner) = name
            .strip_prefix(delimiter)
            .and_then(|rest| rest.strip_suffix(delimiter))
        {
            return inner;
        }
    }
    name
}

fn assignment_name_target(mut target: Node<'_>) -> Option<Node<'_>> {
    loop {
        match target.kind() {
            "identifier" | "string" => return Some(target),
            "extract_operator" => target = target.child_by_field_name("lhs")?,
            "subset" | "subset2" => target = target.child_by_field_name("function")?,
            "call" => {
                let arguments = target.child_by_field_name("arguments")?;
                target = first_positional_argument(arguments)?;
            }
            _ => return None,
        }
    }
}

fn binding_name_argument<'tree>(
    call: Node<'tree>,
    formal: &str,
    text: &str,
) -> Option<Node<'tree>> {
    let arguments = call.child_by_field_name("arguments")?;
    let mut first_positional = None;
    let mut cursor = arguments.walk();
    for argument in arguments
        .children(&mut cursor)
        .filter(|child| child.kind() == "argument")
    {
        let name = argument.child_by_field_name("name");
        let value = argument.child_by_field_name("value");
        if name.and_then(|name| text.get(name.start_byte()..name.end_byte())) == Some(formal) {
            return value.filter(|value| value.kind() == "string");
        }
        if name.is_none() && first_positional.is_none() {
            first_positional = value;
        }
    }
    first_positional.filter(|value| value.kind() == "string")
}

fn first_positional_argument(arguments: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = arguments.walk();
    arguments
        .children(&mut cursor)
        .find(|child| child.kind() == "argument" && child.child_by_field_name("name").is_none())?
        .child_by_field_name("value")
}
