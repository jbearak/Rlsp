//! Enforce a naming scheme on user-defined symbols.
//!
//! Walks the tree-sitter AST and flags assignment targets and function
//! parameters whose names don't match the configured [`ObjectNameStyle`] or
//! custom regexes. Mirrors `lintr::object_name_linter` with three per-kind
//! settings: `function`, `variable`, and `argument`. Each kind defaults to
//! `snake_case` + `symbols` (lintr's default `styles`) and can be
//! independently disabled by including [`ObjectNameStyle::Any`] in its style
//! list. Checked positions are direct symbol targets of assignments,
//! quoted-string targets (`"foo" <- 1`), and formal parameters.
//! A name passes when it matches any accepted named style or any accepted
//! regex. Named styles keep lintr's decorative-leading-dot behavior; regexes
//! are matched unanchored (partial match) against the [`strip_name`]-
//! normalized identifier (leading dot included); anchor with `^...$` to
//! require a whole-name match.
//!
//! Carve-outs:
//!
//! * **Backtick-quoted names** are *stripped*, not exempted (issue #599,
//!   mirroring lintr's `strip_names`): one leading/trailing run of backticks,
//!   quotes, and `%` is removed (plus a trailing `<-`, so replacement
//!   functions like `` `height<-` `` check as `height`), and the remaining
//!   name goes through the normal pattern matching. `` `myBadName` <- 1 `` is
//!   therefore flagged exactly like `myBadName <- 1`, while `` `%+%` `` (an
//!   operator overload) strips to `+` and passes via the `symbols` style. A
//!   name that strips to nothing (e.g. `` `%%` ``) is skipped, as in lintr.
//! * **S3 method dispatch**: a name of the shape `<generic>.<class>` is
//!   exempt when `<generic>` is a known base R S3 generic (see
//!   [`is_known_s3_generic`], ported from lintr's `.base_s3_generics`,
//!   including operator generics like `+` so `` `+.foo` `` is exempt) or a
//!   generic declared in the same file (a top-level assignment of a function
//!   whose body calls `UseMethod`, mirroring lintr's
//!   `declared_s3_generics`). Every dot is tried as a possible split point so
//!   methods of generics that themselves contain dots (`as.Date.character`,
//!   `is.numeric.foo`) match; class names that contain dots
//!   (`print.data.frame`) also match because the leftmost generic wins.
//!   A leading `.` (hidden identifier convention) is stripped before the
//!   lookup so hidden methods like `.print.MyClass` are still recognized —
//!   a deliberate leniency over lintr, which flags hidden methods.
//! * **Special functions** (`.onLoad`, `.onAttach`, `.onUnload`,
//!   `.onDetach`, `.Last.lib`, `.First`, `.Last`) and `...` are always
//!   exempt, matching lintr's `is_special_function`.
//! * **Leading-dot "hidden" names** (`.foo`, `.my_helper`) are accepted under
//!   every scheme — an optional leading dot is stripped before scheme
//!   classification, mirroring lintr.
//! * **Non-ASCII identifiers** are skipped when no regexes are configured
//!   for the kind — case is locale-dependent and the named styles' simple
//!   ASCII schemes can't classify them. When regexes are configured
//!   (regex-only or combined with styles), non-ASCII names are checked
//!   against the regexes; the named styles never match them. (Deliberate
//!   divergence: lintr's ASCII-only style regexes flag non-ASCII names.)
//! * **Named-argument `=`** (`f(name = value)`) is never an assignment target,
//!   so it isn't checked. `=` elsewhere (top level, function bodies, braced
//!   blocks) *is* treated as assignment and the LHS is checked.
//! * **Compound LHS**: a `$`/`@` chain checks its *leftmost* object (`a` in
//!   `a$b$c <- 1` — the field names may be beyond the user's control), and
//!   subscripted targets (`x[[i]] <- ...`) are skipped entirely, matching
//!   lintr. Literal binding names in `assign("name", …)` and
//!   `setGeneric("name", …)` calls are checked too.

