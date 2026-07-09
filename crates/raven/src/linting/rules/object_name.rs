//! Enforce a naming scheme on user-defined symbols.
//!
//! Walks the tree-sitter AST and flags assignment targets and function
//! parameters whose names don't match the configured [`ObjectNameStyle`] or
//! custom regexes. Mirrors `lintr::object_name_linter` with three per-kind
//! settings: `function`, `variable`, and `argument`. Each kind defaults to
//! `snake_case` and can be independently disabled by including
//! [`ObjectNameStyle::Any`] in its style list.
//! A name passes when it matches any accepted named style or any accepted
//! regex. Named styles keep lintr's decorative-leading-dot behavior; regexes
//! are matched unanchored (partial match) against the full identifier text as
//! written in source; anchor with `^...$` to require a whole-name match.
//!
//! Carve-outs:
//!
//! * **Backtick-quoted names** (`` `with spaces` <- 1 ``, operator overloads
//!   like `` `+.foo` <- function(x, y) ... ``) are skipped, matching lintr.
//! * **S3 method dispatch**: a function definition whose name has the shape
//!   `<generic>.<class>` is exempt when `<generic>` is a known base R S3
//!   generic (see [`is_known_s3_generic`]). Every dot is tried as a possible
//!   split point so methods of generics that themselves contain dots
//!   (`as.Date.character`, `is.numeric.foo`) match; class names that contain
//!   dots (`print.data.frame`) also match because the leftmost generic wins.
//!   A leading `.` (hidden identifier convention) is stripped before the
//!   lookup so hidden methods like `.print.MyClass` are still recognized.
//!   Names with no recognized generic in any prefix (e.g. `foo.Bar`,
//!   `my.func`) are checked normally.
//! * **Leading-dot "hidden" names** (`.foo`, `.my_helper`, `.onLoad`) are
//!   accepted under every scheme — an optional leading dot is stripped before
//!   scheme classification, mirroring lintr.
//! * **Non-ASCII identifiers** are skipped — case is locale-dependent and a
//!   simple regex can't classify them.
//! * **Named-argument `=`** (`f(name = value)`) is never an assignment target,
//!   so it isn't checked. `=` elsewhere (top level, function bodies, braced
//!   blocks) *is* treated as assignment and the LHS is checked.
//! * **Compound LHS** (`obj$field <- ...`, `obj[[i]] <- ...`) is skipped: the
//!   assignment doesn't introduce a new symbol name.

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::linting::LINT_SOURCE;
use crate::linting::config::{CompiledRegex, ObjectNameStyle};
use crate::linting::nolint::Suppressions;
use crate::linting::rule_ids;
use crate::utf16::byte_offset_to_utf16_column;

/// Accepted named styles and regexes for one symbol kind.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KindPatterns<'a> {
    pub styles: &'a [ObjectNameStyle],
    pub regexes: &'a [CompiledRegex],
}

impl KindPatterns<'_> {
    fn is_disabled(self) -> bool {
        self.styles.contains(&ObjectNameStyle::Any)
            || (self.styles.is_empty() && self.regexes.is_empty())
    }
}

/// Per-kind pattern configuration for the rule.
#[derive(Debug, Clone)]
pub(crate) struct ObjectNameStyles<'a> {
    pub function: KindPatterns<'a>,
    pub variable: KindPatterns<'a>,
    pub argument: KindPatterns<'a>,
}

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    styles: ObjectNameStyles<'_>,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    visit(root, text, &styles, severity, suppressions, out);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolKind {
    Function,
    Variable,
    Argument,
}

fn visit(
    node: Node<'_>,
    text: &str,
    styles: &ObjectNameStyles<'_>,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    match node.kind() {
        "binary_operator" => check_assignment(node, text, styles, severity, suppressions, out),
        "function_definition" => check_parameters(node, text, styles, severity, suppressions, out),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, styles, severity, suppressions, out);
    }
}

