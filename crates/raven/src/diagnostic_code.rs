//! Canonical, kebab-case diagnostic/suppression code namespace (F2).
//!
//! One unified namespace spans both analyzer diagnostics (undefined variables,
//! syntax errors, …) and the opt-in lint rules. Codes are kebab-case and
//! descriptive — never opaque numbers (the TypeScript `TS2304` style is the
//! anti-pattern). They are the spelling a user writes inside a suppression
//! directive, e.g. `# raven: ignore[undefined-variable]`.
//!
//! ## Lint-config compatibility (do not break)
//!
//! Lint *rule identifiers* are NOT purely internal: `.lintr` files and the
//! `raven.toml` / VS Code lint config enable and disable rules **by name**,
//! using lintr's `snake_case` (`object_name`, `line_length`). Those config keys
//! must keep working. Only the *suppression-code spelling* is free to be
//! kebab-case. The two spellings are a pure `_`/`-` transform of each other, so
//! [`normalize`] (→ kebab, for suppression matching) and [`to_lint_rule_id`]
//! (→ snake, for config / `Diagnostic.code` lookups) bridge them without a
//! brittle hand-maintained table. Suppression parsing accepts **either**
//! spelling so a user who writes `# raven: ignore[line_length]` is honored too.
//!
//! ## Cascading sub-kinds
//!
//! Following Pyrefly/rust-analyzer, a code may be a child of an umbrella code:
//! suppressing the parent suppresses its children. The canonical example is the
//! `syntax-error` umbrella over concrete parse failures (`unclosed-paren`, …).
//! [`suppresses`] walks the parent chain so `ignore[syntax-error]` covers them
//! all, while `ignore[unclosed-paren]` targets just the one.

// ---- Analyzer diagnostic codes -------------------------------------------

/// Undefined / used-before-defined variable.
pub const UNDEFINED_VARIABLE: &str = "undefined-variable";
/// Umbrella for parse failures; parent of the concrete `*-paren`/`*-brace`/… kinds.
pub const SYNTAX_ERROR: &str = "syntax-error";
/// A `source()` / `# raven: source` path that does not resolve to a file.
pub const UNRESOLVED_SOURCE_PATH: &str = "unresolved-source-path";
/// A `source()` / forward-directive path (issue #530) **or** a backward-directive
/// path (`# raven: sourced-by` / `run-by` / `included-by`, issue #535) that
/// resolves only via a case-only difference from the real on-disk filename.
/// Host-derived severity: information on a case-insensitive filesystem (a
/// portability hazard — it will break on case-sensitive Linux), warning on a
/// case-sensitive one. The forward and backward collectors share this code but
/// emit different messages: the forward message notes R itself would error at
/// `source()` time; the backward message does not (R never executes a backward
/// directive — it is a Raven-only annotation) and instead asks to fix the
/// directive's path casing. Governed by its severity setting, not suppressible.
pub const SOURCE_PATH_CASE_MISMATCH: &str = "source-path-case-mismatch";
/// Assignment whose target is a string literal (`"x" <- 1`) or other
/// almost-certainly-unintended target.
pub const ASSIGN_TO_STRING_LITERAL: &str = "assign-to-string-literal";
/// A `library()`/`require()` of a package that is not installed / not found.
pub const PACKAGE_NOT_INSTALLED: &str = "package-not-installed";
/// A missing package that is installed in a vanilla R library removed from
/// `.libPaths()` by the workspace's active renv project.
pub const PACKAGE_OUTSIDE_ACTIVE_LIBRARY: &str = "package-outside-active-library";
/// A `pkg::member` reference where a *complete* package export set has no such
/// exported member or data object. Never emitted for `pkg:::member` (internal
/// access) or from partial/unknown metadata. See `namespace_member_status_sync`.
pub const NAMESPACE_MEMBER_NOT_FOUND: &str = "namespace-member-not-found";
/// A static local `box::use()` spec that resolves to no file module or `__init__`.
pub const BOX_MODULE_NOT_FOUND: &str = "box-module-not-found";
/// A static local `box::use()` spec whose on-disk spelling differs only by case.
pub const BOX_MODULE_CASE_MISMATCH: &str = "box-module-case-mismatch";
/// A selected `box::use()` attachment that is absent from a complete export set.
pub const BOX_EXPORT_NOT_FOUND: &str = "box-export-not-found";
/// A `# raven: expect[...]` (or, under the global sweep, any suppression)
/// that suppressed nothing. Hint severity. F2.
pub const UNUSED_SUPPRESSION: &str = "unused-suppression";

