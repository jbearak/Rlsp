//! Enforce conventional spacing around infix operators.
//!
//! Walks the tree-sitter AST and flags missing whitespace around the same
//! operator set `lintr::infix_spaces_linter()` lints by default — the
//! "low-precedence" operators of the tidyverse style guide:
//!
//! * Arithmetic `+`, `-`, `*`, `/`; comparison `<`, `>`, `<=`, `>=`, `==`,
//!   `!=`; logical `&`, `|`, `&&`, `||`; assignment `<-`, `<<-`, `:=`, `->`,
//!   `->>`, `=`; pipe `|>`; binary formula `~`; and every `%any%` user-defined
//!   operator. Each requires at least one space on both sides.
//! * The `=` of named call arguments (`f(name = value)`) and of formal
//!   defaults (`function(x = 1)`) is linted the same way, matching lintr's
//!   `EQ_SUB` / `EQ_FORMALS` defaults. A named argument with a *missing*
//!   value (`alist(a =)`, `switch(b =, ...)`) still requires a space after
//!   the `=` before the following `,` or closer — lintr's test suite pins
//!   this exact behavior (`alist(missing_arg = )` clean, `alist(missing_arg
//!   =)` flagged).
//!
//! High-precedence operators (`^`/`**`, `:`, `::`/`:::`, `$`, `@`, `?`) and
//! unary `-`/`+`/`!`/`~` are **not linted at all**, in either direction —
//! the style guide exempts them and lintr's `infix_metadata$low_precedence`
//! filter skips them, so `x^2`, `x ^ 2`, `1:10`, and `obj $ field` are all
//! left alone.
//!
//! Carve-outs shared with lintr:
//!
//! * Extra whitespace is fine (`allow_multiple_spaces = TRUE` semantics):
//!   alignment like `x   <- 1` is never flagged; only a *zero-width* gap is.
//! * Line continuations — operator at end of line, operand on the next line
//!   (or the mirror image) — are left alone since the line break itself
//!   supplies the separation.
//! * `/` inside a `box::use(...)` declaration is a module path, not
//!   division, and is exempt.
//!
//! Disambiguation between unary and binary forms is handled by tree-sitter:
//! `-x` parses as a `unary_operator`, `a - b` as a `binary_operator`, so the
//! unary forms never reach the binary check.

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
        "binary_operator" => check_binary(node, text, severity, suppressions, out),
        // Named call arguments (`f(a = 1)`) and formal-parameter defaults
        // (`function(x = 1)`) carry their `=` as a direct child, not as a
        // `binary_operator` — lintr lints these as EQ_SUB / EQ_FORMALS.
        "argument" | "parameter" => check_named_eq(node, text, severity, suppressions, out),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, severity, suppressions, out);
    }
}

/// Whether a `binary_operator` token belongs to lintr's linted
/// ("low-precedence") set. Everything else — `^`/`**`, `:`, `?`, and any
/// future operator kinds — is skipped entirely.
fn requires_spaces(op_text: &str) -> bool {
    // The `%...%` family of user-defined infix operators always requires
    // spaces. tree-sitter-r reports the operator text as the literal `%...%`
    // form, so a simple prefix/suffix test is sufficient.
    if op_text.starts_with('%') && op_text.ends_with('%') && op_text.len() >= 2 {
        return true;
    }

    matches!(
        op_text,
        "+" | "-"
            | "*"
            | "/"
            | "<"
            | ">"
            | "<="
            | ">="
            | "=="
            | "!="
            | "&"
            | "|"
            | "&&"
            | "||"
            | "<-"
            | "<<-"
            | ":="
            | "->"
            | "->>"
            | "="
            | "|>"
            | "~"
    )
}

fn check_binary(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let Some(op) = node.child_by_field_name("operator") else {
        return;
    };
    let op_text = match text.get(op.start_byte()..op.end_byte()) {
        Some(s) => s,
        None => return,
    };
    if !requires_spaces(op_text) {
        return;
    }
    let Some(lhs) = node.child_by_field_name("lhs") else {
        return;
    };
    let Some(rhs) = node.child_by_field_name("rhs") else {
        return;
    };

    // In `box::use(a/b, ../a)` the slashes are module paths, not division;
    // lintr exempts them and so do we.
    if op_text == "/" && inside_box_use(node, text) {
        return;
    }

    check_gaps(
        text,
        op,
        op_text,
        Some(lhs.end_byte()),
        Some(rhs.start_byte()),
        severity,
        suppressions,
        out,
    );
}