/// Check the assignment target of a `binary_operator` node.
fn check_assignment(
    node: Node<'_>,
    text: &str,
    styles: &ObjectNameStyles<'_>,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let op_node = match node.child_by_field_name("operator") {
        Some(n) => n,
        None => return,
    };
    let op_text = node_text(op_node, text);

    let (target_node, value_node) = match op_text {
        "<-" | "<<-" | "=" => {
            let lhs = node.child_by_field_name("lhs");
            let rhs = node.child_by_field_name("rhs");
            (lhs, rhs)
        }
        "->" | "->>" => {
            let lhs = node.child_by_field_name("lhs");
            let rhs = node.child_by_field_name("rhs");
            (rhs, lhs)
        }
        _ => return,
    };

    let target = match target_node {
        Some(t) => t,
        None => return,
    };

    // `=` inside an argument list is a named argument, not an assignment.
    if op_text == "=" && node.parent().is_some_and(|p| p.kind() == "argument") {
        return;
    }

    if target.kind() != "identifier" {
        return;
    }

    let name = node_text(target, text);
    if name.is_empty() {
        return;
    }

    let kind = if value_node
        .map(|v| is_function_definition_after_parens(v))
        .unwrap_or(false)
    {
        SymbolKind::Function
    } else {
        SymbolKind::Variable
    };

    let patterns = patterns_for(kind, styles);
    if patterns.is_disabled() {
        return;
    }

    report_if_bad(
        target,
        name,
        kind,
        patterns,
        text,
        severity,
        suppressions,
        out,
    );
}

/// Check formal arguments of a `function_definition` node.
fn check_parameters(
    node: Node<'_>,
    text: &str,
    styles: &ObjectNameStyles<'_>,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let patterns = patterns_for(SymbolKind::Argument, styles);
    if patterns.is_disabled() {
        return;
    }
    let params_node = node.child_by_field_name("parameters").or_else(|| {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|c| c.is_named() && c.kind() == "parameters")
    });
    let params_node = match params_node {
        Some(n) => n,
        None => return,
    };

    let mut cursor = params_node.walk();
    for child in params_node.children(&mut cursor) {
        let ident = match child.kind() {
            "parameter" | "default_parameter" => {
                let mut name_node = None;
                for sub in child.children(&mut child.walk()) {
                    if sub.kind() == "identifier" {
                        name_node = Some(sub);
                        break;
                    }
                }
                name_node
            }
            "identifier" => Some(child),
            // `dots` (`...`) is a literal token, not a user-chosen name.
            _ => None,
        };
        if let Some(ident) = ident {
            let name = node_text(ident, text);
            if name.is_empty() {
                continue;
            }
            report_if_bad(
                ident,
                name,
                SymbolKind::Argument,
                patterns,
                text,
                severity,
                suppressions,
                out,
            );
        }
    }
}

/// Report a diagnostic for `name` when it does not match `patterns`.
///
/// Callers must pre-check [`KindPatterns::is_disabled`] so the disabled fast
/// path is evaluated exactly once at the assignment/parameter call site.
#[allow(clippy::too_many_arguments)]
fn report_if_bad(
    name_node: Node<'_>,
    name: &str,
    kind: SymbolKind,
    patterns: KindPatterns<'_>,
    text: &str,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    debug_assert!(!patterns.is_disabled());
    if should_skip_name(name, kind) {
        return;
    }
    if matches_patterns(name, patterns) {
        return;
    }
    let line_no = name_node.start_position().row as u32;
    if suppressions.is_suppressed_code(line_no, rule_ids::OBJECT_NAME) {
        return;
    }
    let line_text = text.lines().nth(line_no as usize).unwrap_or("");
    let start_col = byte_offset_to_utf16_column(line_text, name_node.start_position().column);
    let end_col = byte_offset_to_utf16_column(line_text, name_node.end_position().column);
    let kind_label = match kind {
        SymbolKind::Function => "Function",
        SymbolKind::Variable => "Variable",
        SymbolKind::Argument => "Argument",
    };
    let message = object_name_message(kind_label, name, patterns);
    out.push(Diagnostic {
        range: Range {
            start: Position::new(line_no, start_col),
            end: Position::new(name_node.end_position().row as u32, end_col),
        },
        severity: Some(severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::OBJECT_NAME.to_string())),
        message,
        ..Default::default()
    });
}

/// Look up the configured patterns for a given symbol kind.
fn patterns_for<'a>(kind: SymbolKind, styles: &ObjectNameStyles<'a>) -> KindPatterns<'a> {
    match kind {
        SymbolKind::Function => styles.function,
        SymbolKind::Variable => styles.variable,
        SymbolKind::Argument => styles.argument,
    }
}

fn matches_patterns(name: &str, patterns: KindPatterns<'_>) -> bool {
    patterns
        .styles
        .iter()
        .any(|&style| matches_scheme(name, style))
        || patterns.regexes.iter().any(|regex| regex.is_match(name))
}