/// A chained comparison (`0 < x < 1`, `a == b == c`) — a parse error in R
/// that tree-sitter-r accepts silently, reported by
/// `collect_chained_comparison_errors` in `handlers.rs`. A `syntax-error`
/// sub-kind, so (like every parse error) it is not suppressible.
pub const CHAINED_COMPARISON: &str = "chained-comparison";

/// Concrete `syntax-error` sub-kinds. Each maps to [`SYNTAX_ERROR`] as its
/// parent so suppressing the umbrella suppresses all of them.
pub const SYNTAX_ERROR_CHILDREN: &[&str] = &[
    "unclosed-paren",
    "unclosed-brace",
    "unclosed-bracket",
    "unexpected-token",
    "missing-brace",
    CHAINED_COMPARISON,
];

/// All canonical analyzer codes (umbrella codes included, children excluded).
pub const ANALYZER_CODES: &[&str] = &[
    UNDEFINED_VARIABLE,
    SYNTAX_ERROR,
    UNRESOLVED_SOURCE_PATH,
    SOURCE_PATH_CASE_MISMATCH,
    ASSIGN_TO_STRING_LITERAL,
    PACKAGE_NOT_INSTALLED,
    NAMESPACE_MEMBER_NOT_FOUND,
    BOX_MODULE_NOT_FOUND,
    BOX_MODULE_CASE_MISMATCH,
    BOX_EXPORT_NOT_FOUND,
    UNUSED_SUPPRESSION,
];

/// Analyzer codes that ignore directives may suppress.
///
/// This includes suppressible canonical analyzer codes from [`ANALYZER_CODES`]
/// and independently suppressible child codes. Deliberately excludes
/// `syntax-error` (parse errors are not suppressible), `unresolved-source-path`
/// and the other dependency-graph diagnostics (governed only by their severity
/// settings), and `unused-suppression` itself. Used by the range/file/chunk
/// post-filter so block- and chunk-level suppression covers these codes the same
/// way the inline per-line checks do. See `docs/linting.md`.
pub const SUPPRESSIBLE_ANALYZER_CODES: &[&str] = &[
    UNDEFINED_VARIABLE,
    ASSIGN_TO_STRING_LITERAL,
    PACKAGE_NOT_INSTALLED,
    PACKAGE_OUTSIDE_ACTIVE_LIBRARY,
    NAMESPACE_MEMBER_NOT_FOUND,
    BOX_EXPORT_NOT_FOUND,
];

/// Canonical lint codes, kebab-case (the suppression spelling of the
/// `snake_case` rule identifiers in [`crate::linting::rule_ids`]).
pub const LINT_CODES: &[&str] = &[
    "line-length",
    "trailing-whitespace",
    "no-tab",
    "trailing-blank-lines",
    "assignment-operator",
    "object-name",
    "infix-spaces",
    "commented-code",
    "quotes",
    "commas",
    "t-and-f-symbol",
    "semicolon",
    "equals-na",
    "object-length",
    "vector-logic",
    "function-left-parentheses",
    "spaces-inside",
    "indentation",
    "mixed-logical",
    "condition-assignment",
];

/// Normalize a user-written suppression code to its canonical kebab-case
/// spelling: trim, lowercase, and map `_` → `-`. This accepts both the
/// kebab-case suppression spelling and the lintr `snake_case` rule-id spelling.
pub fn normalize(input: &str) -> String {
    input.trim().to_ascii_lowercase().replace('_', "-")
}

/// Map a (kebab-case) suppression code to the `snake_case` lint rule identifier
/// used by `.lintr`, `raven.toml`, the VS Code lint config, and the
/// `Diagnostic.code` emitted by lint rules. Inverse of the kebab spelling.
pub fn to_lint_rule_id(code: &str) -> String {
    normalize(code).replace('-', "_")
}

/// The umbrella parent of a (cascading) sub-kind code, if any.
pub fn parent(code: &str) -> Option<&'static str> {
    let norm = normalize(code);
    if SYNTAX_ERROR_CHILDREN.contains(&norm.as_str()) {
        return Some(SYNTAX_ERROR);
    }
    if norm == PACKAGE_OUTSIDE_ACTIVE_LIBRARY {
        return Some(PACKAGE_NOT_INSTALLED);
    }
    None
}