use std::collections::HashSet;

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
    let declared_generics = collect_declared_s3_generics(root, text);
    let cx = CheckContext {
        styles: &styles,
        declared_generics: &declared_generics,
        severity,
        suppressions,
    };
    visit(root, text, &cx, out);
}

/// Immutable per-run inputs threaded through the AST walk.
struct CheckContext<'a> {
    styles: &'a ObjectNameStyles<'a>,
    /// Names of S3 generics declared in this file (top-level assignments of
    /// functions whose body calls `UseMethod`), mirroring lintr's
    /// `declared_s3_generics`. Methods of these generics are exempt just like
    /// methods of base generics.
    declared_generics: &'a HashSet<String>,
    severity: DiagnosticSeverity,
    suppressions: &'a Suppressions,
}

/// Collect the names of S3 generics declared at the top level of the file: a
/// direct `program` child of the form `name <- function(...) ...` (also `=`,
/// `<<-`, right-assign, or a lambda) whose body contains a `UseMethod(...)`
/// call. Mirrors lintr's `declared_s3_generics`.
pub(crate) fn collect_declared_s3_generics(root: Node<'_>, text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "binary_operator" {
            continue;
        }
        let Some(op) = child.child_by_field_name("operator") else {
            continue;
        };
        let (target, value) = match node_text(op, text) {
            "<-" | "<<-" | "=" => (
                child.child_by_field_name("lhs"),
                child.child_by_field_name("rhs"),
            ),
            "->" | "->>" => (
                child.child_by_field_name("rhs"),
                child.child_by_field_name("lhs"),
            ),
            _ => continue,
        };
        let (Some(target), Some(value)) = (target, value) else {
            continue;
        };
        if target.kind() != "identifier"
            || !is_function_definition_after_parens(value)
            || !contains_use_method_call(value, text)
        {
            continue;
        }
        out.insert(strip_name(node_text(target, text)).to_string());
    }
    out
}

/// Descend a `$`/`@` extract chain to its leftmost object. Any other shape
/// (subscripts, calls) is returned as-is and skipped by the caller's kind
/// check.
fn leftmost_extract_object<'t>(node: Node<'t>) -> Node<'t> {
    let mut current = node;
    while current.kind() == "extract_operator" {
        match current.child_by_field_name("lhs") {
            Some(lhs) => current = lhs,
            None => break,
        }
    }
    current
}

/// True when the subtree contains a call to `UseMethod`.
fn contains_use_method_call(node: Node<'_>, text: &str) -> bool {
    if node.kind() == "call"
        && node
            .child_by_field_name("function")
            .is_some_and(|f| f.kind() == "identifier" && node_text(f, text) == "UseMethod")
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| contains_use_method_call(child, text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolKind {
    Function,
    Variable,
    Argument,
}

fn visit(node: Node<'_>, text: &str, cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    match node.kind() {
        "binary_operator" => check_assignment(node, text, cx, out),
        "function_definition" => check_parameters(node, text, cx, out),
        "call" => check_binding_call(node, text, cx, out),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, cx, out);
    }
}