/// Names that should be skipped regardless of the configured scheme.
fn should_skip_name(name: &str, kind: SymbolKind) -> bool {
    // Backtick-quoted identifiers (operator overloads, names with spaces).
    if name.starts_with('`') {
        return true;
    }
    // Non-ASCII identifiers can't be classified by simple ASCII regex.
    if !name.is_ascii() {
        return true;
    }
    // S3 method dispatch: only relevant for function definitions. A name like
    // `print.MyClass` is `<generic>.<ClassName>` — exempt when some prefix
    // ending at a dot is a *known* base R S3 generic (see
    // [`is_known_s3_generic`]). Names whose prefix isn't a recognized generic
    // (e.g. `foo.Bar`) are still checked: there's no signal that they're
    // actually method dispatch rather than a quirky dotted name, and lintr
    // similarly requires evidence (a `UseMethod` call or a known generic)
    // before exempting.
    //
    // We scan *every* dot position rather than just the first because both
    // generics and class names can themselves contain dots:
    //
    //   * `as.Date.character` — method of generic `as.Date` for `character`.
    //     The first dot gives `as` (not a generic); the second gives the
    //     match.
    //   * `print.data.frame` — method of generic `print` for `data.frame`.
    //     The first dot gives `print` (match), so we exit early.
    //
    // We also strip an optional leading `.` so hidden S3 methods like
    // `.print.MyClass` resolve through `print`.
    if kind == SymbolKind::Function {
        let body = name.strip_prefix('.').unwrap_or(name);
        for (i, c) in body.char_indices() {
            if c == '.' && is_known_s3_generic(&body[..i]) {
                return true;
            }
        }
    }
    false
}

/// Base R S3 generics whose `<generic>.<class>` methods are conventionally
/// exempt from naming-style enforcement. The list is intentionally finite — if
/// users define their own generic and want methods exempt, they can suppress
/// the line with `# nolint` or `# raven: ignore` (alias `# @lsp-ignore`).
///
/// Sourced from base R's documented generics across `methods("...")` output
/// for typical interactive sessions: print/format/summary family,
/// statistical model accessors, coercion (`as.*`)/predicate (`is.*`) families,
/// the group generics (`Ops`, `Math`, `Summary`, `Complex`), and a handful of
/// commonly-extended utilities.
fn is_known_s3_generic(name: &str) -> bool {
    // Sorted alphabetically so `binary_search` works.
    const GENERICS: &[&str] = &[
        "AIC",
        "BIC",
        "Complex",
        "Math",
        "Ops",
        "Summary",
        "all.equal",
        "anova",
        "as.Date",
        "as.POSIXct",
        "as.POSIXlt",
        "as.character",
        "as.data.frame",
        "as.double",
        "as.environment",
        "as.factor",
        "as.function",
        "as.integer",
        "as.list",
        "as.logical",
        "as.matrix",
        "as.numeric",
        "as.vector",
        "c",
        "cbind",
        "coef",
        "coefficients",
        "confint",
        "deviance",
        "dim",
        "dimnames",
        "fitted",
        "fitted.values",
        "format",
        "formula",
        "head",
        "is.character",
        "is.data.frame",
        "is.double",
        "is.environment",
        "is.factor",
        "is.function",
        "is.integer",
        "is.list",
        "is.logical",
        "is.matrix",
        "is.numeric",
        "is.vector",
        "labels",
        "length",
        "levels",
        "logLik",
        "mean",
        "merge",
        "names",
        "nobs",
        "plot",
        "predict",
        "print",
        "range",
        "rbind",
        "residuals",
        "rev",
        "simulate",
        "sort",
        "split",
        "str",
        "subset",
        "summary",
        "t",
        "tail",
        "terms",
        "toString",
        "transform",
        "unique",
        "vcov",
        "with",
        "within",
    ];
    GENERICS.binary_search(&name).is_ok()
}

fn matches_scheme(name: &str, style: ObjectNameStyle) -> bool {
    if !name.is_ascii() {
        // Should already be handled by `should_skip_name`, but be defensive.
        return true;
    }
    // R treats a leading dot as the "hidden identifier" marker (e.g. `.foo`).
    // lintr accepts an optional leading dot for every scheme — match that so
    // common idioms like `.my_helper` aren't flagged as snake_case violations.
    // The body after the dot must still match the scheme's normal pattern,
    // and we reject a bare `.` or `..something` to avoid swallowing the
    // dots-in-name case.
    let body = match name.strip_prefix('.') {
        Some(rest) if !rest.starts_with('.') && !rest.is_empty() => rest,
        Some(_) => return false,
        None => name,
    };
    match style {
        ObjectNameStyle::Any => true,
        ObjectNameStyle::SnakeCase => is_snake_case(body),
        ObjectNameStyle::CamelCase => is_camel_case(body),
        ObjectNameStyle::DottedCase => is_dotted_case(body),
        ObjectNameStyle::UpperCase => is_upper_case(body),
        ObjectNameStyle::Lowercase => is_lowercase(body),
    }
}

