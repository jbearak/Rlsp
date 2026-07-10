//! Native style/lint diagnostics.
//!
//! Implements a small set of `lintr`-equivalent rules natively against the
//! tree-sitter AST and raw text. No R subprocess; rules run in microseconds on
//! the already-parsed tree.
//!
//! Scope:
//! * `line_length` — flag lines wider than the configured maximum.
//! * `trailing_whitespace` — trailing spaces/tabs at end of line (inside
//!   multi-line strings is exempt, like lintr's `allow_in_strings`).
//! * `no_tab` — tabs used for indentation (tabs in comments, strings, or
//!   between tokens are left alone, matching `lintr::whitespace_linter`).
//! * `trailing_blank_lines` — blank lines at the very end of the file.
//! * `assignment_operator` — enforce the preferred assignment operator;
//!   also flags `->`/`->>`/`%<>%`, with lintr's implicit-assignment
//!   exclusion for call arguments and conditions.
//! * `object_name` — enforce accepted naming styles and regex patterns on
//!   assignment targets and function arguments.
//! * `infix_spaces` — flag missing spaces around lintr's low-precedence
//!   operator set (including named-argument and formal-default `=`);
//!   high-precedence operators (`^`, `:`, `::`, `$`, `@`) and unary forms
//!   are never linted.
//! * `commented_code` — flag comment blocks and end-of-line comments whose body parses as R
//!   and contains a call, assignment, operator, or function definition. This
//!   rule re-parses each candidate comment body via the thread-local parser
//!   pool. The same parser pool is also exercised by the suppression parser
//!   on the rare commented-code line that carries an inline `# nolint` (see
//!   below).
//! * `quotes` — flag string literals not using the configured delimiter (`"`
//!   or `'`), raw strings included; a literal containing the preferred
//!   quote character is exempt (switching would force escaping).
//! * `commas` — flag whitespace before `,` and missing whitespace after `,`
//!   (newline after is fine).
//! * `t_and_f_symbol` — flag bare `T` / `F` identifiers used as references
//!   to `TRUE` / `FALSE` (assignment targets get a dedicated variable-name
//!   message). Named arguments, formal parameters, `$` / `@` field names,
//!   formula terms, subscripted uses, and callees are exempt.
//! * `semicolon` — flag `;` statement separators in source. Unlike the other
//!   rules, this one byte-scans the raw source (skipping every leaf-token
//!   range: strings, comments, backticked identifiers, `%…%` operators)
//!   because tree-sitter-r does not emit `;` as a node.
//! * `equals_na` — flag `x == NA`, `x != NA`, and the typed `NA_*` variants
//!   on either side, plus `x %in% NA`.
//! * `object_length` — flag identifier names longer than the configured
//!   maximum length.
//! * `vector_logic` — flag `&` / `|` in `if` / `while` (and
//!   `expect_true`/`expect_false`) conditions, and the mirror `&&` / `||`
//!   inside `subset()`/`filter()` arguments; call boundaries stop the
//!   condition scan and bitwise-arithmetic operands are exempt.
//! * `mixed_logical` — flag `|` / `||` whose immediate operand is a bare
//!   `&` / `&&` (without parentheses), e.g. `a & b | c`. `&` binds tighter
//!   than `|` in R, making the grouping easy to mis-read; adding parentheses
//!   makes the intent explicit.
//! * `condition_assignment` — flag `=` used as a binary operator directly
//!   inside an `if` or `while` condition. R rejects `if (x = 1)` as a
//!   syntax error at runtime but tree-sitter-r accepts it silently; use `==`
//!   for equality tests and `<-` for assignment.
//! * `function_left_parentheses` — flag whitespace between `function`
//!   (or `\`) and `(`, and between a call's function name and its `(`.
//! * `spaces_inside` — flag whitespace immediately inside `(`, `[`, `[[`
//!   and their closers (whitespace-only groupings flag both sides);
//!   multi-line wrapping, comma/`= )` neighbors, and comment-after-opener
//!   are exempt.
//! * `indentation` — flag lines whose leading whitespace doesn't match the
//!   expected indent, using lintr's accumulated indent-change model (tidy
//!   style); Raven additionally accepts the aligned/block/chain-start forms
//!   the on-type formatter produces.
//!
//! Implementation note: most rules walk the already-parsed tree directly.
//! `commented_code` re-parses each candidate comment body; `semicolon`
//! byte-scans the source text using AST ranges only to skip strings and
//! comments. `indentation` walks the tree to compute per-line scope
//! expectations and also scans line text to count leading whitespace and
//! detect mixed tabs.
//!
//! Suppression supports both lintr and Raven conventions:
//! * `# nolint` (with optional `: rule_a, rule_b` filter) suppresses the line.
//! * `# nolint start` / `# nolint end` brackets a region.
//! * `# raven: ignore` (alias `# @lsp-ignore`) suppresses the line it appears
//!   on.
//! * `# raven: ignore-next` (alias `# @lsp-ignore-next`) suppresses the
//!   *following* source line.
//!
//! The `@lsp-` forms are permanent aliases that parse identically to the
//! canonical `# raven:` forms.
//!
//! Same-line markers (`# nolint`, `# nolint start/end`, `# @lsp-ignore`) are
//! additionally recognised when nested inside a commented-code line — e.g.
//! `# x <- 1 # nolint` — via a parse-gated fallback. See `nolint` for the
//! full pipeline and limits.

pub mod config;
mod nolint;
mod parse_gate;

/// Test-only re-export of the `nolint` string scanner so the cross-parser
/// parity property test in `cross_file::property_tests` can reach it. The
/// `nolint` module itself stays private to `linting`.
#[cfg(test)]
pub(crate) use nolint::{
    Suppressions as SuppressionsForParityTest, first_hash_body_for_parity_test,
};
#[cfg(test)]
mod lintr_parity;
pub mod rule_ids;
mod rules;

use tower_lsp::lsp_types::Diagnostic;
use tree_sitter::Node;

pub use self::config::{
    AssignmentOperatorStyle, CompiledRegex, LintConfig, LintEnabled, ObjectNameStyle,
    StringDelimiter,
};

/// Source identifier set on every diagnostic produced by this module.
///
/// Lets clients (and tests) distinguish lint diagnostics from cross-file or
/// syntax diagnostics, and gives the user a recognizable badge in the editor.
pub const LINT_SOURCE: &str = "raven (lint)";

/// Run all enabled lint rules against the given document.
///
/// `text` is the document text and `tree_root` is the tree-sitter root node.
/// Returns an empty `Vec` when `config.enabled` is false; individual rules are
/// gated by their per-rule severity inside [`LintConfig`].
pub fn run_lints(text: &str, tree_root: Node<'_>, config: &LintConfig) -> Vec<Diagnostic> {
    if !config.enabled {
        return Vec::new();
    }

    let suppressions = nolint::Suppressions::from_text(text);
    run_lints_with(text, tree_root, config, suppressions)
}

/// Same as [`run_lints`] but with **no** suppression filtering — every
/// violation is emitted regardless of `# nolint` / `# raven:` / `@lsp-ignore`
/// markers. Used by the `unused-suppression` sweep (F2 Step 3) to recover the
/// raw, pre-suppression lint diagnostics so it can tell which suppression
/// directives actually removed something.
pub fn run_lints_raw(text: &str, tree_root: Node<'_>, config: &LintConfig) -> Vec<Diagnostic> {
    if !config.enabled {
        return Vec::new();
    }
    run_lints_with(text, tree_root, config, nolint::Suppressions::default())
}

