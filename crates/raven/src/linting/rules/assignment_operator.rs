//! Enforce a preferred assignment operator.
//!
//! Mirrors `lintr::assignment_linter`. Under the default `<-` style the
//! flagged operators are `=`, `->`, `->>`, and the magrittr assignment pipe
//! `%<>%` (`<<-` is allowed — lintr 3.3.0.1's default is
//! `operator = c("<-", "<<-")`). Under the `=` style, `<-`, `<<-`, `->`,
//! `->>`, and `%<>%` are flagged. `:=` is never linted (rlang/data.table
//! usage).
//!
//! A `=` whose `binary_operator` lives *directly* under an `argument` node is
//! named-argument syntax (`f(name = value)`) and is never reported. Beyond
//! that, lintr's implicit-assignment exclusion is mirrored: an assignment
//! nested (at any depth) inside a call argument, an `if`/`while` condition,
//! or a `for` sequence is skipped — `lapply(xs, function(x) { y = x; y })`
//! and `if ({a = TRUE}) 1` are clean — *unless* the enclosing argument /
//! condition is explicitly parenthesized (`fun((blah = fun(1)))` is still
//! flagged), exactly matching lintr's XPath.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::linting::LINT_SOURCE;
use crate::linting::config::AssignmentOperatorStyle;
use crate::linting::nolint::Suppressions;
use crate::linting::rule_ids;
use crate::utf16::byte_offset_to_utf16_column;

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    style: AssignmentOperatorStyle,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    visit(root, text, style, severity, suppressions, out);
}

fn visit(
    node: Node<'_>,
    text: &str,
    style: AssignmentOperatorStyle,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    if node.kind() == "binary_operator"
        && let Some(op_node) = node.child_by_field_name("operator")
    {
        let op_text = node_text(op_node, text);
        // Skip `=` that is directly in an if/while condition —
        // condition_assignment handles it with a more specific message.
        let skip_for_condition = op_text == "=" && is_if_while_condition_directly(node);
        if !skip_for_condition && !is_named_argument(node, op_text) {
            let preferred = match style {
                AssignmentOperatorStyle::LeftArrow => "<-",
                AssignmentOperatorStyle::Equals => "=",
            };
            let bad = match style {
                AssignmentOperatorStyle::LeftArrow => {
                    matches!(op_text, "=" | "->" | "->>" | "%<>%")
                }
                AssignmentOperatorStyle::Equals => {
                    matches!(op_text, "<-" | "<<-" | "->" | "->>" | "%<>%")
                }
            };
            if bad && !in_implicit_assignment_context(node) {
                let line_no = op_node.start_position().row as u32;
                if !suppressions.is_suppressed_code(line_no, rule_ids::ASSIGNMENT_OPERATOR) {
                    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
                    let start_col =
                        byte_offset_to_utf16_column(line_text, op_node.start_position().column);
                    let end_col =
                        byte_offset_to_utf16_column(line_text, op_node.end_position().column);
                    let message = match op_text {
                        "%<>%" => "Avoid the assignment pipe `%<>%`; prefer pipes and \
                                   assignment in separate steps."
                            .to_string(),
                        "<<-" | "->>" => format!(
                            "Replace `{op_text}` by assigning to a specific environment \
                             (with `assign()` or `{preferred}`) to avoid hard-to-predict \
                             behavior."
                        ),
                        _ => format!("Use `{preferred}` for assignment instead of `{op_text}`."),
                    };
                    out.push(Diagnostic {
                        range: Range {
                            start: Position::new(line_no, start_col),
                            end: Position::new(op_node.end_position().row as u32, end_col),
                        },
                        severity: Some(severity),
                        source: Some(LINT_SOURCE.to_string()),
                        code: Some(NumberOrString::String(
                            rule_ids::ASSIGNMENT_OPERATOR.to_string(),
                        )),
                        message,
                        ..Default::default()
                    });
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, style, severity, suppressions, out);
    }
}

/// True if the given `binary_operator` node represents a named argument like
/// `name = value` inside a call. Tree-sitter-r wraps each top-level
/// expression in a call's argument list in an `argument` node, so a named
/// argument's `=` `binary_operator` has `argument` as its direct parent.
///
/// Anything nested deeper — assignments inside a function body
/// (`lapply(xs, function(x) { y = x; y })`), inside a braced block
/// (`f({ y = 1 })`), or inside control flow (`f(if (cond) y = 1)`) — is a
/// real assignment and must be reported.
fn is_named_argument(binop: Node<'_>, op_text: &str) -> bool {
    if op_text != "=" {
        return false;
    }
    binop.parent().is_some_and(|p| p.kind() == "argument")
}

/// lintr's implicit-assignment exclusion: skip an assignment operator that
/// is nested — at any depth — inside a call argument, an `if`/`while`
/// condition, or a `for` sequence, unless the enclosing argument/condition
/// expression is explicitly parenthesized. Mirrors the XPath
/// `not(ancestor::expr[preceding-sibling::*[call/IF/WHILE/IN] and
/// not(descendant-or-self::expr/*[1][self::OP-LEFT-PAREN])])`.
fn in_implicit_assignment_context(binop: Node<'_>) -> bool {
    let mut child = binop;
    while let Some(parent) = child.parent() {
        let excludes = match parent.kind() {
            // A call argument's value (positional or named).
            "argument" => true,
            // The condition of `if`/`while`, or the `for` sequence.
            "if_statement" | "while_statement" => parent
                .child_by_field_name("condition")
                .is_some_and(|cond| cond.id() == child.id()),
            "for_statement" => parent
                .child_by_field_name("sequence")
                .is_some_and(|sequence| sequence.id() == child.id()),
            _ => false,
        };
        if excludes && child.kind() != "parenthesized_expression" {
            return true;
        }
        child = parent;
    }
    false
}

/// Returns `true` if `binop` is the direct `condition` field of an
/// `if_statement` or `while_statement`. Used to avoid double-diagnosing
/// `if (x = 1)` — `condition_assignment` handles that case specifically.
fn is_if_while_condition_directly(binop: Node<'_>) -> bool {
    if let Some(parent) = binop.parent()
        && matches!(parent.kind(), "if_statement" | "while_statement")
        && let Some(cond) = parent.child_by_field_name("condition")
    {
        return cond.id() == binop.id();
    }
    false
}

fn node_text<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    let start = node.start_byte();
    let end = node.end_byte();
    text.get(start..end).unwrap_or("")
}