fn is_snake_case(body: &str) -> bool {
    let bytes = body.as_bytes();
    bytes.first().is_some_and(|b| b.is_ascii_lowercase())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

fn is_camel_case(body: &str) -> bool {
    let bytes = body.as_bytes();
    bytes.first().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.iter().all(|b| b.is_ascii_alphanumeric())
}

fn is_dotted_case(body: &str) -> bool {
    let bytes = body.as_bytes();
    bytes.first().is_some_and(|b| b.is_ascii_lowercase())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.')
}

fn is_upper_case(body: &str) -> bool {
    let bytes = body.as_bytes();
    bytes.first().is_some_and(|b| b.is_ascii_uppercase())
        && bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

fn is_lowercase(body: &str) -> bool {
    let bytes = body.as_bytes();
    bytes.first().is_some_and(|b| b.is_ascii_lowercase())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn scheme_label(style: ObjectNameStyle) -> &'static str {
    match style {
        ObjectNameStyle::SnakeCase => "snake_case",
        ObjectNameStyle::CamelCase => "camelCase",
        ObjectNameStyle::DottedCase => "dotted.case",
        ObjectNameStyle::UpperCase => "UPPER_CASE",
        ObjectNameStyle::Lowercase => "lowercase",
        ObjectNameStyle::Any => "any",
    }
}

fn object_name_message(kind_label: &str, name: &str, patterns: KindPatterns<'_>) -> String {
    if patterns.styles.len() == 1 && patterns.regexes.is_empty() {
        let scheme_label = scheme_label(patterns.styles[0]);
        return format!(
            "{kind_label} name `{name}` does not match the {scheme_label} naming style."
        );
    }

    if patterns.styles.is_empty() {
        return format!("{kind_label} name `{name}` does not match any accepted naming pattern.");
    }

    let labels = patterns
        .styles
        .iter()
        .map(|&style| scheme_label(style))
        .collect::<Vec<_>>()
        .join(", ");
    if patterns.regexes.is_empty() {
        format!("{kind_label} name `{name}` does not match any accepted naming style ({labels}).")
    } else {
        format!(
            "{kind_label} name `{name}` does not match any accepted naming style ({labels}) or pattern."
        )
    }
}

/// Walk through `parenthesized_expression` wrappers and report whether the
/// inner node is a `function_definition`. Mirrors the helper in
/// `cross_file/scope.rs` so paren-wrapped functions still classify as such
/// for naming purposes: `foo <- (function() 1)` is still a function.
fn is_function_definition_after_parens(node: Node<'_>) -> bool {
    let mut current = node;
    loop {
        match current.kind() {
            "function_definition" => return true,
            "parenthesized_expression" => {
                let mut inner = None;
                for child in current.children(&mut current.walk()) {
                    if child.is_named() {
                        inner = Some(child);
                        break;
                    }
                }
                match inner {
                    Some(c) => current = c,
                    None => return false,
                }
            }
            _ => return false,
        }
    }
}

fn node_text<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    let start = node.start_byte();
    let end = node.end_byte();
    text.get(start..end).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linting::nolint::Suppressions;
    use crate::parser_pool::with_parser;

    #[test]
    fn snake_case_classifier_accepts_common_names() {
        assert!(is_snake_case("foo"));
        assert!(is_snake_case("foo_bar"));
        assert!(is_snake_case("foo_bar_2"));
        assert!(is_snake_case("x"));
    }

    #[test]
    fn snake_case_classifier_rejects_other_styles() {
        assert!(!is_snake_case("FooBar"));
        assert!(!is_snake_case("fooBar"));
        assert!(!is_snake_case("foo.bar"));
        assert!(!is_snake_case("FOO"));
        assert!(!is_snake_case("_foo"));
        assert!(!is_snake_case(""));
        assert!(!is_snake_case("2foo"));
    }

    #[test]
    fn camel_case_classifier() {
        assert!(is_camel_case("fooBar"));
        assert!(is_camel_case("parseURL"));
        assert!(is_camel_case("foo2"));
        assert!(!is_camel_case("foo_bar"));
        assert!(!is_camel_case("FooBar"));
        assert!(!is_camel_case("foo.bar"));
    }

    #[test]
    fn dotted_case_classifier() {
        assert!(is_dotted_case("foo.bar"));
        assert!(is_dotted_case("data.frame"));
        assert!(!is_dotted_case("fooBar"));
        assert!(!is_dotted_case("foo_bar"));
    }

    #[test]
    fn upper_case_classifier() {
        assert!(is_upper_case("FOO"));
        assert!(is_upper_case("FOO_BAR"));
        assert!(is_upper_case("PI2"));
        assert!(!is_upper_case("Foo"));
        assert!(!is_upper_case("foo"));
    }

    #[test]
    fn s3_method_detected_for_function_kind_only() {
        // Prefix is a known base R generic — exempt.
        assert!(should_skip_name("print.MyClass", SymbolKind::Function));
        assert!(should_skip_name("format.Date", SymbolKind::Function));
        assert!(should_skip_name("summary.lm", SymbolKind::Function));
        // For variables, dotted names are checked normally — `print.MyClass`
        // isn't a method definition when bound to a non-function value.
        assert!(!should_skip_name("print.MyClass", SymbolKind::Variable));
        // All-lowercase dotted name with unknown prefix is still checked.
        assert!(!should_skip_name("my.func", SymbolKind::Function));
        // Unknown prefix + capitalized suffix (regression for over-broad
        // exemption): `foo` is not a known generic, so `foo.Bar` is checked.
        assert!(!should_skip_name("foo.Bar", SymbolKind::Function));
    }

    #[test]
    fn s3_method_detection_handles_dotted_generics() {
        // Regression: `as.Date.character` is a method of generic `as.Date`
        // for class `character`. Previously the prefix-before-first-dot
        // lookup gave `"as"` (not in the list), so the method was wrongly
        // flagged. The progressive-prefix scan tries `as`, then `as.Date`,
        // and exempts on the second.
        assert!(should_skip_name("as.Date.character", SymbolKind::Function));
        assert!(should_skip_name("as.numeric.foo", SymbolKind::Function));
        assert!(should_skip_name(
            "is.character.MyClass",
            SymbolKind::Function
        ));
        assert!(should_skip_name("all.equal.default", SymbolKind::Function));
        assert!(should_skip_name(
            "fitted.values.MyModel",
            SymbolKind::Function
        ));
        // Class names containing dots also work because the leftmost matching
        // generic wins.
        assert!(should_skip_name("print.data.frame", SymbolKind::Function));
        // Generic name itself (no class suffix) still requires at least one
        // dot to be considered S3 — bare `as.Date` defining the generic is
        // checked by the scheme (and would pass `dotted.case`).
    }

    #[test]
    fn s3_method_detection_handles_hidden_methods() {
        // Hidden S3 methods (`.print.MyClass`) — a leading `.` is stripped
        // before the generic lookup, so `.print.MyClass` still resolves
        // through `print`.
        assert!(should_skip_name(".print.MyClass", SymbolKind::Function));
        assert!(should_skip_name(".as.Date.character", SymbolKind::Function));
        // `.foo.Bar` — `foo` is not a generic, so still flagged.
        assert!(!should_skip_name(".foo.Bar", SymbolKind::Function));
    }

    #[test]
    fn known_s3_generic_recognizes_base_r_generics() {
        assert!(is_known_s3_generic("print"));
        assert!(is_known_s3_generic("format"));
        assert!(is_known_s3_generic("as.Date"));
        assert!(is_known_s3_generic("Ops"));
        assert!(!is_known_s3_generic("foo"));
        assert!(!is_known_s3_generic(""));
    }

    #[test]
    fn matches_scheme_accepts_leading_dot() {
        // R's "hidden identifier" convention: a single leading dot is
        // decorative, and the remainder must still match the scheme.
        assert!(matches_scheme(".foo", ObjectNameStyle::SnakeCase));
        assert!(matches_scheme(".foo_bar", ObjectNameStyle::SnakeCase));
        assert!(matches_scheme(".fooBar", ObjectNameStyle::CamelCase));
        assert!(matches_scheme(".FOO_BAR", ObjectNameStyle::UpperCase));
        // Body after the dot still must match — `.FooBar` is not snake_case.
        assert!(!matches_scheme(".FooBar", ObjectNameStyle::SnakeCase));
        // Two leading dots (or more) is not the hidden convention; reject so
        // we don't accidentally swallow ill-formed names.
        assert!(!matches_scheme("..foo", ObjectNameStyle::SnakeCase));
        assert!(!matches_scheme(".", ObjectNameStyle::SnakeCase));
    }

    #[test]
    fn matches_patterns_accepts_any_named_style_or_regex() {
        let styles = [ObjectNameStyle::SnakeCase, ObjectNameStyle::CamelCase];
        let regexes = [CompiledRegex::new("^x[0-9]+$").unwrap()];
        let patterns = KindPatterns {
            styles: &styles,
            regexes: &regexes,
        };

        assert!(matches_patterns("foo_bar", patterns));
        assert!(matches_patterns("fooBar", patterns));
        assert!(matches_patterns("x123", patterns));
        assert!(!matches_patterns("BadName", patterns));
    }

    #[test]
    fn regex_patterns_match_full_name_and_are_partial() {
        let dot_regexes = [CompiledRegex::new(r"^\.").unwrap()];
        assert!(matches_patterns(
            ".Foo",
            KindPatterns {
                styles: &[],
                regexes: &dot_regexes,
            }
        ));

        let partial_regexes = [CompiledRegex::new("Bar").unwrap()];
        assert!(matches_patterns(
            "fooBarBaz",
            KindPatterns {
                styles: &[],
                regexes: &partial_regexes,
            }
        ));
    }

    #[test]
    fn kind_patterns_disabled_predicate() {
        let any_styles = [ObjectNameStyle::Any];
        let regexes = [CompiledRegex::new("^x").unwrap()];
        assert!(
            KindPatterns {
                styles: &any_styles,
                regexes: &regexes,
            }
            .is_disabled()
        );
        assert!(
            KindPatterns {
                styles: &[],
                regexes: &[],
            }
            .is_disabled()
        );
        assert!(
            !KindPatterns {
                styles: &[],
                regexes: &regexes,
            }
            .is_disabled()
        );
    }

    #[test]
    fn diagnostic_message_preserves_single_style_wording() {
        let styles = [ObjectNameStyle::SnakeCase];
        assert_eq!(
            object_name_message(
                "Variable",
                "fooBar",
                KindPatterns {
                    styles: &styles,
                    regexes: &[],
                },
            ),
            "Variable name `fooBar` does not match the snake_case naming style."
        );
    }

    #[test]
    fn diagnostic_message_describes_multi_style_and_regex_patterns() {
        let styles = [ObjectNameStyle::SnakeCase, ObjectNameStyle::CamelCase];
        let regexes = [CompiledRegex::new("^x").unwrap()];
        assert_eq!(
            object_name_message(
                "Function",
                "BadName",
                KindPatterns {
                    styles: &styles,
                    regexes: &regexes,
                },
            ),
            "Function name `BadName` does not match any accepted naming style (snake_case, camelCase) or pattern."
        );
        assert_eq!(
            object_name_message(
                "Argument",
                "badArg",
                KindPatterns {
                    styles: &[],
                    regexes: &regexes,
                },
            ),
            "Argument name `badArg` does not match any accepted naming pattern."
        );
    }

    #[test]
    fn regex_only_argument_check_does_not_early_return() {
        let tree =
            with_parser(|p| p.parse("f <- function(y) y\n", None)).expect("parse must succeed");
        let any_styles = [ObjectNameStyle::Any];
        let argument_regexes = [CompiledRegex::new("^x").unwrap()];
        let styles = ObjectNameStyles {
            function: KindPatterns {
                styles: &any_styles,
                regexes: &[],
            },
            variable: KindPatterns {
                styles: &any_styles,
                regexes: &[],
            },
            argument: KindPatterns {
                styles: &[],
                regexes: &argument_regexes,
            },
        };
        let mut diags = Vec::new();
        collect(
            "f <- function(y) y\n",
            tree.root_node(),
            styles,
            DiagnosticSeverity::INFORMATION,
            &Suppressions::default(),
            &mut diags,
        );

        assert_eq!(diags.len(), 1, "got {diags:?}");
        assert!(diags[0].message.contains("Argument name `y`"));
    }

    #[test]
    fn backtick_quoted_names_are_skipped() {
        assert!(should_skip_name("`with spaces`", SymbolKind::Variable));
        assert!(should_skip_name("`+.foo`", SymbolKind::Function));
    }

    #[test]
    fn non_ascii_names_are_skipped() {
        assert!(should_skip_name("\u{03b1}", SymbolKind::Variable));
    }
}