fn run_lints_with(
    text: &str,
    tree_root: Node<'_>,
    config: &LintConfig,
    suppressions: nolint::Suppressions,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    if let Some(sev) = config.line_length_severity {
        rules::line_length::collect(text, config.line_length, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.trailing_whitespace_severity {
        rules::trailing_whitespace::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.no_tab_severity {
        rules::no_tab::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.trailing_blank_lines_severity {
        rules::trailing_blank_lines::collect(text, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.assignment_operator_severity {
        rules::assignment_operator::collect(
            text,
            tree_root,
            config.assignment_operator_style,
            sev,
            &suppressions,
            &mut out,
        );
    }
    if let Some(sev) = config.object_name_severity {
        rules::object_name::collect(
            text,
            tree_root,
            rules::object_name::ObjectNameStyles {
                function: rules::object_name::KindPatterns {
                    styles: &config.object_name_style_function,
                    regexes: &config.object_name_regexes_function,
                },
                variable: rules::object_name::KindPatterns {
                    styles: &config.object_name_style_variable,
                    regexes: &config.object_name_regexes_variable,
                },
                argument: rules::object_name::KindPatterns {
                    styles: &config.object_name_style_argument,
                    regexes: &config.object_name_regexes_argument,
                },
            },
            sev,
            &suppressions,
            &mut out,
        );
    }
    if let Some(sev) = config.infix_spaces_severity {
        rules::infix_spaces::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.commented_code_severity {
        rules::commented_code::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.quotes_severity {
        rules::quotes::collect(
            text,
            tree_root,
            config.string_delimiter,
            sev,
            &suppressions,
            &mut out,
        );
    }
    if let Some(sev) = config.commas_severity {
        rules::commas::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.t_and_f_symbol_severity {
        rules::t_and_f_symbol::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.semicolon_severity {
        rules::semicolon::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.equals_na_severity {
        rules::equals_na::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.object_length_severity {
        rules::object_length::collect(
            text,
            tree_root,
            config.object_length,
            sev,
            &suppressions,
            &mut out,
        );
    }
    if let Some(sev) = config.vector_logic_severity {
        rules::vector_logic::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.function_left_parentheses_severity {
        rules::function_left_parentheses::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.spaces_inside_severity {
        rules::spaces_inside::collect(text, tree_root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = config.indentation_severity {
        rules::indentation::collect(
            text,
            tree_root,
            config.indentation_unit,
            sev,
            &suppressions,
            &mut out,
        );
    }

    out
}

/// Runs the always-on semantic checks that flag likely-wrong code regardless of
/// whether the style-lint master switch (`LintConfig::enabled`) is on.
///
/// These rules detect precedence bugs and runtime errors, not style preferences,
/// and belong in the main diagnostic pipeline. Callers should pass severity
/// values from `CrossFileConfig`.
pub fn run_semantic_checks(
    text: &str,
    root: Node<'_>,
    mixed_logical_severity: Option<tower_lsp::lsp_types::DiagnosticSeverity>,
    condition_assignment_severity: Option<tower_lsp::lsp_types::DiagnosticSeverity>,
) -> Vec<Diagnostic> {
    if mixed_logical_severity.is_none() && condition_assignment_severity.is_none() {
        return Vec::new();
    }
    let suppressions = nolint::Suppressions::from_text(text);
    run_semantic_checks_with(
        text,
        root,
        mixed_logical_severity,
        condition_assignment_severity,
        suppressions,
    )
}

/// Same as [`run_semantic_checks`] but with **no** suppression filtering. Used
/// by the `unused-suppression` sweep (F2 Step 3); see [`run_lints_raw`].
pub fn run_semantic_checks_raw(
    text: &str,
    root: Node<'_>,
    mixed_logical_severity: Option<tower_lsp::lsp_types::DiagnosticSeverity>,
    condition_assignment_severity: Option<tower_lsp::lsp_types::DiagnosticSeverity>,
) -> Vec<Diagnostic> {
    if mixed_logical_severity.is_none() && condition_assignment_severity.is_none() {
        return Vec::new();
    }
    run_semantic_checks_with(
        text,
        root,
        mixed_logical_severity,
        condition_assignment_severity,
        nolint::Suppressions::default(),
    )
}

fn run_semantic_checks_with(
    text: &str,
    root: Node<'_>,
    mixed_logical_severity: Option<tower_lsp::lsp_types::DiagnosticSeverity>,
    condition_assignment_severity: Option<tower_lsp::lsp_types::DiagnosticSeverity>,
    suppressions: nolint::Suppressions,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if let Some(sev) = mixed_logical_severity {
        rules::mixed_logical::collect(text, root, sev, &suppressions, &mut out);
    }
    if let Some(sev) = condition_assignment_severity {
        rules::condition_assignment::collect(text, root, sev, &suppressions, &mut out);
    }
    out
}

/// F2 Step 4: does a range- or file-level suppression in `meta` cover a
/// diagnostic at `line` with code `code`?
///
/// Block (`# raven: ignore-start/end`), file (`# raven: ignore-file`), and
/// chunk-level (`raven.ignore=…`, `# raven: ignore-chunk`) suppressions live in
/// [`CrossFileMetadata`](crate::cross_file::types::CrossFileMetadata)'s
/// `ignored_ranges` / `ignored_file`. Some diagnostics consult these inline
/// (the undefined-variable / missing-package collectors), but the lint track's
/// per-line `nolint` parser and the AST `assign-to-string-literal` collector do
/// not — so the diagnostics pipeline and `raven lint` call this as a final
/// filter to apply range/file suppression uniformly.
///
/// Restricted to the suppressible codes —
/// [`LINT_CODES`](crate::diagnostic_code::LINT_CODES) and
/// [`SUPPRESSIBLE_ANALYZER_CODES`](crate::diagnostic_code::SUPPRESSIBLE_ANALYZER_CODES)
/// — so parse errors and the dependency-graph diagnostics are never suppressed
/// by ignore directives. Removing an analyzer code that was already dropped
/// inline is a harmless no-op (it is no longer in the diagnostic list).
pub fn range_or_file_suppresses(
    meta: &crate::cross_file::types::CrossFileMetadata,
    line: u32,
    code: &str,
) -> bool {
    let norm = crate::diagnostic_code::normalize(code);
    let suppressible = crate::diagnostic_code::LINT_CODES.contains(&norm.as_str())
        || crate::diagnostic_code::SUPPRESSIBLE_ANALYZER_CODES.contains(&norm.as_str());
    if !suppressible {
        return false;
    }
    meta.ignored_file
        .as_ref()
        .is_some_and(|f| f.covers(Some(&norm)))
        || meta
            .ignored_ranges
            .iter()
            .any(|r| line >= r.start && line <= r.end && r.what.covers(Some(&norm)))
}

/// F2 Step 3: the `(line, kebab-code)` pairs that lint-track suppression
/// directives actually removed from `text`.
///
/// Recomputes the raw (pre-suppression) lint + semantic diagnostics and returns
/// those whose line is suppressed for their rule. The `unused-suppression`
/// sweep uses this — combined with the analyzer track's captured pairs — to
/// decide which directives suppressed something and which are unused. Keeping
/// the `nolint` dependency inside this module avoids exposing the suppression
/// parser to the rest of the crate.
pub fn suppressed_lint_pairs(
    text: &str,
    tree_root: Node<'_>,
    config: &LintConfig,
    mixed_logical_severity: Option<tower_lsp::lsp_types::DiagnosticSeverity>,
    condition_assignment_severity: Option<tower_lsp::lsp_types::DiagnosticSeverity>,
) -> Vec<(u32, String)> {
    let suppressions = nolint::Suppressions::from_text(text);
    let raw = run_lints_raw(text, tree_root, config)
        .into_iter()
        .chain(run_semantic_checks_raw(
            text,
            tree_root,
            mixed_logical_severity,
            condition_assignment_severity,
        ));
    let mut out = Vec::new();
    for d in raw {
        if let Some(tower_lsp::lsp_types::NumberOrString::String(code)) = &d.code {
            let line = d.range.start.line;
            let rule_id = crate::diagnostic_code::to_lint_rule_id(code);
            if suppressions.is_suppressed_code(line, &rule_id) {
                out.push((line, crate::diagnostic_code::normalize(code)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser_pool::with_parser;
    use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

    fn enabled_config() -> LintConfig {
        LintConfig {
            enabled: true,
            ..LintConfig::default()
        }
    }

    fn lint(text: &str, config: &LintConfig) -> Vec<Diagnostic> {
        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        run_lints(text, tree.root_node(), config)
    }

    fn lint_semantic(
        text: &str,
        mixed_sev: Option<DiagnosticSeverity>,
        cond_sev: Option<DiagnosticSeverity>,
    ) -> Vec<Diagnostic> {
        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        run_semantic_checks(text, tree.root_node(), mixed_sev, cond_sev)
    }

    #[test]
    fn master_switch_off_returns_empty() {
        let mut config = enabled_config();
        config.enabled = false;
        let diags = lint("x  \n", &config);
        assert!(diags.is_empty());
    }

    #[test]
    fn default_native_lint_severities_are_information() {
        let config = LintConfig::default();
        let expected = Some(DiagnosticSeverity::INFORMATION);

        assert_eq!(config.line_length_severity, expected);
        assert_eq!(config.trailing_whitespace_severity, expected);
        assert_eq!(config.no_tab_severity, expected);
        assert_eq!(config.trailing_blank_lines_severity, expected);
        assert_eq!(config.assignment_operator_severity, expected);
        assert_eq!(config.object_name_severity, expected);
        assert_eq!(config.infix_spaces_severity, expected);
        assert_eq!(config.commented_code_severity, expected);
        assert_eq!(config.quotes_severity, expected);
        assert_eq!(config.commas_severity, expected);
        assert_eq!(config.t_and_f_symbol_severity, expected);
        assert_eq!(config.semicolon_severity, expected);
        assert_eq!(config.equals_na_severity, expected);
        assert_eq!(config.object_length_severity, expected);
        assert_eq!(config.vector_logic_severity, expected);
        assert_eq!(config.function_left_parentheses_severity, expected);
        assert_eq!(config.spaces_inside_severity, expected);
        assert_eq!(config.indentation_severity, expected);
    }

    #[test]
    fn line_length_flags_only_overlong_lines() {
        let config = LintConfig {
            line_length: 10,
            ..enabled_config()
        };
        let diags = lint("short\nthis line is much too long\nshort\n", &config);
        let line_lints: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("characters long"))
            .collect();
        assert_eq!(line_lints.len(), 1);
        assert_eq!(line_lints[0].range.start.line, 1);
    }

    #[test]
    fn trailing_whitespace_is_flagged() {
        let config = LintConfig {
            line_length_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            assignment_operator_severity: None,
            ..enabled_config()
        };
        let diags = lint("x <- 1   \n", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("Trailing whitespace"));
        assert_eq!(diags[0].range.start.character, 6);
    }

    #[test]
    fn no_tab_flags_first_run_per_line() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            trailing_blank_lines_severity: None,
            assignment_operator_severity: None,
            ..enabled_config()
        };
        let diags = lint("\t\tx <- 1\n", &config);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.character, 0);
        assert_eq!(diags[0].range.end.character, 2);
    }

    #[test]
    fn trailing_blank_lines_flag_multiple() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            assignment_operator_severity: None,
            ..enabled_config()
        };
        let diags = lint("x <- 1\n\n\n", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("trailing blank"));
    }

    #[test]
    fn missing_final_newline_is_flagged() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            assignment_operator_severity: None,
            ..enabled_config()
        };
        let diags = lint("x <- 1", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("end with a newline"));
    }

    #[test]
    fn assignment_operator_flags_top_level_equals_when_left_arrow_preferred() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            ..enabled_config()
        };
        let diags = lint("x = 1\n", &config);
        let assign_lints: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("assignment"))
            .collect();
        assert_eq!(assign_lints.len(), 1);
    }

    #[test]
    fn assignment_operator_ignores_named_arguments() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            ..enabled_config()
        };
        // `n = 5` inside a call is a named argument, not assignment.
        let diags = lint("rnorm(n = 5)\n", &config);
        assert!(diags.is_empty(), "named-argument `=` must not be flagged");
    }

    #[test]
    fn assignment_operator_flags_inside_function_body_passed_as_arg() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            semicolon_severity: None,
            ..enabled_config()
        };
        // lintr's implicit-assignment exclusion: anything nested under a
        // call argument is skipped, so `y = x` inside the lambda body is
        // clean (verified against lintr 3.3.0.1). Semicolon-severity is
        // disabled in this fixture so the assertion can focus on the
        // assignment-operator rule.
        let diags = lint("lapply(xs, function(x) { y = x; y })\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn assignment_operator_flags_inside_braced_block_passed_as_arg() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            ..enabled_config()
        };
        // `{ y = 1 }` as a call argument falls under lintr's
        // implicit-assignment exclusion — clean. A parenthesized argument is
        // NOT excluded (`fun((blah = fun(1)))` stays flagged).
        let diags = lint("f({ y = 1 })\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
        let parenthesized = lint("fun((blah = fun(1)))\n", &config);
        assert_eq!(parenthesized.len(), 1, "got {:?}", parenthesized);
    }

    #[test]
    fn assignment_operator_flags_inside_if_body_passed_as_arg() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            ..enabled_config()
        };
        // The `if` sits inside a call argument, so lintr's
        // implicit-assignment exclusion covers its body too — clean.
        let diags = lint("f(if (cond) y = 1)\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
        // Braced if/while/for conditions are also excluded.
        assert!(lint("if ({a = TRUE}) 1\n", &config).is_empty());
        assert!(lint("while ({a = TRUE}) 1\n", &config).is_empty());
        assert!(lint("for (ii in {a = TRUE}) 1\n", &config).is_empty());
    }

    #[test]
    fn assignment_operator_flags_right_assign_and_assignment_pipe() {
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            ..enabled_config()
        };
        // lintr's default flags right assigns and the magrittr assignment
        // pipe; `<<-` is allowed (operator = c("<-", "<<-")).
        let right = lint("1 -> blah\n", &config);
        assert_eq!(right.len(), 1, "got {:?}", right);
        assert!(right[0].message.contains("`->`"));
        let cascade = lint("1 ->> blah\n", &config);
        assert_eq!(cascade.len(), 1, "got {:?}", cascade);
        let pipe = lint("x %<>% sum()\n", &config);
        assert_eq!(pipe.len(), 1, "got {:?}", pipe);
        assert!(pipe[0].message.contains("assignment pipe"));
        assert!(lint("x <<- 1\n", &config).is_empty());
    }

    #[test]
    fn assignment_operator_equals_style_flags_left_arrow() {
        let config = LintConfig {
            assignment_operator_style: AssignmentOperatorStyle::Equals,
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            ..enabled_config()
        };
        let diags = lint("x <- 1\n", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("Use `=`"));
    }

    #[test]
    fn nolint_line_suppresses_diagnostics_on_that_line() {
        let config = LintConfig {
            line_length: 10,
            line_length_severity: Some(DiagnosticSeverity::HINT),
            trailing_whitespace_severity: Some(DiagnosticSeverity::HINT),
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            assignment_operator_severity: None,
            ..enabled_config()
        };
        // Overlong + trailing whitespace, but suppressed.
        let diags = lint("this line is very long   # nolint\n", &config);
        assert!(diags.is_empty(), "nolint comment must suppress lints");
    }

    #[test]
    fn nolint_block_suppresses_a_range() {
        let config = LintConfig {
            line_length: 5,
            line_length_severity: Some(DiagnosticSeverity::HINT),
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            assignment_operator_severity: None,
            ..enabled_config()
        };
        let src = "longline_outside\n# nolint start\nlongline_inside_a\nlongline_inside_b\n# nolint end\nlongline_after\n";
        let diags = lint(src, &config);
        let lines: Vec<u32> = diags.iter().map(|d| d.range.start.line).collect();
        assert!(lines.contains(&0));
        assert!(!lines.contains(&2));
        assert!(!lines.contains(&3));
        assert!(lines.contains(&5));
    }

    fn object_name_only_config() -> LintConfig {
        LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            assignment_operator_severity: None,
            infix_spaces_severity: None,
            commented_code_severity: None,
            indentation_severity: None,
            ..enabled_config()
        }
    }

    #[test]
    fn object_name_flags_camelcase_variable_under_snake_case() {
        let config = object_name_only_config();
        let diags = lint("myVar <- 1\n", &config);
        assert_eq!(diags.len(), 1, "expected one diagnostic, got {:?}", diags);
        assert!(diags[0].message.contains("Variable name `myVar`"));
        assert!(diags[0].message.contains("snake_case"));
    }

    #[test]
    fn object_name_accepts_snake_case() {
        let config = object_name_only_config();
        let diags = lint(
            "my_var <- 1\nmy_func <- function(x_arg, y_arg) x_arg + y_arg\n",
            &config,
        );
        assert!(
            diags.is_empty(),
            "snake_case names should pass: {:?}",
            diags
        );
    }

    #[test]
    fn object_name_flags_function_name() {
        let config = object_name_only_config();
        let diags = lint("MyFunc <- function() 1\n", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("Function name `MyFunc`"));
    }

    #[test]
    fn object_name_flags_argument_names() {
        let config = object_name_only_config();
        let diags = lint("f <- function(badArg, goodArg, other) 1\n", &config);
        // Two camelCase args should be flagged; `other` is fine.
        let arg_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("Argument name"))
            .collect();
        assert_eq!(arg_diags.len(), 2, "got {:?}", diags);
    }

    #[test]
    fn object_name_exempts_s3_method_dispatch() {
        let config = object_name_only_config();
        // `print.MyClass <- function(x, ...) ...` is S3 dispatch; the function
        // name must not be flagged even though it isn't snake_case.
        let diags = lint("print.MyClass <- function(x, ...) NULL\n", &config);
        let fn_diags: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains("Function name"))
            .collect();
        assert!(
            fn_diags.is_empty(),
            "S3 method should be exempt: {:?}",
            diags
        );
    }

    #[test]
    fn object_name_flags_dotted_function_with_unknown_generic_prefix() {
        let config = object_name_only_config();
        // `foo` is not a known base R S3 generic, so `foo.Bar` is *not*
        // auto-exempted — the user gets a snake_case violation instead of a
        // silent pass. Regression for the original over-broad heuristic that
        // exempted any function name with an uppercase post-dot suffix.
        let diags = lint("foo.Bar <- function() 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("Function name `foo.Bar`"));
    }

    #[test]
    fn object_name_accepts_leading_dot_hidden_names() {
        let config = object_name_only_config();
        // R uses a leading `.` to mark "hidden" identifiers; lintr accepts
        // it as decorative on every scheme. Don't flag `.foo`, `.my_var`, or
        // `.helper <- function(...)`.
        let diags = lint(
            ".hidden_var <- 1\n.another_var <- 2\n.helper <- function(x_arg) x_arg\n",
            &config,
        );
        assert!(
            diags.is_empty(),
            "leading-dot names should be allowed: {:?}",
            diags
        );
    }

    #[test]
    fn object_name_still_flags_non_dispatch_dotted_function() {
        let config = object_name_only_config();
        // All-lowercase dotted name is *not* method dispatch; under snake_case
        // it's still a violation (the dot is not a snake_case separator).
        let diags = lint("my.func <- function() 1\n", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("Function name `my.func`"));
    }

    #[test]
    fn object_name_skips_named_arguments() {
        let config = object_name_only_config();
        // `myArg` is a parameter name (flag), and `someArg = 1` inside the
        // function call is a named argument (no flag). Net: one diagnostic.
        let diags = lint("f <- function(myArg) g(someArg = 1)\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("Argument name `myArg`"));
    }

    #[test]
    fn object_name_skips_compound_lhs() {
        let config = object_name_only_config();
        // `obj$badName <- 1` doesn't introduce a new symbol — just updates a
        // field. Should not flag.
        let diags = lint("obj$badName <- 1\n", &config);
        assert!(
            diags.is_empty(),
            "compound LHS should be skipped: {:?}",
            diags
        );
    }

    #[test]
    fn object_name_strips_backticks_and_checks_the_name() {
        // Issue #599: lintr strips backticks and applies the styles, so a
        // backticked non-conforming name lints like its unquoted spelling.
        let config = object_name_only_config();
        let diags = lint("`with spaces` <- 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("`with spaces`"));

        let bad = lint("`myBadName` <- 1\n", &config);
        assert_eq!(bad.len(), 1, "got {:?}", bad);

        assert!(lint("`my_var` <- 1\n", &config).is_empty());
        // Operator overloads strip to pure-symbol names, which the default
        // `symbols` style accepts; S3 operator methods hit the generic table.
        assert!(lint("`%+%` <- function(a, b) a\n", &config).is_empty());
        assert!(lint("`+.foo` <- function(e1, e2) e1\n", &config).is_empty());
    }

    #[test]
    fn object_name_camel_case_style_flags_snake_case() {
        let mut config = object_name_only_config();
        config.object_name_style_variable = vec![ObjectNameStyle::CamelCase];
        let diags = lint("my_var <- 1\n", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("camelCase"));
    }

    #[test]
    fn object_name_any_disables_specific_kind() {
        let mut config = object_name_only_config();
        config.object_name_style_function = vec![ObjectNameStyle::Any];
        // Variable is still checked, function is not.
        let diags = lint("BadName <- function() badVar <- 1\n", &config);
        // BadName (function): exempt; badVar (variable inside function): flagged.
        let fn_diags = diags
            .iter()
            .filter(|d| d.message.contains("Function name"))
            .count();
        let var_diags = diags
            .iter()
            .filter(|d| d.message.contains("Variable name"))
            .count();
        assert_eq!(fn_diags, 0);
        assert_eq!(var_diags, 1);
    }

    #[test]
    fn object_name_respects_arrow_right_assignment() {
        let config = object_name_only_config();
        let diags = lint("1 -> BadName\n", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("Variable name `BadName`"));
    }

    #[test]
    fn object_name_respects_nolint_marker() {
        let config = object_name_only_config();
        let diags = lint("BadName <- 1 # nolint\n", &config);
        assert!(diags.is_empty(), "nolint should suppress: {:?}", diags);
    }

    #[test]
    fn object_name_respects_lsp_ignore_marker() {
        let config = object_name_only_config();
        // Same-line `# @lsp-ignore` and next-line `# @lsp-ignore-next`
        // markers must suppress object-name diagnostics, same as `# nolint`.
        // Sanity check that the new rule is wired through the shared
        // `Suppressions` infrastructure.
        let diags = lint(
            "BadOne <- 1 # @lsp-ignore\n# @lsp-ignore-next\nBadTwo <- 2\nBadThree <- 3\n",
            &config,
        );
        let lines: Vec<u32> = diags.iter().map(|d| d.range.start.line).collect();
        assert!(
            !lines.contains(&0),
            "@lsp-ignore should suppress line 0: {:?}",
            diags
        );
        assert!(
            !lines.contains(&2),
            "@lsp-ignore-next should suppress line 2: {:?}",
            diags
        );
        assert!(
            lines.contains(&3),
            "unsuppressed line 3 should still flag: {:?}",
            diags
        );
    }

    #[test]
    fn object_name_exempts_methods_of_dotted_generics() {
        let config = object_name_only_config();
        // Regression test for the unreachable-GENERICS bug: methods of
        // generics that themselves contain dots (`as.Date`, `is.numeric`,
        // `all.equal`, `fitted.values`) must be exempt. Previously the
        // prefix-before-first-dot lookup yielded `"as"` / `"is"` / `"all"`
        // (none in the allowlist), false-flagging legitimate S3 methods.
        let src = "\
as.Date.character <- function(x, ...) NULL
as.numeric.MyClass <- function(x) NULL
is.numeric.foo <- function(x) NULL
all.equal.MyModel <- function(target, current, ...) NULL
print.data.frame <- function(x, ...) NULL
";
        let diags = lint(src, &config);
        assert!(
            diags.is_empty(),
            "methods of dotted/multi-part generics must be exempt: {:?}",
            diags
        );
    }

    #[test]
    fn object_name_severity_off_disables_rule() {
        let mut config = object_name_only_config();
        config.object_name_severity = None;
        let diags = lint("BadName <- 1\n", &config);
        assert!(diags.is_empty());
    }

    #[test]
    fn diagnostics_carry_lint_source() {
        let config = LintConfig {
            line_length: 5,
            ..enabled_config()
        };
        let diags = lint("longline\n", &config);
        assert!(!diags.is_empty());
        for d in &diags {
            assert_eq!(d.source.as_deref(), Some(LINT_SOURCE));
        }
    }

    /// Every style rule disabled (semantic checks are separate). Single-rule
    /// test configs build on this so a diagnostic can only come from the rule
    /// under test — new rules default on in `enabled_config()` and would
    /// otherwise contaminate "only" configs written before they existed.
    fn all_lint_rules_off() -> LintConfig {
        LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            assignment_operator_severity: None,
            object_name_severity: None,
            infix_spaces_severity: None,
            commented_code_severity: None,
            quotes_severity: None,
            commas_severity: None,
            t_and_f_symbol_severity: None,
            semicolon_severity: None,
            equals_na_severity: None,
            object_length_severity: None,
            vector_logic_severity: None,
            function_left_parentheses_severity: None,
            spaces_inside_severity: None,
            indentation_severity: None,
            ..enabled_config()
        }
    }

    fn infix_spaces_only_config() -> LintConfig {
        LintConfig {
            infix_spaces_severity: Some(DiagnosticSeverity::INFORMATION),
            ..all_lint_rules_off()
        }
    }

    #[test]
    fn infix_spaces_flags_missing_spaces_around_plus() {
        let config = infix_spaces_only_config();
        let diags = lint("x <- 1+2\n", &config);
        assert_eq!(
            diags.len(),
            2,
            "expected 2 diagnostics (before+after), got {:?}",
            diags
        );
        for d in &diags {
            assert!(d.message.contains("space"), "msg: {}", d.message);
            assert!(d.message.contains("`+`"), "msg: {}", d.message);
        }
    }

    #[test]
    fn infix_spaces_flags_only_missing_side() {
        let config = infix_spaces_only_config();
        let diags = lint("x <- 1 +2\n", &config);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("after"));
    }

    #[test]
    fn infix_spaces_does_not_flag_correct_spacing() {
        let config = infix_spaces_only_config();
        let diags = lint("x <- a + b * c / d ^ e\n", &config);
        assert!(diags.is_empty(), "expected no diagnostics, got {:?}", diags);
    }

    #[test]
    fn infix_spaces_does_not_flag_alignment_whitespace() {
        // Multiple spaces around an operator are allowed — alignment is a
        // common pattern and collapsing it would be more annoying than helpful.
        let config = infix_spaces_only_config();
        let diags = lint("x   <-   1\n", &config);
        assert!(
            diags.is_empty(),
            "alignment spaces must not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn infix_spaces_never_flags_high_precedence_operators() {
        // lintr's `infix_metadata$low_precedence` filter skips the
        // high-precedence operators entirely — `^`/`**`, `:`, `::`, `$`, `@`,
        // `?` are not linted in either direction (issue #600's `x^2` case).
        let config = infix_spaces_only_config();
        for text in [
            "y <- x^2\n",
            "y <- x ^ 2\n",
            "y <- x ^2\n",
            "y <- x**2\n",
            "y <- x ** 2\n",
            "xs <- 1:10\n",
            "xs <- 1 : 10\n",
            "x <- pkg::fun()\n",
            "x <- obj $ field\n",
            "x <- obj$field\n",
            "x <- obj @ slot\n",
            "q ? r\n",
        ] {
            let diags = lint(text, &config);
            assert!(
                diags.is_empty(),
                "high-precedence operator in {text:?} must not be flagged: {diags:?}"
            );
        }
    }

    #[test]
    fn infix_spaces_never_flags_unary_operators() {
        // lintr requires two operands (`count(expr) > 1`), so unary `-`, `+`,
        // `!`, and `~` are exempt — spaced or tight.
        let config = infix_spaces_only_config();
        for text in [
            "y <- -x\n",
            "y <- - x\n",
            "y <- ! x\n",
            "y <- !x\n",
            "a[-1]\n",
            "a[- 1]\n",
            "b <- 2E+4\n",
            "a <- 1e-3\n",
            "a[-1 + 1]\n",
            "a[1 + -1]\n",
        ] {
            let diags = lint(text, &config);
            assert!(
                diags.is_empty(),
                "unary operator in {text:?} must not be flagged: {diags:?}"
            );
        }
    }

    #[test]
    fn infix_spaces_flags_binary_in_mixed_unary_binary_expression() {
        // lintr: `-1-1` lints the *binary* minus only (column 3).
        let config = infix_spaces_only_config();
        let diags = lint("-1-1\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
        assert!(diags.iter().all(|d| d.message.contains("`-`")));
        assert!(diags.iter().all(|d| d.range.start.character == 2));
    }

    #[test]
    fn infix_spaces_flags_walrus_assignment_without_spaces() {
        // data.table's `:=` maps to lintr's LEFT_ASSIGN entry.
        let config = infix_spaces_only_config();
        let diags = lint("dt[, a:=1]\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
        assert!(diags.iter().all(|d| d.message.contains("`:=`")));
    }

    #[test]
    fn infix_spaces_does_not_flag_binary_minus() {
        let config = infix_spaces_only_config();
        // `a - b` with binary minus and spaces — fine.
        let diags = lint("y <- a - b\n", &config);
        assert!(
            diags.is_empty(),
            "binary `-` with spaces must pass: {:?}",
            diags
        );
    }

    #[test]
    fn infix_spaces_flags_named_argument_eq() {
        // lintr lints EQ_SUB by default: `fun(a=1)` is flagged.
        let config = infix_spaces_only_config();
        let diags = lint("f(name=value)\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
        assert!(diags.iter().all(|d| d.message.contains("`=`")));

        let one_side = lint("f(a =1)\n", &config);
        assert_eq!(one_side.len(), 1, "got {:?}", one_side);
        assert!(one_side[0].message.contains("after"));

        // Second named argument in a call: `f(x = 1, y=2)`.
        let second = lint("f(x = 1, y=2)\n", &config);
        assert_eq!(second.len(), 2, "got {:?}", second);
        assert!(second.iter().all(|d| d.range.start.character == 10));
    }

    #[test]
    fn infix_spaces_does_not_flag_spaced_named_argument() {
        let config = infix_spaces_only_config();
        for text in [
            "f(name = value)\n",
            "fun(blah =  1)\n", // multiple spaces allowed (alignment)
            "f(\"a\" = 1)\n",
            "sum(x, na.rm = TRUE)\n",
        ] {
            let diags = lint(text, &config);
            assert!(diags.is_empty(), "{text:?} must not be flagged: {diags:?}");
        }
    }

    #[test]
    fn infix_spaces_flags_function_default_argument_eq() {
        // lintr lints EQ_FORMALS by default: `function(x=1)` is flagged.
        let config = infix_spaces_only_config();
        let no_space = lint("f <- function(x=1) x\n", &config);
        assert_eq!(no_space.len(), 2, "got {:?}", no_space);
        assert!(no_space.iter().all(|d| d.message.contains("`=`")));

        let with_space = lint("f <- function(x = 1) x\n", &config);
        assert!(
            with_space.is_empty(),
            "function(x = 1) defaults must not be flagged: {:?}",
            with_space
        );

        let lambda = lint("f <- \\(x=1) x\n", &config);
        assert_eq!(lambda.len(), 2, "lambda formals: got {:?}", lambda);
    }

    #[test]
    fn infix_spaces_missing_argument_value_still_needs_space_after_eq() {
        // Pinned by lintr's own suite: `alist(missing_arg = )` is clean but
        // `alist(missing_arg =)` is flagged against the following token.
        let config = infix_spaces_only_config();
        assert!(lint("alist(missing_arg = )\n", &config).is_empty());
        assert!(lint("switch(a = , b = 2)\n", &config).is_empty());

        let alist = lint("alist(missing_arg =)\n", &config);
        assert_eq!(alist.len(), 1, "got {:?}", alist);
        assert!(alist[0].message.contains("after"));

        let switch = lint("switch(a =, b = 2)\n", &config);
        assert_eq!(switch.len(), 1, "got {:?}", switch);
        assert!(switch[0].message.contains("after"));
    }

    #[test]
    fn infix_spaces_flags_eq_assign_without_spaces() {
        let config = infix_spaces_only_config();
        let diags = lint("y =1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("after"));

        // Chained EQ_ASSIGN with spaces is fine.
        assert!(lint("my = bad = variable = name <- 2.0\n", &config).is_empty());
        // `=` inside a string is not an operator.
        assert!(lint("\"my  =  variable\" <- 42.0\n", &config).is_empty());
    }

    #[test]
    fn infix_spaces_box_use_slashes_exempt() {
        // `/` inside box::use() is a module path, not division (lintr parity).
        let config = infix_spaces_only_config();
        assert!(lint("box::use(a/b)\n", &config).is_empty());
        assert!(lint("box::use(./a/b)\n", &config).is_empty());
        let multi = "box::use(\n  a,\n  a/b,\n  ../a,\n  alias = a/b/c[xyz = abc, ...],\n)\n";
        assert!(
            lint(multi, &config).is_empty(),
            "got {:?}",
            lint(multi, &config)
        );

        // ...but ordinary division is still linted.
        let division = lint("x <- a/b\n", &config);
        assert_eq!(division.len(), 2, "got {:?}", division);
    }

    #[test]
    fn infix_spaces_does_not_flag_line_continuation() {
        let config = infix_spaces_only_config();
        // Operator at end of line, RHS on the next line — the newline supplies
        // the separation, so neither side should be flagged.
        let diags = lint("x <- a +\n  b\n", &config);
        assert!(
            diags.is_empty(),
            "line-continuation `+` must not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn infix_spaces_handles_comment_before_operand() {
        // Regression: ensures `child_by_field_name("rhs")` (rather than
        // positional walking) picks the real operand even when a comment
        // node intervenes between the operator and its operand.
        let config = infix_spaces_only_config();
        // A comment between `<-` and `1` is uncommon but legal R. The rule
        // should still be able to evaluate the gap consistently — and since
        // the comment forces a newline before `1`, `gap_text` returns `None`
        // (line-continuation case), so no diagnostic should be produced.
        let diags = lint("x <- # comment\n  1\n", &config);
        assert!(
            diags.is_empty(),
            "comment-then-newline gap must not produce diagnostics: {:?}",
            diags
        );
    }

    #[test]
    fn infix_spaces_flags_custom_percent_op_with_missing_spaces() {
        let config = infix_spaces_only_config();
        let diags = lint("x <- a%>%b\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
        assert!(diags.iter().all(|d| d.message.contains("`%>%`")));
    }

    #[test]
    fn infix_spaces_flags_assignment_without_spaces() {
        let config = infix_spaces_only_config();
        let diags = lint("x<-1\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
        assert!(diags.iter().all(|d| d.message.contains("`<-`")));
    }

    #[test]
    fn infix_spaces_flags_comparison_without_spaces() {
        let config = infix_spaces_only_config();
        let diags = lint("if (a<=b) NULL\n", &config);
        assert_eq!(diags.len(), 2);
        assert!(diags.iter().all(|d| d.message.contains("`<=`")));
    }

    #[test]
    fn infix_spaces_flags_logical_without_spaces() {
        let config = infix_spaces_only_config();
        let diags = lint("if (a&&b) NULL\n", &config);
        assert_eq!(diags.len(), 2);
        assert!(diags.iter().all(|d| d.message.contains("`&&`")));
    }

    #[test]
    fn infix_spaces_flags_formula_without_spaces() {
        let config = infix_spaces_only_config();
        let diags = lint("m <- lm(y~x)\n", &config);
        let tilde_diags: Vec<_> = diags.iter().filter(|d| d.message.contains("`~`")).collect();
        assert_eq!(tilde_diags.len(), 2, "got {:?}", diags);
    }

    #[test]
    fn infix_spaces_does_not_flag_unary_formula() {
        let config = infix_spaces_only_config();
        // `~ x` and `~x` are both acceptable formula-head forms. The rule
        // should not touch unary `~` either way.
        let no_space = lint("f(~x)\n", &config);
        let with_space = lint("f(~ x)\n", &config);
        assert!(no_space.is_empty(), "unary `~x` must pass: {:?}", no_space);
        assert!(
            with_space.is_empty(),
            "unary `~ x` must pass: {:?}",
            with_space
        );
    }

    #[test]
    fn infix_spaces_does_not_flag_pipe_with_spaces() {
        let config = infix_spaces_only_config();
        let diags = lint("xs |> length()\n", &config);
        assert!(diags.is_empty(), "pipe with spaces must pass: {:?}", diags);
    }

    #[test]
    fn infix_spaces_respects_nolint() {
        let config = infix_spaces_only_config();
        let diags = lint("x <- 1+2 # nolint\n", &config);
        assert!(diags.is_empty(), "nolint must suppress: {:?}", diags);
    }

    #[test]
    fn infix_spaces_respects_lsp_ignore_next() {
        let config = infix_spaces_only_config();
        let diags = lint("# @lsp-ignore-next\nx <- 1+2\ny <- 3+4\n", &config);
        let lines: Vec<u32> = diags.iter().map(|d| d.range.start.line).collect();
        assert!(
            !lines.contains(&1),
            "@lsp-ignore-next must suppress line 1: {:?}",
            diags
        );
        assert!(
            lines.contains(&2),
            "unsuppressed line 2 must still flag: {:?}",
            diags
        );
    }

    #[test]
    fn infix_spaces_severity_off_disables_rule() {
        let mut config = infix_spaces_only_config();
        config.infix_spaces_severity = None;
        let diags = lint("x<-1\n", &config);
        assert!(
            diags.is_empty(),
            "rule must be silent when severity is None: {:?}",
            diags
        );
    }

    fn commented_code_only_config() -> LintConfig {
        LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            assignment_operator_severity: None,
            object_name_severity: None,
            infix_spaces_severity: None,
            indentation_severity: None,
            ..enabled_config()
        }
    }

    #[test]
    fn commented_code_flags_obvious_call() {
        let config = commented_code_only_config();
        let diags = lint("# foo(bar, baz)\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("Commented code"));
        assert_eq!(diags[0].range.start.line, 0);
    }

    #[test]
    fn commented_code_flags_assignment() {
        let config = commented_code_only_config();
        let diags = lint("# x <- 1 + 2\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn commented_code_skips_prose_without_operators() {
        let config = commented_code_only_config();
        // Bare identifier — could be a noun in prose. Don't flag.
        let diags = lint("# foo\n# another comment\n", &config);
        assert!(diags.is_empty(), "prose must not be flagged: {:?}", diags);
    }

    // Issue #346: a raw leading U+FEFF on the first line (in-memory text from a
    // non-VS-Code client) must not hide first-line commented-out code. The
    // commented-code rule's raw-text seams (`strip_hash_prefix`, `is_skip_line`,
    // the standalone-prefix check) use BOM-insensitive `trim_start`, so without
    // tolerance the `#` is never recognised and the line is silently not flagged.
    #[test]
    fn commented_code_flags_first_line_after_bom() {
        let config = commented_code_only_config();
        let diags = lint("\u{FEFF}# x <- 1 + 2\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 0);
        // The BOM occupies UTF-16 column 0, so the `#` reports at column 1 —
        // the diagnostic stays aligned with the client's BOM-bearing buffer.
        assert_eq!(diags[0].range.start.character, 1);
    }

    #[test]
    fn commented_code_skips_roxygen() {
        let config = commented_code_only_config();
        // Roxygen blocks routinely contain code-shaped examples like
        // `@param x default 1` — those must not be flagged.
        let diags = lint("#' @param x default value\n#' foo(bar = 1)\n", &config);
        assert!(diags.is_empty(), "roxygen must not be flagged: {:?}", diags);
    }

    #[test]
    fn commented_code_skips_shebang_on_first_line() {
        let config = commented_code_only_config();
        let diags = lint("#!/usr/bin/env Rscript\n", &config);
        assert!(diags.is_empty(), "shebang must not be flagged: {:?}", diags);
    }

    #[test]
    fn commented_code_skips_todo_and_fixme_lines() {
        let config = commented_code_only_config();
        let diags = lint(
            "# TODO: rewrite foo(x, y)\n# FIXME(jmb): fix logger(level)\n# NOTE: see help() below\n",
            &config,
        );
        assert!(
            diags.is_empty(),
            "annotation comments must not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_skips_directive_markers() {
        let config = commented_code_only_config();
        // `# nolint`, `# @lsp-source`, etc. must never be flagged — these are
        // suppression / cross-file directives, not commented-out code.
        let diags = lint(
            "# nolint\n# nolint: line_length\n# @lsp-source ../helpers.R\n# @lsp-ignore\n",
            &config,
        );
        assert!(
            diags.is_empty(),
            "directive markers must not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_checks_end_of_line_comments() {
        let config = commented_code_only_config();
        // lintr checks trailing comments too: a code-shaped end-of-line
        // comment is flagged, prose is not.
        let diags = lint("x <- 1 # x <- 2\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(lint("x <- 1 # for example only\n", &config).is_empty());
    }

    #[test]
    fn commented_code_groups_contiguous_block() {
        let config = commented_code_only_config();
        // Two commented-out code lines that are syntactically valid R when
        // joined. Should produce *one* diagnostic for the whole block.
        let diags = lint("# x <- 1\n# y <- x + 2\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        let d = &diags[0];
        assert_eq!(d.range.start.line, 0);
        assert_eq!(d.range.end.line, 1);
    }

    #[test]
    fn commented_code_respects_nolint_block() {
        let config = commented_code_only_config();
        let diags = lint("# nolint start\n# x <- 1 + 2\n# nolint end\n", &config);
        assert!(diags.is_empty(), "nolint block must suppress: {:?}", diags);
    }

    #[test]
    fn commented_code_respects_inline_nolint_marker() {
        // Issue #242: a `# nolint` written *inside* a commented-code line
        // suppresses `commented_code` on that line. The parse-gated fallback
        // in `Suppressions::from_text` recognises the interior marker because
        // the prefix `x <- 1 + 2` parses as real R code.
        let config = commented_code_only_config();
        let diags = lint("# x <- 1 + 2 # nolint\n", &config);
        assert!(
            diags.is_empty(),
            "inline `# nolint` must suppress commented_code: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_respects_inline_lsp_ignore_marker() {
        let config = commented_code_only_config();
        let diags = lint("# x <- 1 + 2 # @lsp-ignore\n", &config);
        assert!(
            diags.is_empty(),
            "inline `# @lsp-ignore` must suppress commented_code: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_inline_marker_inside_string_does_not_suppress() {
        // The interior `# nolint` is inside a string in the commented-out
        // code, so the inline-marker fallback must not treat it as a marker.
        let config = commented_code_only_config();
        let diags = lint("# x <- \"# nolint\"\n", &config);
        assert_eq!(diags.len(), 1, "expected one diagnostic, got {:?}", diags);
    }

    #[test]
    fn commented_code_respects_lsp_ignore_next() {
        let config = commented_code_only_config();
        let diags = lint("# @lsp-ignore-next\n# x <- 1 + 2\n# y <- 3 + 4\n", &config);
        // `@lsp-ignore-next` suppresses line 1. Line 2 is also commented
        // code, and it must STILL be flagged — the contiguous block is split
        // at the directive line and at the suppressed line, leaving line 2
        // as its own sub-group that the rule evaluates independently.
        let lines: Vec<u32> = diags.iter().map(|d| d.range.start.line).collect();
        assert!(
            !lines.contains(&1),
            "@lsp-ignore-next must suppress line 1: {:?}",
            diags
        );
        assert!(
            lines.contains(&2),
            "unsuppressed line 2 must still be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_splits_block_at_directive_line() {
        let config = commented_code_only_config();
        // A `# @lsp-source` directive sandwiched between two real commented
        // code lines must NOT swallow them. The block should split into two
        // single-line sub-groups; each parses as code and is reported.
        let diags = lint("# x <- 1\n# @lsp-source ../helpers.R\n# y <- 2\n", &config);
        let lines: Vec<u32> = diags.iter().map(|d| d.range.start.line).collect();
        assert!(
            lines.contains(&0),
            "line 0 commented code must still flag: {:?}",
            diags
        );
        assert!(
            lines.contains(&2),
            "line 2 commented code must still flag: {:?}",
            diags
        );
        assert!(
            !lines.contains(&1),
            "directive line itself must not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_splits_block_at_shebang_line() {
        let config = commented_code_only_config();
        // Shebang on line 0 plus commented code on line 1 must not silently
        // skip the commented code as part of the same block.
        let diags = lint("#!/usr/bin/env Rscript\n# x <- 1\n", &config);
        let lines: Vec<u32> = diags.iter().map(|d| d.range.start.line).collect();
        assert!(
            lines.contains(&1),
            "commented code adjacent to shebang must flag: {:?}",
            diags
        );
        assert!(
            !lines.contains(&0),
            "shebang itself must not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_splits_block_at_todo_line() {
        let config = commented_code_only_config();
        // Same idea with an annotation comment in the middle.
        let diags = lint("# x <- 1\n# TODO: revisit\n# y <- 2\n", &config);
        let lines: Vec<u32> = diags.iter().map(|d| d.range.start.line).collect();
        assert!(lines.contains(&0));
        assert!(lines.contains(&2));
        assert!(!lines.contains(&1));
    }

    #[test]
    fn commented_code_flags_multi_line_block_when_individual_lines_dont_parse() {
        let config = commented_code_only_config();
        // Each individual line is not a complete R expression — only the
        // joined block parses. The grouping pass should still flag the
        // block. This is the headline value-add of the contiguous-grouping
        // logic vs. line-at-a-time evaluation.
        let diags = lint("# function(x) {\n#   x + 1\n# }\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 0);
        assert_eq!(diags[0].range.end.line, 2);
    }

    #[test]
    fn commented_code_still_groups_when_no_skip_lines() {
        let config = commented_code_only_config();
        // Without any skip lines in between, a contiguous block should still
        // join as one diagnostic — Codex's regression case must not break
        // the original grouping behaviour.
        let diags = lint("# x <- 1\n# y <- x + 2\n# z <- y * 3\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 0);
        assert_eq!(diags[0].range.end.line, 2);
    }

    #[test]
    fn commented_code_skips_mode_line() {
        let config = commented_code_only_config();
        let diags = lint("# -*- coding: utf-8 -*-\n", &config);
        assert!(
            diags.is_empty(),
            "Emacs mode line must not be flagged: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_severity_off_disables_rule() {
        let mut config = commented_code_only_config();
        config.commented_code_severity = None;
        let diags = lint("# x <- 1\n", &config);
        assert!(
            diags.is_empty(),
            "rule must be silent when severity is None: {:?}",
            diags
        );
    }

    #[test]
    fn commented_code_diagnostic_carries_lint_source() {
        let config = commented_code_only_config();
        let diags = lint("# foo(bar)\n", &config);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].source.as_deref(), Some(LINT_SOURCE));
    }

    /// Builds a config with every existing rule disabled, leaving only the
    /// rule under test enabled. Used by the per-rule sub-tests below — each
    /// rule sets its own severity back on top of this baseline.
    fn solo_config() -> LintConfig {
        LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            assignment_operator_severity: None,
            object_name_severity: None,
            infix_spaces_severity: None,
            commented_code_severity: None,
            quotes_severity: None,
            commas_severity: None,
            t_and_f_symbol_severity: None,
            semicolon_severity: None,
            equals_na_severity: None,
            object_length_severity: None,
            vector_logic_severity: None,
            function_left_parentheses_severity: None,
            spaces_inside_severity: None,
            indentation_severity: None,
            ..enabled_config()
        }
    }

    // ------------------------------------------------------------------
    // quotes
    // ------------------------------------------------------------------

    fn quotes_only_config() -> LintConfig {
        LintConfig {
            quotes_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn quotes_flags_single_when_double_preferred() {
        let config = quotes_only_config();
        let diags = lint("x <- 'hi'\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("`'`"));
        assert!(diags[0].message.contains("`\"`"));
    }

    #[test]
    fn quotes_accepts_double() {
        let config = quotes_only_config();
        let diags = lint("x <- \"hi\"\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn quotes_checks_raw_strings_by_content() {
        let config = quotes_only_config();
        // lintr flags single-quoted raw strings whose body contains no `"`.
        let diags = lint("x <- r'(hi)'\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        // ...but leaves them alone when switching would need escaping.
        assert!(lint("x <- r'(\")'\n", &config).is_empty());
        assert!(lint("x <- r\"(hi)\"\n", &config).is_empty());
    }

    #[test]
    fn quotes_content_with_other_delimiter_is_exempt() {
        let config = quotes_only_config();
        // A single-quoted string containing a double quote cannot switch
        // delimiters without escaping — lintr leaves these alone.
        assert!(lint("x <- 'he said \"hi\"'\n", &config).is_empty());
        assert!(lint("x <- '\\\"hi\\\"'\n", &config).is_empty());
        assert!(lint("x <- '\"'\n", &config).is_empty());
    }

    #[test]
    fn quotes_single_mode_flags_double() {
        let mut config = quotes_only_config();
        config.string_delimiter = StringDelimiter::Single;
        let diags = lint("x <- \"hi\"\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("`\"`"));
        assert!(diags[0].message.contains("`'`"));
    }

    #[test]
    fn quotes_respects_nolint() {
        let config = quotes_only_config();
        let diags = lint("x <- 'hi' # nolint\n", &config);
        assert!(diags.is_empty(), "nolint must suppress: {:?}", diags);
    }

    // ------------------------------------------------------------------
    // commas
    // ------------------------------------------------------------------

    fn commas_only_config() -> LintConfig {
        LintConfig {
            commas_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn commas_flags_space_before() {
        let config = commas_only_config();
        let diags = lint("c(1 , 2)\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("before"));
    }

    #[test]
    fn commas_flags_missing_space_after() {
        let config = commas_only_config();
        let diags = lint("c(1,2)\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("after"));
    }

    #[test]
    fn commas_accepts_clean_spacing() {
        let config = commas_only_config();
        let diags = lint("c(1, 2, 3)\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn commas_allows_newline_after() {
        let config = commas_only_config();
        // A newline after the comma is fine — multi-line argument lists are
        // common and shouldn't be reformatted.
        let diags = lint("c(\n  1,\n  2\n)\n", &config);
        assert!(
            diags.is_empty(),
            "newline-after-comma must pass: {:?}",
            diags
        );
    }

    #[test]
    fn commas_flags_trailing_comma_before_close() {
        // Matches `lintr::commas_linter(allow_trailing = FALSE)` — `a[1,]`
        // has no whitespace after `,` so it's still flagged.
        let config = commas_only_config();
        let diags = lint("a[1,]\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn commas_and_spaces_inside_accept_omitted_subset_dimension() {
        let config = LintConfig {
            commas_severity: Some(DiagnosticSeverity::HINT),
            spaces_inside_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        };

        let canonical = lint("x[i, ]\n", &config);
        assert!(
            canonical.is_empty(),
            "`x[i, ]` must be clean with both rules enabled: {:?}",
            canonical
        );

        let compact = lint("x[i,]\n", &config);
        assert_eq!(compact.len(), 1, "got {:?}", compact);
        assert_eq!(
            compact[0].code,
            Some(NumberOrString::String(rule_ids::COMMAS.to_string()))
        );
        assert_eq!(compact[0].message, "Missing space after `,`.");
    }

    #[test]
    fn commas_in_parameters_are_also_checked() {
        let config = commas_only_config();
        // Tree-sitter treats `parameter` commas the same way; the rule walks
        // them too.
        let diags = lint("f <- function(a,b) a + b\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    // ------------------------------------------------------------------
    // T_and_F_symbol
    // ------------------------------------------------------------------

    fn t_and_f_only_config() -> LintConfig {
        LintConfig {
            t_and_f_symbol_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn t_and_f_flags_bare_t() {
        let config = t_and_f_only_config();
        let diags = lint("if (T) 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("TRUE"));
    }

    #[test]
    fn t_and_f_flags_bare_f() {
        let config = t_and_f_only_config();
        let diags = lint("x <- F\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("FALSE"));
    }

    #[test]
    fn t_and_f_accepts_true_false() {
        let config = t_and_f_only_config();
        let diags = lint("if (TRUE) 1\nx <- FALSE\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn t_and_f_flags_assignment_target_as_variable_name() {
        let config = t_and_f_only_config();
        // lintr flags assignment targets with a dedicated variable-name lint.
        let diags = lint("T <- 0\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("variable name"), "{:?}", diags);
        assert_eq!(lint("\"foo\" -> T\n", &config).len(), 1);
        assert_eq!(lint("T <<- 1\n", &config).len(), 1);
    }

    #[test]
    fn t_and_f_formula_exemption_covers_assignment_targets() {
        // lintr leaves even `y ~ {T <- 1}` alone.
        let config = t_and_f_only_config();
        assert!(lint("m <- y ~ {T <- 1}\n", &config).is_empty());
    }

    #[test]
    fn t_and_f_formula_exemption_spans_braces_and_lambdas() {
        // lintr exempts T/F anywhere under `~`, including inside braces and
        // lambda bodies (verified against 3.3.0.1).
        let config = t_and_f_only_config();
        assert!(lint("m <- y ~ {T}\n", &config).is_empty());
        assert!(lint("m <- y ~ sapply(x, function(i) T)\n", &config).is_empty());
    }

    #[test]
    fn t_and_f_skips_formula_and_subscript_contexts() {
        let config = t_and_f_only_config();
        // Formula terms, subscripted uses, and callees are not boolean reads.
        assert!(lint("m <- y ~ T + F\n", &config).is_empty());
        assert!(lint("x <- T[1]\n", &config).is_empty());
        assert!(lint("x <- T[[1]]\n", &config).is_empty());
        assert!(lint("x <- T(1)\n", &config).is_empty());
        // ...but a named-argument value inside a formula is a real read.
        let diags = lint("m <- y ~ foo(x, arg = T)\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn t_and_f_skips_named_argument() {
        let config = t_and_f_only_config();
        // `foo(T = TRUE)` — the `T` is a parameter label.
        let diags = lint("foo(T = TRUE)\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn t_and_f_skips_extract_rhs() {
        let config = t_and_f_only_config();
        // `obj$T` — `T` is a field name on `obj`.
        let diags = lint("x <- obj$T\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn t_and_f_flags_extract_lhs() {
        let config = t_and_f_only_config();
        // `T$foo` — `T` on the LHS *is* a read of the boolean.
        let diags = lint("x <- T$foo\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn t_and_f_skips_formal_parameter() {
        let config = t_and_f_only_config();
        // `function(T) ...` — declaration of `T` as a parameter, not a read.
        let diags = lint("f <- function(T) T + 1\n", &config);
        // Only the *use* of `T` in the body is a read.
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    // ------------------------------------------------------------------
    // semicolon
    // ------------------------------------------------------------------

    fn semicolon_only_config() -> LintConfig {
        LintConfig {
            semicolon_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn semicolon_flags_separator() {
        let config = semicolon_only_config();
        let diags = lint("a; b\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.character, 1);
    }

    #[test]
    fn semicolon_flags_trailing() {
        let config = semicolon_only_config();
        let diags = lint("a;\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn semicolon_flags_multiple_per_line() {
        let config = semicolon_only_config();
        let diags = lint("a; b; c\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
    }

    #[test]
    fn semicolon_ignores_strings_and_comments() {
        let config = semicolon_only_config();
        // `;` inside a string or comment is not a separator.
        let diags = lint("x <- \"a;b\"\ny <- 1 # ;\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    // ------------------------------------------------------------------
    // equals_na
    // ------------------------------------------------------------------

    fn equals_na_only_config() -> LintConfig {
        LintConfig {
            equals_na_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn equals_na_flags_x_eq_na() {
        let config = equals_na_only_config();
        let diags = lint("x == NA\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("is.na"));
    }

    #[test]
    fn equals_na_flags_na_eq_x() {
        let config = equals_na_only_config();
        let diags = lint("NA == x\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn equals_na_flags_neq() {
        let config = equals_na_only_config();
        let diags = lint("x != NA\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn equals_na_flags_typed_na_variants() {
        let config = equals_na_only_config();
        let diags = lint(
            "x == NA_integer_\nx == NA_real_\nx == NA_character_\nx == NA_complex_\n",
            &config,
        );
        assert_eq!(diags.len(), 4, "got {:?}", diags);
    }

    #[test]
    fn equals_na_does_not_flag_is_na() {
        let config = equals_na_only_config();
        let diags = lint("is.na(x)\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    // ------------------------------------------------------------------
    // object_length
    // ------------------------------------------------------------------

    fn object_length_only_config(max: u32) -> LintConfig {
        LintConfig {
            object_length: max,
            object_length_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn object_length_flags_overlong_assignment_target() {
        let config = object_length_only_config(5);
        let diags = lint("longer_name <- 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("longer_name"));
        assert!(diags[0].message.contains("11"));
    }

    #[test]
    fn object_length_accepts_short_name() {
        let config = object_length_only_config(5);
        let diags = lint("ok <- 1\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn object_length_flags_overlong_parameter() {
        let config = object_length_only_config(5);
        let diags = lint("f <- function(longer_arg) 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("longer_arg"));
    }

    #[test]
    fn object_length_counts_leading_dot() {
        // lintr measures `nchar` on the stripped name, which *includes* a
        // leading dot: `.foo_bar` is 8 characters, so it fails max=7 and
        // passes max=8.
        let config = object_length_only_config(7);
        let diags = lint(".foo_bar <- 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(lint(".foo_bar <- 1\n", &object_length_only_config(8)).is_empty());
    }

    #[test]
    fn object_length_strips_backticks_and_measures() {
        // lintr strips the backticks and measures the name; a long backticked
        // name is flagged like its unquoted spelling (`a very long backtick
        // name` is 25 characters).
        let config = object_length_only_config(5);
        let diags = lint("`a very long backtick name` <- 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(
            lint(
                "`a very long backtick name` <- 1\n",
                &object_length_only_config(25)
            )
            .is_empty()
        );
    }

    #[test]
    fn object_length_removes_generic_prefix_for_s3_methods() {
        // lintr: "only lints S3 implementations if the class names are too
        // long". `print.short_class` measures just `short_class` (11).
        let config = object_length_only_config(11);
        assert!(lint("print.short_class <- function(x) 1\n", &config).is_empty());
        // ...but a class part over the limit still lints.
        let diags = lint("print.a_longer_class <- function(x) 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        // Unknown prefixes get no removal: `foo.bar_baz` measures 11 in full.
        assert!(lint("foo.bar_baz <- 1\n", &config).is_empty());
        assert_eq!(lint("foo.bar_bazz <- 1\n", &config).len(), 1);
    }

    #[test]
    fn object_length_skips_compound_lhs() {
        let config = object_length_only_config(5);
        // `obj$longer_field <- 1` — assignment doesn't introduce a new symbol.
        let diags = lint("obj$longer_field <- 1\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    // ------------------------------------------------------------------
    // vector_logic
    // ------------------------------------------------------------------

    fn vector_logic_only_config() -> LintConfig {
        LintConfig {
            vector_logic_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn vector_logic_flags_amp_in_if() {
        let config = vector_logic_only_config();
        let diags = lint("if (a & b) 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("&&"));
    }

    #[test]
    fn vector_logic_flags_pipe_in_while() {
        let config = vector_logic_only_config();
        let diags = lint("while (a | b) 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("||"));
    }

    #[test]
    fn vector_logic_accepts_double_operators() {
        let config = vector_logic_only_config();
        let diags = lint("if (a && b) 1\nwhile (a || b) 1\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn vector_logic_skips_inside_function_call() {
        // `if (any(x & y))` — the `&` is evaluated inside `any()` on a
        // vector, not on the condition itself.
        let config = vector_logic_only_config();
        let diags = lint("if (any(x & y)) 1\n", &config);
        assert!(
            diags.is_empty(),
            "call boundary must stop scan: {:?}",
            diags
        );
    }

    #[test]
    fn vector_logic_recurses_through_logical_operators() {
        // `if (a & b || c)` — the `&` deep inside the condition is still
        // flagged because the scan recurses through binary operators.
        let config = vector_logic_only_config();
        let diags = lint("if (a & b || c) 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    // ------------------------------------------------------------------
    // mixed_logical  (semantic check — uses lint_semantic, not lint)
    // ------------------------------------------------------------------

    const ML_WARN: Option<DiagnosticSeverity> = Some(DiagnosticSeverity::WARNING);

    #[test]
    fn mixed_logical_flags_and_inside_or() {
        let diags = lint_semantic("x <- a & b | c\n", ML_WARN, None);
        assert!(!diags.is_empty(), "expected diagnostic, got none");
        assert!(
            diags[0].message.contains("parentheses"),
            "got: {:?}",
            diags[0].message
        );
    }

    #[test]
    fn mixed_logical_flags_or_inside_and() {
        // tree-sitter parses `a | b & c` as `a | (b & c)`; `|` is outer.
        let diags = lint_semantic("x <- a | b & c\n", ML_WARN, None);
        assert!(!diags.is_empty(), "expected diagnostic, got none");
    }

    #[test]
    fn mixed_logical_flags_double_operators() {
        let diags = lint_semantic("x <- a && b || c\n", ML_WARN, None);
        assert!(!diags.is_empty(), "expected diagnostic, got none");
        assert!(
            diags[0].message.contains("&&"),
            "got: {:?}",
            diags[0].message
        );
    }

    #[test]
    fn mixed_logical_accepts_explicit_parens_left() {
        let diags = lint_semantic("x <- (a & b) | c\n", ML_WARN, None);
        assert!(diags.is_empty(), "expected no diagnostic, got {:?}", diags);
    }

    #[test]
    fn mixed_logical_accepts_explicit_parens_right() {
        let diags = lint_semantic("x <- a & (b | c)\n", ML_WARN, None);
        assert!(diags.is_empty(), "expected no diagnostic, got {:?}", diags);
    }

    #[test]
    fn mixed_logical_accepts_pure_and() {
        let diags = lint_semantic("x <- a & b & c\n", ML_WARN, None);
        assert!(
            diags.is_empty(),
            "pure `&` should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn mixed_logical_accepts_pure_or() {
        let diags = lint_semantic("x <- a | b | c\n", ML_WARN, None);
        assert!(
            diags.is_empty(),
            "pure `|` should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn mixed_logical_flags_in_if_condition() {
        let diags = lint_semantic("if (a & b | c) x\n", ML_WARN, None);
        assert!(
            !diags.is_empty(),
            "should flag mixed operators in condition"
        );
    }

    #[test]
    fn mixed_logical_flags_cross_family_or_then_double_and() {
        let diags = lint_semantic("x <- a | b && c\n", ML_WARN, None);
        assert!(
            !diags.is_empty(),
            "a | b && c should be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn mixed_logical_flags_cross_family_double_or_then_and() {
        let diags = lint_semantic("x <- a || b & c\n", ML_WARN, None);
        assert!(
            !diags.is_empty(),
            "a || b & c should be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn mixed_logical_skips_inside_call() {
        let diags = lint_semantic("filter(df, a | b & c)\n", ML_WARN, None);
        assert!(
            diags.is_empty(),
            "mixed operators inside a call should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn mixed_logical_skips_inside_subset() {
        let diags = lint_semantic("df[a | b & c, ]\n", ML_WARN, None);
        assert!(
            diags.is_empty(),
            "mixed operators inside subset should not be flagged, got {:?}",
            diags
        );
    }

    // ------------------------------------------------------------------
    // condition_assignment  (semantic check — uses lint_semantic, not lint)
    // ------------------------------------------------------------------

    const CA_WARN: Option<DiagnosticSeverity> = Some(DiagnosticSeverity::WARNING);

    #[test]
    fn condition_assignment_flags_equals_in_if() {
        let diags = lint_semantic("if (x = 1) x\n", None, CA_WARN);
        assert!(!diags.is_empty(), "expected diagnostic, got none");
        assert!(
            diags[0].message.contains("=="),
            "got: {:?}",
            diags[0].message
        );
    }

    #[test]
    fn condition_assignment_flags_equals_in_while() {
        let diags = lint_semantic("while (done = FALSE) x\n", None, CA_WARN);
        assert!(
            !diags.is_empty(),
            "expected diagnostic for = in while condition"
        );
    }

    #[test]
    fn condition_assignment_accepts_double_equals() {
        let diags = lint_semantic("if (x == 1) x\n", None, CA_WARN);
        assert!(
            diags.is_empty(),
            "`==` should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn condition_assignment_accepts_left_arrow() {
        let diags = lint_semantic("if (x <- 1) x\n", None, CA_WARN);
        assert!(
            diags.is_empty(),
            "`<-` in condition should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn condition_assignment_skips_named_arg_inside_call() {
        let diags = lint_semantic("if (identical(x = 1, 1)) x\n", None, CA_WARN);
        assert!(
            diags.is_empty(),
            "named-arg `=` inside a call should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn condition_assignment_accepts_top_level_equals() {
        let diags = lint_semantic("x = 1\n", None, CA_WARN);
        assert!(
            diags.is_empty(),
            "top-level `=` should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn condition_assignment_skips_parenthesized_condition() {
        let diags = lint_semantic("if ((x = 1)) x\n", None, CA_WARN);
        assert!(
            diags.is_empty(),
            "`=` inside parenthesized_expression should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn condition_assignment_skips_braced_condition() {
        let diags = lint_semantic("if ({ x = 1; x > 0 }) x\n", None, CA_WARN);
        assert!(
            diags.is_empty(),
            "`=` inside braced_expression should not be flagged, got {:?}",
            diags
        );
    }

    #[test]
    fn condition_assignment_no_duplicate_with_assignment_operator() {
        // `if (x = 1)` — condition_assignment fires; assignment_operator must
        // NOT also fire (contradictory advice).
        let semantic = lint_semantic("if (x = 1) x\n", None, CA_WARN);
        let style = lint(
            "if (x = 1) x\n",
            &LintConfig {
                assignment_operator_severity: Some(DiagnosticSeverity::HINT),
                ..solo_config()
            },
        );
        let mut all = semantic;
        all.extend(style);
        assert_eq!(
            all.len(),
            1,
            "expected exactly one diagnostic for `if (x = 1)`, got {:?}",
            all
        );
        assert!(
            all[0].message.contains("=="),
            "expected condition_assignment message, got {:?}",
            all[0].message
        );
    }

    // ------------------------------------------------------------------
    // function_left_parentheses
    // ------------------------------------------------------------------

    fn flp_only_config() -> LintConfig {
        LintConfig {
            function_left_parentheses_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn flp_flags_space_after_function() {
        let config = flp_only_config();
        let diags = lint("f <- function (x) x\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("function"));
    }

    #[test]
    fn flp_accepts_tight() {
        let config = flp_only_config();
        let diags = lint("f <- function(x) x\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn flp_flags_lambda_with_space() {
        let config = flp_only_config();
        let diags = lint("f <- \\ (x) x\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("\\"));
    }

    #[test]
    fn flp_accepts_tight_lambda() {
        let config = flp_only_config();
        let diags = lint("f <- \\(x) x\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    // ------------------------------------------------------------------
    // spaces_inside
    // ------------------------------------------------------------------

    fn spaces_inside_only_config() -> LintConfig {
        LintConfig {
            spaces_inside_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn spaces_inside_flags_call_with_spaces() {
        let config = spaces_inside_only_config();
        let diags = lint("f( x )\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_accepts_tight_call() {
        let config = spaces_inside_only_config();
        let diags = lint("f(x)\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_flags_whitespace_only_groupings() {
        let config = spaces_inside_only_config();
        // `f()` is fine; `f( )` is flagged on both sides (lintr parity).
        // A multi-line `f(\n)` stays fine.
        assert!(lint("f()\n", &config).is_empty());
        let diags_padded = lint("f(  )\n", &config);
        assert_eq!(diags_padded.len(), 2, "got {:?}", diags_padded);
        assert!(lint("f(\n)\n", &config).is_empty());
    }

    #[test]
    fn spaces_inside_flags_subset() {
        let config = spaces_inside_only_config();
        let diags = lint("a[ 1 ]\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_still_flags_subset_padding() {
        let config = spaces_inside_only_config();
        let diags = lint("df[ 1 ]\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_flags_subset2() {
        let config = spaces_inside_only_config();
        let diags = lint("a[[ 1 ]]\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_accepts_trailing_comma_subset_omission() {
        let config = spaces_inside_only_config();
        let diags = lint("x[i, ]\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_accepts_trailing_comma_subset2_omission() {
        let config = spaces_inside_only_config();
        let diags = lint("x[[i, ]]\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_accepts_first_dimension_omission() {
        let config = spaces_inside_only_config();
        let diags = lint("x[, ]\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_flags_before_close_when_subset_prev_token_is_argument() {
        let config = spaces_inside_only_config();
        let diags = lint("x[i, j ]\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].message, "Remove whitespace before `]`.");
    }

    #[test]
    fn spaces_inside_exempts_call_trailing_comma_padding() {
        // lintr's comma-before-closer exemption applies to calls too: `f(a, )`
        // is clean, as is a value-less named argument before `)`.
        let config = spaces_inside_only_config();
        assert!(lint("f(a, )\n", &config).is_empty());
        assert!(lint("a(, )\n", &config).is_empty());
        assert!(lint("alist(missing_arg = )\n", &config).is_empty());
        // The `=` carve-out is paren-only: `x[j = ]` stays flagged.
        let subset = lint("x[j = ]\n", &config);
        assert_eq!(subset.len(), 1, "got {:?}", subset);
    }

    #[test]
    fn spaces_inside_covers_control_flow_and_definition_parens() {
        // lintr's XPath matches every `(` token, including control flow and
        // function/lambda definitions.
        let config = spaces_inside_only_config();
        assert_eq!(lint("if ( x ) y\n", &config).len(), 2);
        assert_eq!(lint("while ( x ) break\n", &config).len(), 2);
        assert_eq!(lint("for ( i in 1:3 ) print(i)\n", &config).len(), 2);
        assert_eq!(lint("f <- function( x ) x\n", &config).len(), 2);
        assert_eq!(lint("sapply(x, \\( y ) y)\n", &config).len(), 2);
        assert!(lint("if (x) y\n", &config).is_empty());
    }

    #[test]
    fn spaces_inside_exempts_comment_after_opener() {
        let config = spaces_inside_only_config();
        assert!(lint("or( #code\n  x, y\n)\n", &config).is_empty());
    }

    #[test]
    fn spaces_inside_allows_comment_before_subset_close() {
        let config = spaces_inside_only_config();
        let diags = lint("x[i, # c\n]\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_allows_multiline_wrapping() {
        let config = spaces_inside_only_config();
        // Multi-line argument layout — the leading/trailing newlines are not
        // single-line whitespace, so no diagnostic.
        let diags = lint("f(\n  1,\n  2\n)\n", &config);
        assert!(diags.is_empty(), "got {:?}", diags);
    }

    #[test]
    fn spaces_inside_flags_parenthesized_expression() {
        let config = spaces_inside_only_config();
        let diags = lint("x <- ( 1 + 2 )\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
    }

    // ------------------------------------------------------------------
    // indentation
    // ------------------------------------------------------------------

    fn indentation_only_config() -> LintConfig {
        LintConfig {
            indentation_severity: Some(DiagnosticSeverity::HINT),
            ..solo_config()
        }
    }

    #[test]
    fn indentation_dispatcher_flags_misindented_block_line() {
        let config = indentation_only_config();
        let diags = lint("f <- function() {\nx <- 1\n}\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 2 spaces"));
    }

    #[test]
    fn indentation_dispatcher_honors_configured_unit() {
        let config = LintConfig {
            indentation_unit: 4,
            ..indentation_only_config()
        };
        // 4 spaces is correct under indent_unit=4; 2 spaces is wrong.
        assert!(lint("f <- function() {\n    x <- 1\n}\n", &config).is_empty());
        let diags = lint("f <- function() {\n  x <- 1\n}\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("should be 4 spaces"));
    }

    fn trailing_whitespace_only_config() -> LintConfig {
        LintConfig {
            trailing_whitespace_severity: Some(DiagnosticSeverity::INFORMATION),
            ..all_lint_rules_off()
        }
    }

    fn no_tab_only_config() -> LintConfig {
        LintConfig {
            no_tab_severity: Some(DiagnosticSeverity::INFORMATION),
            ..all_lint_rules_off()
        }
    }

    fn function_left_parentheses_only_config() -> LintConfig {
        LintConfig {
            function_left_parentheses_severity: Some(DiagnosticSeverity::INFORMATION),
            ..all_lint_rules_off()
        }
    }

    fn trailing_blank_lines_only_config() -> LintConfig {
        LintConfig {
            trailing_blank_lines_severity: Some(DiagnosticSeverity::INFORMATION),
            ..all_lint_rules_off()
        }
    }

    // ------------------------------------------------------------------
    // lintr-parity regressions (2026-07 audit): each case was verified
    // against real lintr 3.3.0.1 before being pinned here.
    // ------------------------------------------------------------------

    #[test]
    fn commas_exempts_leading_comma_continuation_style() {
        let config = commas_only_config();
        // Comma first on its line (any indentation) is the leading-comma
        // continuation style; lintr's before-check needs a same-line token.
        assert!(lint("fun(1\n  ,\n1)\n", &config).is_empty());
        // Preceding comma marks a missing argument: `a[1, , 2]`.
        assert!(lint("a[1, , 2]\n", &config).is_empty());
        assert!(lint("a[1, , 2, , 3]\n", &config).is_empty());
        // Preceding `=` marks a missing named-argument value.
        assert!(lint("switch(op, x = , y = bar)\n", &config).is_empty());
        assert!(lint("switch(op, \"x\" = , y = bar)\n", &config).is_empty());
        // The plain violation still flags.
        let diags = lint("fun(1 ,1)\n", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
    }

    #[test]
    fn semicolon_ignores_semicolons_inside_tokens() {
        let config = semicolon_only_config();
        assert!(lint("`a;b` <- 1\n", &config).is_empty());
        assert!(lint("x <- a %;% b\n", &config).is_empty());
        assert!(lint("x <- 'a;b'\n", &config).is_empty());
        assert!(lint("# a;b\n", &config).is_empty());
        let diags = lint("a <- 1; b <- 2\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn equals_na_flags_in_operator_with_na_rhs() {
        let config = equals_na_only_config();
        for na in ["NA", "NA_integer_", "NA_real_", "NA_character_"] {
            let text = format!("x %in% {na}\n");
            assert_eq!(lint(&text, &config).len(), 1, "{na}");
        }
        // `NA %in% x` is a legitimate membership test.
        assert!(lint("NA %in% x\n", &config).is_empty());
        assert!(lint("x %in% c(NA)\n", &config).is_empty());
    }

    #[test]
    fn vector_logic_exempts_bitwise_arithmetic() {
        let config = vector_logic_only_config();
        assert!(lint("if (info & as.raw(12)) { }\n", &config).is_empty());
        assert!(lint("if (info | '011') { }\n", &config).is_empty());
        assert!(
            lint(
                "if ((mode & \"111\") != as.octmode(\"111\")) { }\n",
                &config
            )
            .is_empty()
        );
        // Only the operator with the bitwise operand is exempt.
        let diags = lint("if (x & y & \"100\") 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn vector_logic_flags_scalar_operators_in_subset_contexts() {
        let config = vector_logic_only_config();
        for text in [
            "filter(x, y && z)\n",
            "subset(x, y || z)\n",
            "x %>% filter(y && z)\n",
            "x |> filter(y && z)\n",
            "dplyr::filter(x, y && z)\n",
        ] {
            let diags = lint(text, &config);
            assert_eq!(diags.len(), 1, "{text}: got {diags:?}");
            assert!(diags[0].message.contains("subsetting"), "{text}");
        }
        // `stats::filter` is linear filtering, not subsetting.
        assert!(lint("stats::filter(x, y && z)\n", &config).is_empty());
        // The vectorised forms are fine there.
        assert!(lint("filter(x, a & b, c | d)\n", &config).is_empty());
    }

    #[test]
    fn vector_logic_nested_subset_contexts_emit_one_diagnostic() {
        // The inner `subset` is scanned when the walk reaches it; the outer
        // `filter` scan must not also descend into it (lintr emits one lint).
        let config = vector_logic_only_config();
        let diags = lint("filter(x, subset(y, a && b))\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn vector_logic_checks_expect_true_false_conditions() {
        let config = vector_logic_only_config();
        assert_eq!(lint("expect_true(TRUE & FALSE)\n", &config).len(), 1);
        assert_eq!(
            lint("testthat::expect_false(TRUE | TRUE)\n", &config).len(),
            1
        );
        assert!(lint("expect_true(any(x | y))\n", &config).is_empty());
        // The tested expression is the `object` argument, wherever it sits.
        assert_eq!(
            lint("expect_true(info = \"x\", object = x & y)\n", &config).len(),
            1
        );
    }

    #[test]
    fn vector_logic_nested_if_condition_emits_one_diagnostic() {
        // The inner `if` is scanned when the walk reaches it; the outer
        // condition scan must not also descend into it.
        let config = vector_logic_only_config();
        let diags = lint("if (if (x & y) TRUE else FALSE) 1\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn vector_logic_resolves_extracted_callees_and_partial_matching() {
        let config = vector_logic_only_config();
        // `env$as.raw(12)` is still bitwise arithmetic.
        assert!(lint("if (info & env$as.raw(12)) 1\n", &config).is_empty());
        // `env$expect_true(...)` is still a testthat condition.
        assert_eq!(lint("env$expect_true(x & y)\n", &config).len(), 1);
        // R partially matches `obj =` to expect_true's `object` formal.
        assert_eq!(lint("expect_true(obj = x & y)\n", &config).len(), 1);
    }

    #[test]
    fn vector_logic_exempts_circular_control_argument() {
        // stats::filter's scalar control argument, exempted by name in lintr.
        let config = vector_logic_only_config();
        assert!(lint("filter(data, circular = lhs && rhs)\n", &config).is_empty());
    }

    #[test]
    fn vector_logic_subset_scan_stops_at_call_boundaries() {
        // lintr leaves `filter(data, foo(a && b))` alone — the nested call is
        // its own evaluation context.
        let config = vector_logic_only_config();
        assert!(lint("filter(data, foo(a && b))\n", &config).is_empty());
        assert_eq!(lint("filter(x, y && z)\n", &config).len(), 1);
    }

    #[test]
    fn trailing_whitespace_exempts_string_interiors() {
        let config = trailing_whitespace_only_config();
        // lintr's `allow_in_strings = TRUE` default.
        assert!(lint("blah <- '  \n  \n'\n", &config).is_empty());
        // ...but whitespace past the string's closing quote still flags.
        let diags = lint("blah <- '  \n  \n'  \n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 2);
    }

    #[test]
    fn no_tab_flags_indentation_tabs_only() {
        let config = no_tab_only_config();
        // lintr's whitespace_linter only lints indentation tabs.
        assert_eq!(lint("\tx <- 1\n", &config).len(), 1);
        assert_eq!(lint("  \tx <- 1\n", &config).len(), 1);
        // Tabs in comments, strings, and between tokens are fine.
        assert!(lint("#\tblah\n", &config).is_empty());
        assert!(lint("x <- 'a\tb'\n", &config).is_empty());
        assert!(lint("x <- 1\t# note\n", &config).is_empty());
        assert!(lint("x <- c(1,\t2)\n", &config).is_empty());
        // A line starting inside a multi-line string is part of its value.
        assert!(lint("x <- 'a\n\tb'\n", &config).is_empty());
    }

    #[test]
    fn object_name_descends_compound_targets_like_lintr() {
        // lintr checks the base object through subscripts and replacement
        // calls, not just `$`/`@` chains.
        let config = object_name_only_config();
        assert_eq!(lint("badName[[1]] <- 1\n", &config).len(), 1);
        assert_eq!(lint("badName$field[[1]] <- 1\n", &config).len(), 1);
        assert_eq!(lint("names(badName) <- 1\n", &config).len(), 1);
        assert_eq!(lint("attr(badName, \"x\") <- 1\n", &config).len(), 1);
        assert!(lint("names(good_name) <- 1\n", &config).is_empty());
        // Index symbols are not assignment targets.
        assert!(lint("x[badName] <- 1\n", &config).is_empty());
    }

    #[test]
    fn assignment_operator_parenthesized_anywhere_in_argument_still_flags() {
        // lintr's exclusion requires the argument subtree to be paren-free.
        let config = LintConfig {
            line_length_severity: None,
            trailing_whitespace_severity: None,
            no_tab_severity: None,
            trailing_blank_lines_severity: None,
            ..enabled_config()
        };
        let diags = lint("fun(foo + (blah = 1))\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
    }

    #[test]
    fn function_left_parentheses_flags_call_with_comment_gap() {
        // A comment between the callee and its `(` is still a wrong-line
        // call in lintr.
        let config = function_left_parentheses_only_config();
        let diags = lint("if (foo # note\n  (x)) TRUE\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert!(diags[0].message.contains("same line"), "{:?}", diags);
    }

    #[test]
    fn object_name_recognizes_qualified_use_method_declarations() {
        // `base::UseMethod(...)` declares a generic just like the bare call.
        let config = object_name_only_config();
        let src = "my_generic <- function(x) base::UseMethod(\"my_generic\")\nmy_generic.BadName <- function(x) x\n";
        assert!(
            lint(src, &config).is_empty(),
            "got {:?}",
            lint(src, &config)
        );
    }

    #[test]
    fn function_left_parentheses_callee_edge_cases_match_lintr() {
        let config = function_left_parentheses_only_config();
        // A string callee via `::` is not symbol-like — exempt.
        assert!(lint("base::\"mean\" (1)\n", &config).is_empty());
        // `@` slot calls: same-line whitespace exempt, cross-line `(` flagged.
        assert!(lint("obj@predicate (1)\n", &config).is_empty());
        let cross = lint("if (obj@predicate\n(1)) TRUE\n", &config);
        assert_eq!(cross.len(), 1, "got {:?}", cross);
        assert!(cross[0].message.contains("same line"), "{:?}", cross);
    }

    #[test]
    fn trailing_blank_lines_reports_blanks_and_missing_newline_together() {
        let config = trailing_blank_lines_only_config();
        let diags = lint("x <- 1\n\n ", &config);
        assert_eq!(diags.len(), 2, "got {:?}", diags);
        assert!(diags.iter().any(|d| d.message.contains("newline")));
        // Without a final newline the blank-region range clamps to the last
        // existing line's end (line 2, one space wide).
        let blank = diags
            .iter()
            .find(|d| d.message.contains("blank"))
            .expect("blank-lines diagnostic");
        assert_eq!(blank.range.end.line, 2);
        assert_eq!(blank.range.end.character, 1);

        // With a final newline, the range still covers the blank region
        // through the document end.
        let diags = lint("x <- 1\n\n", &config);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert_eq!(diags[0].range.end.line, 2);
    }
}

#[cfg(test)]
mod code_field_tests {
    use super::*;
    use crate::parser_pool::with_parser;
    use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

    fn rule_id_of(d: &tower_lsp::lsp_types::Diagnostic) -> Option<&str> {
        match &d.code {
            Some(NumberOrString::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    fn lint_semantic(
        text: &str,
        mixed_sev: Option<DiagnosticSeverity>,
        cond_sev: Option<DiagnosticSeverity>,
    ) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        run_semantic_checks(text, tree.root_node(), mixed_sev, cond_sev)
    }

    fn run_one(
        text: &str,
        configure: impl FnOnce(&mut LintConfig),
    ) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let mut cfg = LintConfig::default();
        cfg.enabled = true;
        configure(&mut cfg);
        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        run_lints(text, tree.root_node(), &cfg)
    }

    fn warn(slot: &mut Option<DiagnosticSeverity>) {
        *slot = Some(DiagnosticSeverity::WARNING);
    }

    #[test]
    fn every_rule_emits_its_id() {
        use super::rule_ids::*;

        type LintCase = (&'static str, Box<dyn Fn(&mut LintConfig)>, &'static str);
        let cases: Vec<LintCase> = vec![
            (
                LINE_LENGTH,
                Box::new(|c| {
                    c.line_length = 4;
                    warn(&mut c.line_length_severity);
                }),
                "very_long_line\n",
            ),
            (
                TRAILING_WHITESPACE,
                Box::new(|c| warn(&mut c.trailing_whitespace_severity)),
                "x <- 1   \n",
            ),
            (
                NO_TAB,
                Box::new(|c| warn(&mut c.no_tab_severity)),
                "\tx <- 1\n",
            ),
            (
                TRAILING_BLANK_LINES,
                Box::new(|c| warn(&mut c.trailing_blank_lines_severity)),
                "x <- 1\n\n\n",
            ),
            (
                ASSIGNMENT_OPERATOR,
                Box::new(|c| warn(&mut c.assignment_operator_severity)),
                "x = 1\n",
            ),
            (
                OBJECT_NAME,
                Box::new(|c| warn(&mut c.object_name_severity)),
                "BadName <- 1\n",
            ),
            (
                INFIX_SPACES,
                Box::new(|c| warn(&mut c.infix_spaces_severity)),
                "x<-1+2\n",
            ),
            (
                COMMENTED_CODE,
                Box::new(|c| warn(&mut c.commented_code_severity)),
                "# x <- 1\n",
            ),
            (
                QUOTES,
                Box::new(|c| warn(&mut c.quotes_severity)),
                "x <- 'single'\n",
            ),
            (
                COMMAS,
                Box::new(|c| warn(&mut c.commas_severity)),
                "f(a ,b)\n",
            ),
            (
                T_AND_F_SYMBOL,
                Box::new(|c| warn(&mut c.t_and_f_symbol_severity)),
                "if (T) 1 else 2\n",
            ),
            (
                SEMICOLON,
                Box::new(|c| warn(&mut c.semicolon_severity)),
                "x <- 1; y <- 2\n",
            ),
            (
                EQUALS_NA,
                Box::new(|c| warn(&mut c.equals_na_severity)),
                "if (x == NA) 1\n",
            ),
            (
                OBJECT_LENGTH,
                Box::new(|c| {
                    c.object_length = 4;
                    warn(&mut c.object_length_severity);
                }),
                "very_long_name <- 1\n",
            ),
            (
                VECTOR_LOGIC,
                Box::new(|c| warn(&mut c.vector_logic_severity)),
                "if (x & y) 1\n",
            ),
            (
                FUNCTION_LEFT_PARENTHESES,
                Box::new(|c| warn(&mut c.function_left_parentheses_severity)),
                "f <- function (x) x\n",
            ),
            (
                SPACES_INSIDE,
                Box::new(|c| warn(&mut c.spaces_inside_severity)),
                "f( x )\n",
            ),
            (
                INDENTATION,
                Box::new(|c| warn(&mut c.indentation_severity)),
                "if (x) {\n   y <- 1\n}\n",
            ),
        ];

        for (expected_id, configure, fixture) in cases {
            let diags = run_one(fixture, |c| configure(c));
            let matched: Vec<_> = diags
                .iter()
                .filter(|d| rule_id_of(d) == Some(expected_id))
                .collect();
            assert!(
                !matched.is_empty(),
                "rule {} produced no diagnostic for fixture {:?}; emissions: {:?}",
                expected_id,
                fixture,
                diags
            );
        }
    }

    #[test]
    fn semantic_rules_emit_their_ids() {
        use super::rule_ids::*;
        let sev = Some(DiagnosticSeverity::WARNING);
        let ml = lint_semantic("x <- a & b | c\n", sev, None);
        assert!(
            ml.iter().any(|d| rule_id_of(d) == Some(MIXED_LOGICAL)),
            "mixed_logical rule emitted unexpected ids: {:?}",
            ml
        );
        let ca = lint_semantic("if (x = 1) x\n", None, sev);
        assert!(
            ca.iter()
                .any(|d| rule_id_of(d) == Some(CONDITION_ASSIGNMENT)),
            "condition_assignment rule emitted unexpected ids: {:?}",
            ca
        );
    }
}