/// Is `code` a suppressible diagnostic code — i.e., one that the suppression
/// machinery can actually match and silence? Codes in [`LINT_CODES`] and
/// [`SUPPRESSIBLE_ANALYZER_CODES`] are suppressible. Non-suppressible codes
/// (like `syntax-error` and its children, `unresolved-source-path`, etc.)
/// can never be matched by a directive, so an `expect` targeting only
/// non-suppressible codes should not report `unused-suppression`.
pub fn is_suppressible(code: &str) -> bool {
    let norm = normalize(code);
    LINT_CODES.contains(&norm.as_str()) || SUPPRESSIBLE_ANALYZER_CODES.contains(&norm.as_str())
}

/// Does a suppression code written by the user cover a diagnostic's code?
///
/// True when the (normalized) codes are equal, or when the suppression code is
/// an ancestor of the diagnostic code via the cascading sub-kind chain
/// (`ignore[syntax-error]` covers `unclosed-paren`). Comparison is spelling-
/// agnostic (`line_length` and `line-length` match).
pub fn suppresses(suppression_code: &str, diagnostic_code: &str) -> bool {
    let want = normalize(suppression_code);
    let mut cur = normalize(diagnostic_code);
    if want == cur {
        return true;
    }
    while let Some(p) = parent(&cur) {
        if want == p {
            return true;
        }
        cur = p.to_string();
    }
    false
}

/// `Diagnostic.data` marker tagging an `undefined-variable` diagnostic as a
/// **position/ordering variant**: the symbol exists but is not visible at the
/// use site — a forward reference (defined later in the same file) or a symbol
/// brought in by a `source()` that runs later. These are NOT resolvable by
/// package export metadata.
///
/// The emitters set this so consumers can distinguish the variant from a
/// genuinely-missing symbol **without parsing the free-prose message** — the
/// message prepends the raw (possibly backtick-quoted) symbol name, so any
/// substring test over it can be spoofed by a pathological name. The plain
/// "genuinely undefined" variant leaves `data` unset. Read by the CLI's
/// missing-export-metadata gate (`raven check`).
pub const UNDEFINED_VARIABLE_POSITION_VARIANT: &str = "undefined-variable/position-variant";

/// The structured NSE-discoverability hint carried by an undefined-variable
/// diagnostic whose flagged identifier sits inside a call argument that *might*
/// be captured by non-standard evaluation (see `nse_hint_for_usage` in
/// `handlers.rs`). The emitter stores the structured fields here and **never**
/// appends them to the diagnostic message: the editor and `--format json/sarif`
/// carry no NSE prose (their message stays the bare "`x` is not defined"), and
/// the hint surfaces only once, as the deduplicated footer the human-readable
/// `raven check` text report builds via [`nse_footer_directives`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NseHint {
    /// Source spelling of the callee, retaining backticks for a non-syntactic
    /// name (e.g. `` `my fn` ``). Used only in the message prose.
    pub callee: String,
    /// Directive spelling of the callee (a non-syntactic name is quoted, a
    /// qualifier kept) — what the user copies into the directive.
    pub dir: String,
    /// The matched formal name when the flagged argument is named (`foo(x =
    /// ...)` → `Some("x")`); `None` for a positional argument, where the formal
    /// order is unknown and the user must fill in placeholders.
    pub formal: Option<String>,
}