/// Check the `=` of an `argument` or `parameter` node (named call argument /
/// formal default). When the value is missing (`alist(a =)`), the gap after
/// `=` is measured against the following token (`,` or the closer) instead,
/// matching lintr.
fn check_named_eq(
    node: Node<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let mut cursor = node.walk();
    let Some(eq) = node.children(&mut cursor).find(|child| child.kind() == "=") else {
        return;
    };

    let name_end = node.child_by_field_name("name").map(|name| name.end_byte());
    let value_start = node
        .child_by_field_name("value") // `argument` nodes
        .or_else(|| node.child_by_field_name("default")) // `parameter` nodes
        .map(|value| value.start_byte())
        // Missing value: measure against the next token after the argument
        // (`,` or the closing delimiter), which lintr also lints against.
        .or_else(|| node.next_sibling().map(|sib| sib.start_byte()));

    check_gaps(
        text,
        eq,
        "=",
        name_end,
        value_start,
        severity,
        suppressions,
        out,
    );
}

/// Flag zero-width gaps on either side of `op`. `None` bounds (unresolvable
/// neighbors) skip that side, as do cross-line gaps.
// One flat leaf helper shared by two call shapes; a params struct would just
// re-window the same seven values.
#[allow(clippy::too_many_arguments)]
fn check_gaps(
    text: &str,
    op: Node<'_>,
    op_text: &str,
    left_end: Option<usize>,
    right_start: Option<usize>,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let left_gap = left_end.and_then(|end| gap_text(text, end, op.start_byte()));
    let right_gap = right_start.and_then(|start| gap_text(text, op.end_byte(), start));

    if left_gap.is_some_and(|g| g.is_empty()) {
        report(
            text,
            op,
            "missing space before `",
            op_text,
            "`",
            severity,
            suppressions,
            out,
        );
    }
    if right_gap.is_some_and(|g| g.is_empty()) {
        report(
            text,
            op,
            "missing space after `",
            op_text,
            "`",
            severity,
            suppressions,
            out,
        );
    }
}

/// True when `node` sits anywhere inside the arguments of a `box::use(...)`
/// call.
fn inside_box_use(node: Node<'_>, text: &str) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "call"
            && parent
                .child_by_field_name("function")
                .is_some_and(|f| is_box_use_function(f, text))
        {
            return true;
        }
        current = parent;
    }
    false
}

/// True for a call-function node spelling `box::use` (or `box:::use`).
fn is_box_use_function(node: Node<'_>, text: &str) -> bool {
    if node.kind() != "namespace_operator" {
        return false;
    }
    let side_text = |field: &str| {
        node.child_by_field_name(field)
            .and_then(|n| text.get(n.start_byte()..n.end_byte()))
    };
    side_text("lhs") == Some("box") && side_text("rhs") == Some("use")
}

/// Return the text between byte offsets `start` and `end` only if it stays on
/// a single line — i.e. contains no `\n`. Returning `None` for cross-line gaps
/// lets callers skip the check (line-continuation case).
fn gap_text(text: &str, start: usize, end: usize) -> Option<&str> {
    let slice = text.get(start..end)?;
    if slice.as_bytes().contains(&b'\n') {
        None
    } else {
        Some(slice)
    }
}

#[allow(clippy::too_many_arguments)]
fn report(
    text: &str,
    op: Node<'_>,
    prefix: &str,
    op_text: &str,
    suffix: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let line_no = op.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::INFIX_SPACES) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, op.start_position().column);
    let end_col = byte_offset_to_utf16_column(line_text, op.end_position().column);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(op.end_position().row as u32, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::INFIX_SPACES.to_string())),
        message: format!("{prefix}{op_text}{suffix}."),
        ..Default::default()
    });
}
