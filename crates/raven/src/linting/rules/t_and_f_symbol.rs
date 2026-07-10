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
//! * **Assignment targets** (`T <- 0`, `0 -> T`, top-level `T = 0`) get their
//!   own message — "Don't use `T` as a variable name" — matching lintr's
//!   dedicated variable-name lint for shadowing the boolean shorthand.
//! * **`$` / `@` RHS** (`obj$T`, `obj@F`). These are field names, not symbol
//!   lookups in the calling scope.
//! * **Named arguments** (`foo(T = TRUE)`) and **formal parameters**
//!   (`function(T) ...`). The `T` here is a name in the local syntax, not a
//!   reference to the boolean.
//! * **Formula contexts** (`y ~ T + F`): anywhere under a `~`, `T`/`F` are
//!   usually term labels, so lintr skips them — except named-argument values
//!   inside a call in the formula (`y ~ foo(x, arg = T)`), which are real
//!   reads and stay flagged.
//! * **Subscripted uses** (`T[1]`, `T[[1]]`) and **callees** (`T(1)`): the
//!   name is being used as data/function, which lintr's grammar never
//!   matches as the boolean-shorthand SYMBOL.

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
        if name == "T" || name == "F" {
            if in_formula_context(node) {
                // Formula terms are exempt in every position — lintr leaves
                // even `y ~ {T <- 1}` alone.
            } else if let Some(target_op) = assignment_target_of(node) {
                // lintr flags assignment targets with a dedicated message.
                emit(
                    node,
                    name,
                    AnchorKind::VariableName(target_op),
                    text,
                    severity,
                    suppressions,
                    out,
                );
            } else if !is_excluded_position(node) {
                emit(
                    node,
                    name,
                    AnchorKind::Read,
                    text,
                    severity,
                    suppressions,
                    out,
                );
            }
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
    // Formula carve-out: anywhere under a `~`, except named-argument values.
    if in_formula_context(ident) {
        return true;
    }
    let Some(parent) = ident.parent() else {
        return false;
    };
    // Subscripted (`T[1]`, `T[[1]]`) or called (`T(1)`) uses: the identifier
    // is the container/function, which lintr's SYMBOL match never covers.
    if matches!(parent.kind(), "subset" | "subset2" | "call")
        && parent
            .child_by_field_name("function")
            .is_some_and(|f| f.id() == ident.id())
    {
        return true;
    }
    match parent.kind() {
        "extract_operator" => {
            // `$` / `@`: skip the RHS field name; LHS is a real reference.
            // Centralized in `crate::extract_op` so this predicate,
            // go-to-def's qualified-member resolver, and
            // `handlers.rs::is_structural_label` cannot drift on the AST shape.
            crate::extract_op::extract_operator_rhs(ident).is_some()
        }
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

/// If this identifier is the assignment-target side of a `binary_operator`,
/// return the operator text.
fn assignment_target_of<'t>(ident: Node<'t>) -> Option<&'static str> {
    let binop = ident.parent()?;
    if binop.kind() != "binary_operator" {
        return None;
    }
    let op = binop.child_by_field_name("operator")?;
    let target = match op.kind() {
        "<-" => binop.child_by_field_name("lhs").map(|t| (t, "<-")),
        "<<-" => binop.child_by_field_name("lhs").map(|t| (t, "<<-")),
        "=" => binop.child_by_field_name("lhs").map(|t| (t, "=")),
        "->" => binop.child_by_field_name("rhs").map(|t| (t, "->")),
        "->>" => binop.child_by_field_name("rhs").map(|t| (t, "->>")),
        _ => None,
    }?;
    (target.0.id() == ident.id()).then_some(target.1)
}

/// True when the identifier sits anywhere under a `~` formula (either unary
/// or binary), except as the *value* of a named argument in a call inside the
/// formula — lintr flags those (`y ~ foo(x, arg = T)`).
fn in_formula_context(ident: Node<'_>) -> bool {
    let mut child = ident;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            "argument" => {
                // A named argument whose value is *exactly* this symbol is a
                // real read even in a formula (`y ~ foo(x, arg = T)`); a `T`
                // nested deeper in the value (`y ~ foo(arg = T + 1)`) stays
                // exempt, matching lintr.
                if child.id() == ident.id()
                    && parent.child_by_field_name("name").is_some()
                    && parent
                        .child_by_field_name("value")
                        .is_some_and(|v| v.id() == child.id())
                {
                    return false;
                }
            }
            "binary_operator" | "unary_operator"
                if parent
                    .child_by_field_name("operator")
                    .is_some_and(|op| op.kind() == "~") =>
            {
                return true;
            }
            // Braces and lambdas inside a formula do not end the formula
            // exemption — lintr leaves `y ~ {T}` and
            // `y ~ sapply(x, function(i) T)` alone (verified empirically).
            _ => {}
        }
        child = parent;
    }
    false
}

/// Which lint variant to emit.
enum AnchorKind {
    /// A read of `T`/`F` as the boolean shorthand.
    Read,
    /// An assignment target shadowing the shorthand; carries the operator.
    VariableName(&'static str),
}

fn emit(
    node: Node<'_>,
    name: &str,
    kind: AnchorKind,
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
    let message = match kind {
        AnchorKind::Read => format!("Use `{preferred}` instead of the symbol `{name}`."),
        AnchorKind::VariableName(op) => format!(
            "Don't use `{name}` as a variable name, as it can break code relying on `{name}` being `{preferred}` (`{name} {op} …`)."
        ),
    };
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(node.end_position().row as u32, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::T_AND_F_SYMBOL.to_string())),
        message,
        ..Default::default()
    });
}