/// The copy-pasteable directive lines for the deduplicated `raven check` footer
/// (see `format_nse_hint_footer`), built from all of a run's NSE hints. This is
/// the only rendered form of the hints — they are deliberately not appended to
/// the diagnostic message.
///
/// Aggregation is per **callee** and is load-bearing for correctness, because
/// `# raven: nse` is *last-declaration-wins* (see `own_directive_nse_policy` in
/// `handlers.rs`): emitting one directive per formal would leave only the last
/// one in effect, so a user who pasted them would still see the earlier
/// findings. Therefore:
///   - All named formals captured by one callee collapse into a single
///     `# raven: nse callee(x, y, …)` directive (formals sorted for
///     determinism).
///   - A positional argument (formal order unknown) yields the two-line
///     `# raven: func callee(<formals>)` + `# raven: nse callee(<nse-formals>)`
///     placeholder pair the user fills in. The pair is on **two separate lines**
///     because each `# raven:` directive must be the only one on its comment
///     line (the `nse` regex in `cross_file::directive` is start- AND
///     end-anchored), so a one-line `func … and nse …` form would parse the
///     `func` half but silently drop the NSE contract. When a callee has *any*
///     positional finding, only this placeholder form is emitted (the user fills
///     `<nse-formals>` with every captured formal, named ones included), so the
///     two forms never collide for the same callee.
///
/// Named-formal directives sort first; the wordier positional pair sorts last.
pub fn nse_footer_directives(hints: &[NseHint]) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};
    // callee directive spelling -> (named formals, has a positional finding)
    let mut by_callee: BTreeMap<&str, (BTreeSet<&str>, bool)> = BTreeMap::new();
    for h in hints {
        let entry = by_callee.entry(h.dir.as_str()).or_default();
        match &h.formal {
            Some(f) => {
                entry.0.insert(f.as_str());
            }
            None => entry.1 = true,
        }
    }
    let mut out: Vec<String> = Vec::new();
    for (dir, (formals, has_positional)) in &by_callee {
        if *has_positional {
            out.push(format!(
                "# raven: func {dir}(<formals>)\n# raven: nse {dir}(<nse-formals>)"
            ));
        } else if !formals.is_empty() {
            let list = formals.iter().copied().collect::<Vec<_>>().join(", ");
            out.push(format!("# raven: nse {dir}({list})"));
        }
    }
    let is_positional = |s: &str| s.starts_with("# raven: func");
    out.sort_by(|a, b| (is_positional(a), a.as_str()).cmp(&(is_positional(b), b.as_str())));
    out
}

/// Build the `Diagnostic.data` payload for an undefined-variable diagnostic from
/// its two independent, composable markers: whether it is a position variant
/// (forward reference or sourced-later — not resolvable by package export
/// metadata) and an optional NSE-discoverability hint. Returns `None` (leaving
/// `data` unset) for a plain genuinely-undefined usage with neither marker, so
/// the common case stays allocation-free.
pub fn undefined_variable_data(
    position_variant: bool,
    nse_hint: Option<&NseHint>,
) -> Option<serde_json::Value> {
    if !position_variant && nse_hint.is_none() {
        return None;
    }
    let mut obj = serde_json::Map::new();
    if position_variant {
        obj.insert("positionVariant".to_string(), serde_json::Value::Bool(true));
    }
    if let Some(h) = nse_hint {
        let mut hint = serde_json::Map::new();
        hint.insert(
            "callee".to_string(),
            serde_json::Value::String(h.callee.clone()),
        );
        hint.insert("dir".to_string(), serde_json::Value::String(h.dir.clone()));
        if let Some(f) = &h.formal {
            hint.insert("formal".to_string(), serde_json::Value::String(f.clone()));
        }
        obj.insert("nseHint".to_string(), serde_json::Value::Object(hint));
    }
    Some(serde_json::Value::Object(obj))
}

/// True if `data` marks the diagnostic as a position variant (forward reference
/// or sourced-later). Read by the CLI's missing-export-metadata gate.
pub fn is_undefined_variable_position_variant(data: &Option<serde_json::Value>) -> bool {
    matches!(
        data,
        Some(serde_json::Value::Object(obj))
            if obj.get("positionVariant") == Some(&serde_json::Value::Bool(true))
    )
}