/// Check the assignment target of a `binary_operator` node.
fn check_assignment(node: Node<'_>, text: &str, cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
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

    // Direct symbol targets and quoted-string targets (`"foo" <- 1`) are
    // checked — lintr lints STR_CONST assignment targets, stripping the
    // quotes. For a `$`/`@` compound LHS, lintr checks the *leftmost* object
    // (`a` in `a$b$c <- 1` — `b`/`c` may be beyond the user's control), but
    // never anything subscripted (`x[[i]] <- 1`, `x[i]$a <- 1` are exempt).
    let target = leftmost_extract_object(target);
    if !matches!(target.kind(), "identifier" | "string") {
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

    let patterns = patterns_for(kind, cx.styles);
    if patterns.is_disabled() {
        return;
    }

    report_if_bad(target, name, kind, patterns, text, cx, out);
}

/// Check the literal binding name of `assign("name", …)` (a variable) and
/// `setGeneric("name", …)` (a function) — lintr lints both.
fn check_binding_call(
    node: Node<'_>,
    text: &str,
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(function) = node.child_by_field_name("function") else {
        return;
    };
    let function_text = node_text(function, text);
    let kind = match function_text {
        "assign" => SymbolKind::Variable,
        "setGeneric" => SymbolKind::Function,
        _ if function_text.ends_with("::assign") => SymbolKind::Variable,
        _ if function_text.ends_with("::setGeneric") => SymbolKind::Function,
        _ => return,
    };
    let named = match kind {
        SymbolKind::Variable => "x",
        _ => "name",
    };
    let Some(args) = node.child_by_field_name("arguments") else {
        return;
    };
    // The name is the first positional argument, or the one named `x` /
    // `name` (lintr accepts either spelling).
    let mut cursor = args.walk();
    let mut name_node = None;
    for child in args.children(&mut cursor) {
        if child.kind() != "argument" {
            continue;
        }
        match child.child_by_field_name("name") {
            Some(arg_name) if node_text(arg_name, text) == named => {
                name_node = child.child_by_field_name("value");
                break;
            }
            Some(_) => {}
            None => {
                name_node = child.child_by_field_name("value");
                break;
            }
        }
    }
    let Some(name_node) = name_node else {
        return;
    };
    if name_node.kind() != "string" {
        return;
    }
    let patterns = patterns_for(kind, cx.styles);
    if patterns.is_disabled() {
        return;
    }
    report_if_bad(
        name_node,
        node_text(name_node, text),
        kind,
        patterns,
        text,
        cx,
        out,
    );
}

/// Check formal arguments of a `function_definition` node.
fn check_parameters(node: Node<'_>, text: &str, cx: &CheckContext<'_>, out: &mut Vec<Diagnostic>) {
    let patterns = patterns_for(SymbolKind::Argument, cx.styles);
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
            report_if_bad(ident, name, SymbolKind::Argument, patterns, text, cx, out);
        }
    }
}

/// Report a diagnostic for `name` when it does not match `patterns`.
///
/// The raw token text is stripped first (backticks, quotes, `%`, trailing
/// `<-`; see [`strip_name`]) and everything downstream — carve-outs, style
/// and regex matching, the message — operates on the stripped name, matching
/// lintr's `strip_names`.
///
/// Callers pre-check [`KindPatterns::is_disabled`] as a fast path, while this
/// function also guards the invariant so future call sites cannot report every
/// name for a disabled symbol kind.
fn report_if_bad(
    name_node: Node<'_>,
    name: &str,
    kind: SymbolKind,
    patterns: KindPatterns<'_>,
    text: &str,
    cx: &CheckContext<'_>,
    out: &mut Vec<Diagnostic>,
) {
    if patterns.is_disabled() {
        return;
    }
    let name = strip_name(name);
    // A name that strips to nothing (e.g. `` `%%` ``) is conforming in lintr.
    if name.is_empty() {
        return;
    }
    if should_skip_name(name, patterns, cx.declared_generics) {
        return;
    }
    if matches_patterns(name, patterns) {
        return;
    }
    let line_no = name_node.start_position().row as u32;
    if cx
        .suppressions
        .is_suppressed_code(line_no, rule_ids::OBJECT_NAME)
    {
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
        severity: Some(cx.severity),
        source: Some(LINT_SOURCE.to_string()),
        code: Some(NumberOrString::String(rule_ids::OBJECT_NAME.to_string())),
        message,
        ..Default::default()
    });
}