/// Recover the structured NSE hint from `data`, or `None` if the diagnostic
/// carries no hint. Inverse of [`undefined_variable_data`]'s `nse_hint` field.
pub fn undefined_variable_nse_hint(data: &Option<serde_json::Value>) -> Option<NseHint> {
    let obj = data.as_ref()?.as_object()?;
    let hint = obj.get("nseHint")?.as_object()?;
    Some(NseHint {
        callee: hint.get("callee")?.as_str()?.to_string(),
        dir: hint.get("dir")?.as_str()?.to_string(),
        formal: hint
            .get("formal")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// The `Diagnostic.data` payload with the internal NSE hint removed, for
/// serialization into machine output. The hint exists only so the `raven check`
/// text footer can aggregate it; it is not part of the diagnostic's machine
/// contract. Machine
/// consumers still get the position-variant marker (historically present in
/// `data`), but no NSE trace. Returns `None` when nothing remains, matching the
/// "omit empty `data`" convention `Diagnostic` serializes with.
pub fn data_without_nse_hint(data: &Option<serde_json::Value>) -> Option<serde_json::Value> {
    let Some(serde_json::Value::Object(obj)) = data else {
        // Non-object (or absent) data carries no hint to strip.
        return data.clone();
    };
    let mut obj = obj.clone();
    obj.remove("nseHint");
    if obj.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(obj))
    }
}

/// True when a diagnostic's `code` is the canonical string code `want`.
///
/// Centralizes the `Some(NumberOrString::String(_))` match so callers that ask
/// "is this an X diagnostic?" read as intent rather than boilerplate, and stay
/// correct in one place if the `code` representation ever changes. Prefer this
/// over message-text matching: the code is the stable handle, the message is
/// free prose (see the `undefined-variable` reword).
pub fn diagnostic_has_code(
    code: &Option<tower_lsp::lsp_types::NumberOrString>,
    want: &str,
) -> bool {
    matches!(code, Some(tower_lsp::lsp_types::NumberOrString::String(c)) if c == want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_has_code_matches_only_the_string_code() {
        use tower_lsp::lsp_types::NumberOrString;
        let matching = Some(NumberOrString::String(UNDEFINED_VARIABLE.to_string()));
        assert!(diagnostic_has_code(&matching, UNDEFINED_VARIABLE));
        assert!(!diagnostic_has_code(&matching, SYNTAX_ERROR));
        // A different code, a numeric code, and an absent code never match.
        let other = Some(NumberOrString::String(SYNTAX_ERROR.to_string()));
        assert!(!diagnostic_has_code(&other, UNDEFINED_VARIABLE));
        assert!(!diagnostic_has_code(
            &Some(NumberOrString::Number(7)),
            UNDEFINED_VARIABLE
        ));
        assert!(!diagnostic_has_code(&None, UNDEFINED_VARIABLE));
    }

    #[test]
    fn position_variant_marker_round_trips_and_is_specific() {
        let tagged = undefined_variable_data(true, None);
        assert!(is_undefined_variable_position_variant(&tagged));
        // Absent data, a bare string, and an object without the flag never match.
        assert!(!is_undefined_variable_position_variant(&None));
        assert!(!is_undefined_variable_position_variant(&Some(
            serde_json::Value::String("something-else".to_string())
        )));
        assert!(!is_undefined_variable_position_variant(&Some(
            serde_json::json!({ "kind": UNDEFINED_VARIABLE_POSITION_VARIANT })
        )));
    }

    fn named_hint() -> NseHint {
        NseHint {
            callee: "aes".to_string(),
            dir: "aes".to_string(),
            formal: Some("x".to_string()),
        }
    }

    fn positional_hint() -> NseHint {
        NseHint {
            callee: "facet_wrap".to_string(),
            dir: "facet_wrap".to_string(),
            formal: None,
        }
    }

    fn named_hint_for(dir: &str, formal: &str) -> NseHint {
        NseHint {
            callee: dir.to_string(),
            dir: dir.to_string(),
            formal: Some(formal.to_string()),
        }
    }

    #[test]
    fn nse_footer_directives_render_the_copy_pasteable_tail() {
        assert_eq!(
            nse_footer_directives(&[named_hint()]),
            vec!["# raven: nse aes(x)".to_string()]
        );
        // The positional fallback renders the two directives on SEPARATE lines:
        // the `nse` directive regex is end-anchored, so a one-line `func … and
        // nse …` form would silently fail to declare the NSE contract (see
        // `positional_directive_parses_as_two_directives`).
        assert_eq!(
            nse_footer_directives(&[positional_hint()]),
            vec![
                "# raven: func facet_wrap(<formals>)\n# raven: nse facet_wrap(<nse-formals>)"
                    .to_string()
            ]
        );
    }

    #[test]
    fn nse_footer_collapses_multiple_named_formals_for_one_callee() {
        // Regression: `# raven: nse` is last-declaration-wins, so one directive
        // per formal would leave only the last in effect — the user pasting both
        // would still see the other finding. All named formals for one callee
        // must collapse into a single directive (formals sorted).
        let dirs = nse_footer_directives(&[
            named_hint_for("somepkg::f", "x"),
            named_hint_for("somepkg::f", "y"),
        ]);
        assert_eq!(dirs, vec!["# raven: nse somepkg::f(x, y)".to_string()]);
        // And the collapsed directive actually captures BOTH formals when parsed.
        let meta = crate::cross_file::directive::parse_directives(&dirs[0]);
        assert_eq!(meta.nse_declarations.len(), 1, "{dirs:?}");
    }

    #[test]
    fn nse_footer_mixed_named_and_positional_for_one_callee_emits_only_the_pair() {
        // A callee with both a named finding (`f(x = ...)`) and a positional one
        // (`f(undef)`): the formal order is unknown for the positional arg, so we
        // emit ONLY the placeholder pair (the user fills `<nse-formals>` with
        // every captured formal). Emitting `# raven: nse f(x)` alongside it would
        // collide under last-declaration-wins, re-introducing the original bug.
        let dirs = nse_footer_directives(&[
            named_hint_for("somepkg::f", "x"),
            NseHint {
                callee: "somepkg::f".to_string(),
                dir: "somepkg::f".to_string(),
                formal: None,
            },
        ]);
        assert_eq!(
            dirs,
            vec![
                "# raven: func somepkg::f(<formals>)\n# raven: nse somepkg::f(<nse-formals>)"
                    .to_string()
            ],
            "mixed case emits only the positional placeholder pair"
        );
    }

    #[test]
    fn positional_directive_parses_as_two_directives() {
        // Regression: a user who copies the positional suggestion and fills in
        // the placeholders must get BOTH a `func` and an `nse` declaration. The
        // `nse` directive regex is start- AND end-anchored, so the previous
        // one-line `# raven: func … and # raven: nse …` form parsed only the
        // `func` half and silently dropped the NSE contract.
        let applied = nse_footer_directives(&[positional_hint()])[0]
            .replace("<formals>", "data, x")
            .replace("<nse-formals>", "x");
        let meta = crate::cross_file::directive::parse_directives(&applied);
        assert_eq!(
            meta.declared_functions.len(),
            1,
            "func directive must parse: {applied:?}"
        );
        assert_eq!(
            meta.nse_declarations.len(),
            1,
            "nse directive must parse (one-line form silently dropped it): {applied:?}"
        );
    }

    #[test]
    fn undefined_variable_data_composes_both_markers() {
        // Neither marker → no data at all (plain undefined keeps `data` unset).
        assert_eq!(undefined_variable_data(false, None), None);

        // NSE hint alone: round-trips, and is NOT a position variant.
        let hint = named_hint();
        let nse_only = undefined_variable_data(false, Some(&hint));
        assert!(!is_undefined_variable_position_variant(&nse_only));
        assert_eq!(undefined_variable_nse_hint(&nse_only), Some(hint.clone()));

        // Both markers compose on one diagnostic (a forward reference that also
        // sits inside an NSE-capturing call argument).
        let both = undefined_variable_data(true, Some(&hint));
        assert!(is_undefined_variable_position_variant(&both));
        assert_eq!(undefined_variable_nse_hint(&both), Some(hint));

        // Position variant alone carries no NSE hint.
        let pos_only = undefined_variable_data(true, None);
        assert_eq!(undefined_variable_nse_hint(&pos_only), None);
    }

    #[test]
    fn data_without_nse_hint_strips_only_the_hint() {
        let hint = named_hint();

        // Hint alone → nothing left, collapses to None (omitted on serialize).
        let nse_only = undefined_variable_data(false, Some(&hint));
        assert_eq!(data_without_nse_hint(&nse_only), None);

        // Both markers → the position-variant marker survives, hint is gone.
        let both = undefined_variable_data(true, Some(&hint));
        let stripped = data_without_nse_hint(&both);
        assert!(is_undefined_variable_position_variant(&stripped));
        assert_eq!(undefined_variable_nse_hint(&stripped), None);

        // No hint to begin with → unchanged (still a position variant).
        let pos_only = undefined_variable_data(true, None);
        assert_eq!(data_without_nse_hint(&pos_only), pos_only);

        // Absent data → absent.
        assert_eq!(data_without_nse_hint(&None), None);
    }

    #[test]
    fn nse_hint_round_trips_a_positional_formal_omission() {
        let hint = positional_hint();
        let data = undefined_variable_data(false, Some(&hint));
        assert_eq!(undefined_variable_nse_hint(&data), Some(hint));
    }

    #[test]
    fn normalize_accepts_both_spellings() {
        assert_eq!(normalize("line_length"), "line-length");
        assert_eq!(normalize("line-length"), "line-length");
        assert_eq!(normalize("  Line-Length  "), "line-length");
    }

    #[test]
    fn to_lint_rule_id_round_trips_to_snake_case() {
        assert_eq!(to_lint_rule_id("line-length"), "line_length");
        assert_eq!(to_lint_rule_id("line_length"), "line_length");
        assert_eq!(to_lint_rule_id("object-name"), "object_name");
    }

    #[test]
    fn every_lint_rule_id_has_a_kebab_code_and_round_trips() {
        use crate::linting::rule_ids;
        let rule_ids = [
            rule_ids::LINE_LENGTH,
            rule_ids::TRAILING_WHITESPACE,
            rule_ids::NO_TAB,
            rule_ids::TRAILING_BLANK_LINES,
            rule_ids::ASSIGNMENT_OPERATOR,
            rule_ids::OBJECT_NAME,
            rule_ids::INFIX_SPACES,
            rule_ids::COMMENTED_CODE,
            rule_ids::QUOTES,
            rule_ids::COMMAS,
            rule_ids::T_AND_F_SYMBOL,
            rule_ids::SEMICOLON,
            rule_ids::EQUALS_NA,
            rule_ids::OBJECT_LENGTH,
            rule_ids::VECTOR_LOGIC,
            rule_ids::FUNCTION_LEFT_PARENTHESES,
            rule_ids::SPACES_INSIDE,
            rule_ids::INDENTATION,
            rule_ids::MIXED_LOGICAL,
            rule_ids::CONDITION_ASSIGNMENT,
        ];
        for id in rule_ids {
            let code = normalize(id);
            assert!(
                LINT_CODES.contains(&code.as_str()),
                "rule id {id} → {code} must be in LINT_CODES"
            );
            assert_eq!(to_lint_rule_id(&code), id, "round trip for {id}");
        }
    }

    #[test]
    fn suppresses_exact_and_spelling_agnostic() {
        assert!(suppresses("undefined-variable", "undefined-variable"));
        assert!(suppresses("line_length", "line-length"));
        assert!(suppresses("line-length", "line_length"));
        assert!(!suppresses("line-length", "object-name"));
    }

    #[test]
    fn suppresses_cascades_from_umbrella_to_children() {
        for child in SYNTAX_ERROR_CHILDREN {
            assert!(
                suppresses(SYNTAX_ERROR, child),
                "syntax-error must cover {child}"
            );
            // The child does not cover the umbrella or its siblings.
            assert!(!suppresses(child, SYNTAX_ERROR));
        }
        assert!(suppresses("unclosed-paren", "unclosed-paren"));
        assert!(!suppresses("unclosed-paren", "unclosed-brace"));
        assert!(suppresses(
            PACKAGE_NOT_INSTALLED,
            PACKAGE_OUTSIDE_ACTIVE_LIBRARY
        ));
        assert!(!suppresses(
            PACKAGE_OUTSIDE_ACTIVE_LIBRARY,
            PACKAGE_NOT_INSTALLED
        ));
    }

    #[test]
    fn is_suppressible_identifies_suppressible_codes() {
        // Lint codes are suppressible.
        assert!(is_suppressible("line-length"));
        assert!(is_suppressible("line_length")); // snake_case accepted
        // Suppressible analyzer codes.
        assert!(is_suppressible("undefined-variable"));
        assert!(is_suppressible("assign-to-string-literal"));
        assert!(is_suppressible("package-not-installed"));
        assert!(is_suppressible("package-outside-active-library"));
        // Non-suppressible codes.
        assert!(!is_suppressible("syntax-error"));
        assert!(!is_suppressible("unclosed-paren"));
        assert!(!is_suppressible("unresolved-source-path"));
        assert!(!is_suppressible("unused-suppression"));
    }

    #[test]
    fn source_path_case_mismatch_is_registered_and_not_suppressible() {
        // Issue #530: a registered analyzer code, governed only by its severity
        // setting (like unresolved-source-path), never by `# raven: ignore`.
        assert!(ANALYZER_CODES.contains(&SOURCE_PATH_CASE_MISMATCH));
        assert!(!is_suppressible(SOURCE_PATH_CASE_MISMATCH));
        assert_eq!(SOURCE_PATH_CASE_MISMATCH, "source-path-case-mismatch");
    }
}