/// Strip the token decorations lintr's `strip_names` removes before pattern
/// matching: a leading run of backticks/quotes/`%` and a trailing run of
/// backticks/quotes/`<`/`-`/`%`. The trailing `<-` case is what lets
/// replacement functions (`` `height<-` ``) check as their base name.
pub(crate) fn strip_name(name: &str) -> &str {
    name.trim_start_matches(['`', '"', '\'', '%'])
        .trim_end_matches(['`', '"', '\'', '<', '-', '%'])
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

/// Names that should be skipped regardless of the configured scheme. `name`
/// is the [`strip_name`]-normalized token text.
fn should_skip_name(
    name: &str,
    patterns: KindPatterns<'_>,
    declared_generics: &HashSet<String>,
) -> bool {
    // lintr's `is_special_function` names and `...` are always conforming.
    if is_special_function(name) || name == "..." {
        return true;
    }
    // Non-ASCII identifiers can't be classified by the named styles' simple
    // ASCII schemes, so configurations with no regexes skip them. When
    // regexes ARE configured — regex-only or combined with styles — the name
    // is checked: regexes can express Unicode constraints, so exempting
    // non-ASCII names would make such policies unenforceable (the named
    // styles never match a non-ASCII name; see `matches_scheme`).
    if !name.is_ascii() {
        return patterns.regexes.is_empty();
    }
    // S3 method dispatch. A name like `print.MyClass` is
    // `<generic>.<ClassName>` — exempt when some prefix ending at a dot is a
    // known base R S3 generic (see [`is_known_s3_generic`]) or a generic
    // declared in this file. Names whose prefix isn't a recognized generic
    // (e.g. `foo.Bar`) are still checked: there's no signal that they're
    // actually method dispatch rather than a quirky dotted name, and lintr
    // similarly requires evidence (a `UseMethod` call or a known generic)
    // before exempting. Like lintr, the exemption applies to every symbol
    // kind, not just function definitions — lintr runs one generic check over
    // all assignment targets and formals.
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
    // `.print.MyClass` resolve through `print` (a deliberate leniency over
    // lintr, which only matches a generic at the very start of the name).
    // The class part must be non-empty — `print.` is not method dispatch.
    let body = name.strip_prefix('.').unwrap_or(name);
    for (i, c) in body.char_indices() {
        if c == '.'
            && i + 1 < body.len()
            && (is_known_s3_generic(&body[..i]) || declared_generics.contains(&body[..i]))
        {
            return true;
        }
    }
    false
}

/// Namespace hooks and session hooks lintr always exempts
/// (`lintr:::special_funs`).
fn is_special_function(name: &str) -> bool {
    matches!(
        name,
        ".onLoad" | ".onAttach" | ".onUnload" | ".onDetach" | ".Last.lib" | ".First" | ".Last"
    )
}

/// S3 generics whose `<generic>.<class>` methods are exempt from
/// naming-style enforcement. Ported verbatim from lintr's
/// `.base_s3_generics` (lintr 3.3.0): base R's `.knownS3Generics`, the group
/// generics (`Ops`, `Math`, `Summary`, `Complex` and their members, including
/// the operator generics like `+` that make `` `+.foo` `` exempt), and the
/// S3 generics exported by the base and stats namespaces. If users define
/// their own generic in another file and want methods exempt, they can
/// suppress the line with `# nolint` or `# raven: ignore` (alias
/// `# @lsp-ignore`); generics declared in the *same* file are recognized via
/// `collect_declared_s3_generics`.
pub(crate) fn is_known_s3_generic(name: &str) -> bool {
    // Byte-sorted so `binary_search` works.
    const GENERICS: &[&str] = &[
        "!",
        "!=",
        "$",
        "$<-",
        "%%",
        "%*%",
        "%/%",
        "&",
        "*",
        "+",
        "-",
        "/",
        "<",
        "<=",
        "==",
        ">",
        ">=",
        "AIC",
        "Arg",
        "Complex",
        "Conj",
        "Im",
        "Math",
        "Mod",
        "Ops",
        "Re",
        "Summary",
        "[",
        "[<-",
        "[[",
        "[[<-",
        "^",
        "abs",
        "acos",
        "acosh",
        "add1",
        "all",
        "all.equal",
        "anova",
        "any",
        "anyDuplicated",
        "anyNA",
        "aperm",
        "as.Date",
        "as.POSIXct",
        "as.POSIXlt",
        "as.array",
        "as.call",
        "as.character",
        "as.complex",
        "as.data.frame",
        "as.double",
        "as.environment",
        "as.expression",
        "as.function",
        "as.integer",
        "as.list",
        "as.logical",
        "as.matrix",
        "as.null",
        "as.numeric",
        "as.raw",
        "as.single",
        "as.table",
        "as.vector",
        "asin",
        "asinh",
        "atan",
        "atanh",
        "biplot",
        "by",
        "c",
        "cbind",
        "ceiling",
        "chol",
        "chooseOpsMethod",
        "close",
        "coef",
        "conditionCall",
        "conditionMessage",
        "confint",
        "contour",
        "cos",
        "cosh",
        "cospi",
        "crossprod",
        "cummax",
        "cummin",
        "cumprod",
        "cumsum",
        "cut",
        "determinant",
        "deviance",
        "df.residual",
        "diff",
        "digamma",
        "dim",
        "dim<-",
        "dimnames",
        "dimnames<-",
        "drop1",
        "droplevels",
        "duplicated",
        "edit",
        "exp",
        "expm1",
        "extractAIC",
        "fitted",
        "floor",
        "flush",
        "format",
        "formula",
        "gamma",
        "getDLLRegisteredRoutines",
        "hist",
        "identify",
        "image",
        "is.array",
        "is.finite",
        "is.infinite",
        "is.matrix",
        "is.na",
        "is.na<-",
        "is.nan",
        "is.numeric",
        "isSymmetric",
        "julian",
        "kappa",
        "labels",
        "length",
        "length<-",
        "levels",
        "levels<-",
        "lgamma",
        "lines",
        "log",
        "log10",
        "log1p",
        "log2",
        "logLik",
        "matrixOps",
        "max",
        "mean",
        "merge",
        "min",
        "model.frame",
        "model.matrix",
        "months",
        "mtfrm",
        "nameOfClass",
        "names",
        "names<-",
        "open",
        "pairs",
        "plot",
        "points",
        "predict",
        "pretty",
        "print",
        "prod",
        "profile",
        "qqnorm",
        "qr",
        "quarters",
        "range",
        "rbind",
        "rep",
        "residuals",
        "rev",
        "round",
        "row.names",
        "row.names<-",
        "rowsum",
        "scale",
        "se.contrast",
        "seek",
        "seq",
        "seq.int",
        "sequence",
        "sign",
        "signif",
        "sin",
        "sinh",
        "sinpi",
        "solve",
        "sort",
        "sort_by",
        "split",
        "split<-",
        "sqrt",
        "str",
        "subset",
        "sum",
        "summary",
        "t",
        "tan",
        "tanh",
        "tanpi",
        "tcrossprod",
        "terms",
        "text",
        "toString",
        "transform",
        "trigamma",
        "trunc",
        "truncate",
        "unique",
        "units",
        "units<-",
        "update",
        "vcov",
        "weekdays",
        "with",
        "within",
        "xtfrm",
        "|",
    ];
    GENERICS.binary_search(&name).is_ok()
}

fn matches_scheme(name: &str, style: ObjectNameStyle) -> bool {
    // `symbols` (lintr): a name made up entirely of non-alphanumeric
    // characters, e.g. `+` or `%>%` stripped to `>`. Checked before the
    // ASCII guard and the leading-dot handling — both would misjudge
    // symbol-only names (`...` starts with a dot; `€` isn't ASCII).
    if style == ObjectNameStyle::Symbols {
        return !name.chars().any(char::is_alphanumeric);
    }
    if !name.is_ascii() {
        // The ASCII schemes can never classify a non-ASCII name. Returning
        // false matters in combined style+regex configurations, where
        // `should_skip_name` lets non-ASCII names through so the regexes can
        // judge them — a style must not auto-accept what it cannot classify.
        // Configurations without regexes never reach here (skipped earlier).
        return false;
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
        // Handled before the leading-dot normalization above.
        ObjectNameStyle::Symbols => unreachable!("symbols handled before dot-stripping"),
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
        ObjectNameStyle::Symbols => "symbols",
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

    /// [`should_skip_name`] under a typical style-based configuration —
    /// the carve-outs exercised by most tests don't depend on the patterns.
    fn skip_default(name: &str) -> bool {
        should_skip_name(
            name,
            KindPatterns {
                styles: &[ObjectNameStyle::SnakeCase],
                regexes: &[],
            },
            &HashSet::new(),
        )
    }

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
    fn s3_method_detected_for_every_symbol_kind() {
        // Prefix is a known base R generic — exempt. Like lintr, the
        // exemption is kind-independent (lintr runs one generic check over
        // all assignment targets and formals).
        assert!(skip_default("print.MyClass"));
        assert!(skip_default("format.Date"));
        assert!(skip_default("summary.lm"));
        // All-lowercase dotted name with unknown prefix is still checked.
        assert!(!skip_default("my.func"));
        // Unknown prefix + capitalized suffix (regression for over-broad
        // exemption): `foo` is not a known generic, so `foo.Bar` is checked.
        assert!(!skip_default("foo.Bar"));
        // The class part must be non-empty — `print.` is not dispatch.
        assert!(!skip_default("print."));
    }

    #[test]
    fn s3_method_detection_handles_dotted_generics() {
        // Regression: `as.Date.character` is a method of generic `as.Date`
        // for class `character`. Previously the prefix-before-first-dot
        // lookup gave `"as"` (not in the list), so the method was wrongly
        // flagged. The progressive-prefix scan tries `as`, then `as.Date`,
        // and exempts on the second.
        assert!(skip_default("as.Date.character"));
        assert!(skip_default("as.numeric.foo"));
        assert!(skip_default("is.numeric.MyClass"));
        assert!(skip_default("all.equal.default"));
        // `is.character` is a primitive, not an S3 generic — lintr's
        // `.base_s3_generics` omits it and real lintr flags this name.
        assert!(!skip_default("is.character.MyClass"));
        // Class names containing dots also work because the leftmost matching
        // generic wins.
        assert!(skip_default("print.data.frame"));
        // Generic name itself (no class suffix) still requires at least one
        // dot to be considered S3 — bare `as.Date` defining the generic is
        // checked by the scheme (and would pass `dotted.case`).
    }

    #[test]
    fn s3_method_detection_handles_operator_generics() {
        // lintr's `.base_s3_generics` includes the operator generics, so
        // stripped operator-overload methods like `+.foo` (written
        // `` `+.foo` `` in source) are exempt.
        assert!(skip_default("+.foo"));
        assert!(skip_default("==.myclass"));
        assert!(skip_default("[.myclass"));
        assert!(skip_default("%%.myclass"));
        assert!(skip_default("$<-.myclass"));
    }

    #[test]
    fn s3_method_detection_handles_hidden_methods() {
        // Hidden S3 methods (`.print.MyClass`) — a leading `.` is stripped
        // before the generic lookup, so `.print.MyClass` still resolves
        // through `print`.
        assert!(skip_default(".print.MyClass"));
        assert!(skip_default(".as.Date.character"));
        // `.foo.Bar` — `foo` is not a generic, so still flagged.
        assert!(!skip_default(".foo.Bar"));
    }

    #[test]
    fn special_functions_always_exempt() {
        // lintr's `special_funs`: namespace and session hooks.
        assert!(skip_default(".onLoad"));
        assert!(skip_default(".onAttach"));
        assert!(skip_default(".Last.lib"));
        assert!(skip_default(".First"));
        assert!(skip_default("..."));
        // Near-misses are still checked.
        assert!(!skip_default(".onload"));
        assert!(!skip_default("onLoad"));
    }

    #[test]
    fn strip_name_mirrors_lintr() {
        assert_eq!(strip_name("`myBadName`"), "myBadName");
        assert_eq!(strip_name("`with spaces`"), "with spaces");
        assert_eq!(strip_name("`+.foo`"), "+.foo");
        assert_eq!(strip_name("`%+%`"), "+");
        assert_eq!(strip_name("`%%`"), "");
        assert_eq!(strip_name("`height<-`"), "height");
        assert_eq!(strip_name("\"quoted\""), "quoted");
        assert_eq!(strip_name("plain"), "plain");
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
    fn non_ascii_names_are_skipped_for_style_configs_only() {
        // Style-based configurations skip non-ASCII names (the ASCII schemes
        // can't classify them)...
        assert!(skip_default("\u{03b1}"));
        // ...but regex-only configurations check them against the regexes:
        // the user's patterns are the entire policy.
        let regexes = [CompiledRegex::new("^[a-z]+$").unwrap()];
        let regex_only = KindPatterns {
            styles: &[],
            regexes: &regexes,
        };
        assert!(!should_skip_name("\u{03b1}", regex_only, &HashSet::new()));
        assert!(!matches_patterns("\u{e9}Bad", regex_only));
        // Combined style+regex configurations also check non-ASCII names —
        // the regexes may exist precisely to govern Unicode identifiers, and
        // the ASCII styles never match them.
        let styles = [ObjectNameStyle::SnakeCase];
        let combined = KindPatterns {
            styles: &styles,
            regexes: &regexes,
        };
        assert!(!should_skip_name("\u{e9}Bad", combined, &HashSet::new()));
        assert!(!matches_patterns("\u{e9}Bad", combined));
    }

    #[test]
    fn symbols_style_matches_operator_names() {
        // lintr: names made up entirely of non-alphanumeric characters.
        assert!(matches_scheme("+", ObjectNameStyle::Symbols));
        assert!(matches_scheme("<=>", ObjectNameStyle::Symbols));
        assert!(matches_scheme("...", ObjectNameStyle::Symbols));
        assert!(!matches_scheme("m+", ObjectNameStyle::Symbols));
        assert!(!matches_scheme("foo", ObjectNameStyle::Symbols));
    }

    /// Run the full rule with the default (lintr-parity) configuration:
    /// snake_case + symbols for every kind.
    fn lint_default(text: &str) -> Vec<Diagnostic> {
        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        let styles = [ObjectNameStyle::SnakeCase, ObjectNameStyle::Symbols];
        let kind = KindPatterns {
            styles: &styles,
            regexes: &[],
        };
        let styles = ObjectNameStyles {
            function: kind,
            variable: kind,
            argument: kind,
        };
        let mut diags = Vec::new();
        collect(
            text,
            tree.root_node(),
            styles,
            DiagnosticSeverity::INFORMATION,
            &Suppressions::default(),
            &mut diags,
        );
        diags
    }

    #[test]
    fn backtick_quoted_names_are_stripped_and_checked() {
        // Issue #599: lintr strips backticks and applies the styles — a
        // backticked bad name lints exactly like the unquoted spelling.
        let diags = lint_default("`myBadName` <- 1\n");
        assert_eq!(diags.len(), 1, "got {diags:?}");
        assert!(diags[0].message.contains("`myBadName`"), "{diags:?}");

        let diags = lint_default("`my bad name` <- 1\n");
        assert_eq!(diags.len(), 1, "got {diags:?}");

        // Conforming backticked names pass.
        assert!(lint_default("`my_var` <- 1\n").is_empty());
    }

    #[test]
    fn operator_overloads_pass_under_default_styles() {
        // `%+%` strips to `+`, which the default `symbols` style accepts.
        assert!(lint_default("`%+%` <- function(a, b) a\n").is_empty());
        // S3 operator methods are exempt via the operator generics.
        assert!(lint_default("`+.foo` <- function(e1, e2) e1\n").is_empty());
        // ...but alphanumeric-containing operators lint (lintr's own doc
        // example: `%m+%` is flagged).
        let diags = lint_default("`%m+%` <- function(a, b) a\n");
        assert_eq!(diags.len(), 1, "got {diags:?}");
    }

    #[test]
    fn replacement_functions_check_base_name() {
        // `height<-` strips the trailing `<-` and checks `height`.
        assert!(lint_default("`height<-` <- function(x, value) x\n").is_empty());
        let diags = lint_default("`badHeight<-` <- function(x, value) x\n");
        assert_eq!(diags.len(), 1, "got {diags:?}");
    }

    #[test]
    fn name_stripping_to_empty_is_skipped() {
        assert!(lint_default("`%%` <- function(a, b) a\n").is_empty());
    }

    #[test]
    fn compound_lhs_checks_leftmost_object() {
        // lintr lints `a` in `a$b$c <- 1`; subscripted targets stay exempt.
        assert_eq!(lint_default("myBadObj$field <- 1\n").len(), 1);
        assert!(lint_default("good_obj$Field <- 1\n").is_empty());
        assert!(lint_default("x[[i]] <- 1\n").is_empty());
        assert!(lint_default("x[i]$BadName <- 1\n").is_empty());
    }

    #[test]
    fn binding_calls_check_their_literal_names() {
        assert_eq!(lint_default("assign(\"myBadName\", 2)\n").len(), 1);
        assert_eq!(
            lint_default("setGeneric(\"fooBar\", function(x) standardGeneric(\"fooBar\"))\n").len(),
            1
        );
        assert!(lint_default("assign(\"good_name\", 2)\n").is_empty());
        assert!(lint_default("base::assign(\"good_name\", 2)\n").is_empty());
        // Non-literal names are not checked.
        assert!(lint_default("assign(name_var, 2)\n").is_empty());
    }

    #[test]
    fn quoted_string_targets_are_checked() {
        // lintr lints STR_CONST assignment targets after stripping quotes.
        let diags = lint_default("\"myBadString\" <- 1\n");
        assert_eq!(diags.len(), 1, "got {diags:?}");
        assert!(lint_default("\"good_name\" <- 1\n").is_empty());
        // Idiomatic quoted S3 operator methods stay exempt via the operator
        // generics table.
        assert!(lint_default("\"[.Surv\" <- function(x, i) x\n").is_empty());
        // Replacement-function methods (`coef<-.varPower`) are flagged —
        // matching real lintr, whose generics table has `coef` but not
        // `coef<-`, and whose styles reject the `<` and `-` characters.
        assert_eq!(
            lint_default("\"coef<-.varPower\" <- function(x, value) x\n").len(),
            1
        );
        // Compound string LHS (`a$"b" <- 1`) is still exempt: the binop LHS
        // is the `$`-extract node, not the string.
        assert!(lint_default("a$\"myBadString\" <- 1\n").is_empty());
    }

    #[test]
    fn declared_generic_methods_are_exempt() {
        // `myGeneric` doesn't match snake_case but is a declared generic;
        // lintr exempts methods of same-file `UseMethod` generics. The
        // generic's own definition still lints.
        let text = "\
my_generic <- function(x) UseMethod(\"my_generic\")\n\
my_generic.myClass <- function(x) 1\n";
        assert!(
            lint_default(text).is_empty(),
            "got {:?}",
            lint_default(text)
        );

        // Without the UseMethod declaration, the dotted method name lints.
        let undeclared = "my_generic.myClass <- function(x) 1\n";
        assert_eq!(lint_default(undeclared).len(), 1);
    }
}
