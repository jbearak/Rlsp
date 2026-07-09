//! `.lintr` subset reader.
//!
//! `.lintr` is a DCF (Debian Control Format)-style file. Each field begins
//! with `Name:` at column zero; lines that begin with whitespace continue
//! the previous field's value. This reader:
//!
//! 1. Folds continuation lines into per-field values.
//! 2. Token-scans the folded `linters:` and `exclusions:` values, looking
//!    for the documented forms in `docs/linting.md`.
//!
//! Unrecognized linters log warnings; the rest of the file still applies.

use std::path::Path;

use serde_json::{Value, json};

pub struct LoadedLintr {
    pub settings: Value,
    pub warnings: Vec<String>,
}

pub fn load(path: &Path) -> Option<LoadedLintr> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            log::warn!(".lintr: cannot read {}: {}", path.display(), e);
            return None;
        }
    };
    Some(load_str(&text))
}

pub fn load_str(text: &str) -> LoadedLintr {
    let mut warnings = Vec::new();
    let FoldResult {
        fields,
        column0_continuation_count,
    } = dcf_fold(text);
    let mut linting = serde_json::Map::new();
    let mut overrides: Vec<Value> = Vec::new();
    let mut unrecognized_constructs = 0usize;
    // Whether the file contained a recognized linting-config field (`linters:`
    // or `exclusions:`). This is the "expresses linting intent" signal that
    // gates auto-enable: a recognized field is present even for
    // `linters_with_defaults()`, which sets no individual keys, but NOT for a
    // blank/whitespace-only file or one with only unknown fields.
    let mut expresses_config = false;

    for (key, value) in fields {
        // A recognized `linters:`/`exclusions:` field expresses linting intent —
        // so it opts the project in (sets `expresses_config`) and is parsed
        // best-effort — regardless of whether its value is perfectly formed. A
        // bracket imbalance (a missing `)`, or a stray extra one) is almost
        // always a typo, so point at it specifically to help the user locate it;
        // but we still attempt to parse and still opt in, because the
        // alternative — dropping the whole field and treating the file as
        // expressing nothing — would let a single typo silently turn OFF all
        // linting (the user sees no diagnostics and assumes their code is clean)
        // or discard the well-formed customizations alongside the broken one.
        if is_lintr_field(&key) && brackets_unbalanced(&value) {
            warnings.push(format!(
                ".lintr: field '{}' has unbalanced brackets (likely a stray or missing ')'); parsing it best-effort",
                key
            ));
        }
        match key.as_str() {
            "linters" => {
                expresses_config = true;
                apply_linters(
                    &value,
                    &mut linting,
                    &mut warnings,
                    &mut unrecognized_constructs,
                );
            }
            "exclusions" => {
                expresses_config = true;
                apply_exclusions(&value, &mut overrides, &mut unrecognized_constructs);
            }
            other => {
                warnings.push(format!(".lintr: unknown field '{}'; ignoring", other));
            }
        }
    }
    if column0_continuation_count > 0 {
        // Reversible UX knob (see docs/superpowers plan): Raven accepts the
        // column-0 continuation that lintr's read.dcf rejects, but flags it so a
        // user who also runs lintr learns the file is not lintr-portable.
        warnings.push(format!(
            ".lintr: accepted {} continuation line(s) beginning at column 0; lintr's read.dcf requires every continuation line (including the closing `)`) to be indented (\"Regular lines must have a tag\"). Indent them for lintr compatibility.",
            column0_continuation_count,
        ));
    }
    if unrecognized_constructs > 0 {
        warnings.push(format!(
            ".lintr: ignoring {} unrecognized construct(s); see docs/linting.md for the supported subset",
            unrecognized_constructs
        ));
    }
    if !overrides.is_empty() {
        linting.insert("overrides".into(), Value::Array(overrides));
    }
    let mut settings = serde_json::Map::new();
    if !linting.is_empty() || expresses_config {
        // `.lintr` does not contribute the `enabled` master switch. The enable
        // signal is derived from discovery state (see #281): when
        // `parse_lint_config` is called with `lintr_discovered = true`, the
        // default `"auto"` resolves to on. This keeps "drop a configured .lintr
        // to opt in" working without overriding an explicit client `false`.
        //
        // We emit the `linting` object whenever the file expressed linting
        // config — even when that config sets no individual keys (a bare
        // `linters_with_defaults()`) — so the presence of this object is the
        // single signal callers use to tell a *configured* `.lintr` (which
        // opts in) from a blank/empty one (which must NOT). See
        // `config_file::lintr_expresses_linting`.
        settings.insert("linting".into(), Value::Object(linting));
    }
    LoadedLintr {
        settings: Value::Object(settings),
        warnings,
    }
}

/// Result of folding a `.lintr` file into per-field values.
struct FoldResult {
    /// `(key, value)` pairs in file order.
    fields: Vec<(String, String)>,
    /// Count of continuation lines that began at **column 0** (no leading
    /// whitespace). lintr's `read.dcf` rejects these ("Regular lines must have a
    /// tag"); Raven accepts them leniently and surfaces one informational note
    /// (using this count) so a user who also runs lintr learns the file is not
    /// lintr-portable.
    column0_continuation_count: usize,
}

/// Fold a `.lintr` into per-field values.
///
/// A `.lintr` field value is an **R expression**; its continuation is governed
/// by **bracket balance**, not DCF leading-whitespace. So:
///
/// * While the current field's accumulated value has open brackets
///   (string/comment-aware, tracked by the incremental [`ScanState`]), the next
///   physical line continues it **regardless of indentation** — this is what
///   lets a closing `)` at column 0 still attach to its field.
/// * Otherwise the classic DCF rule applies: a line starting with whitespace
///   continues the previous value; a column-0 `Name:` line starts a new field.
///
/// Continuation lines are joined with `\n` (matching R's `read.dcf`), so a
/// trailing `#` comment terminates at its own line instead of commenting out
/// the rest of the folded value.
fn dcf_fold(text: &str) -> FoldResult {
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut column0_continuation_count = 0usize;
    let mut current: Option<(String, String)> = None;
    // Bracket/string/comment state for `current`'s accumulated value, advanced
    // incrementally one line at a time. Re-scanning the whole accumulated value
    // on every physical line would be O(n^2) in the field's length; carrying the
    // state here keeps folding linear. Invariant: whenever `current` is `Some`,
    // `st` has been fed the characters of its value (plus inert line-break
    // `'\n'`s, which never change bracket depth or string state), so `st.depth`
    // tracks the value's open-bracket depth. It is reset on every new field.
    let mut st = ScanState::default();
    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        // Advance `st` over the newline that separates this physical line from
        // the previous one *within the current field*, before making any
        // decision about this line. This closes a `#` comment left open at the
        // end of the previous line (comments end at the newline) while keeping a
        // string literal that legitimately spans the break open — so `st`'s
        // `in_str` / `in_comment` accurately describe the start of this line.
        if current.is_some() {
            st.step('\n');
        }
        // Mid-expression: brackets are still open, so this physical line
        // continues the value no matter its indentation — UNLESS it is itself a
        // recognized `.lintr` field header at column 0 that is NOT inside a
        // string literal. A missing `)` would otherwise fold the rest of the
        // file (including a following `exclusions:` field) into one value and
        // silently drop it; breaking on a recognized header instead leaves the
        // unbalanced field for `load_str` to flag and recovers the following
        // field. A `linters:`/`exclusions:` that falls inside an open string is
        // literal text, not a header, so the string guard keeps folding it (the
        // preceding `\n` step already closed any open comment).
        if st.depth > 0
            && !(st.in_str.is_none() && starts_new_field(raw_line))
            && let Some((_, val)) = current.as_mut()
        {
            if trimmed.is_empty() {
                // Blank lines inside an open bracket are insignificant in R.
                continue;
            }
            // A column-0 `#` comment line is valid DCF (read.dcf keeps comment
            // lines), so it is not a non-portable column-0 continuation and must
            // not be counted toward the portability note.
            if !raw_line.starts_with(|c: char| c.is_whitespace()) && !trimmed.starts_with('#') {
                column0_continuation_count += 1;
            }
            val.push('\n');
            val.push_str(trimmed);
            feed_str(&mut st, trimmed);
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if raw_line.starts_with(|c: char| c.is_whitespace()) {
            // DCF continuation: balanced value, indented line.
            if let Some((_, val)) = current.as_mut() {
                val.push('\n');
                val.push_str(trimmed);
                feed_str(&mut st, trimmed);
            }
            continue;
        }
        // Column 0, non-blank: a new field (a balanced value, or a recognized
        // header interrupting an unbalanced one).
        if let Some(kv) = current.take() {
            fields.push(kv);
        }
        st = ScanState::default();
        if let Some(colon) = raw_line.find(':') {
            let key = raw_line[..colon].trim().to_string();
            let val = raw_line[colon + 1..].trim().to_string();
            feed_str(&mut st, &val);
            current = Some((key, val));
        }
        // A column-0 line with no colon while balanced is malformed; drop it
        // (it cannot be a continuation — brackets are closed).
    }
    if let Some(kv) = current.take() {
        fields.push(kv);
    }
    FoldResult {
        fields,
        column0_continuation_count,
    }
}

/// Advance `st` over every character of `s`. The single "feed a string into the
/// scanner" primitive used by [`dcf_fold`] for both a field's initial value and
/// each folded continuation line, so the two cannot drift on how text is
/// scanned. Callers that need a newline boundary first step `'\n'` themselves
/// (see `dcf_fold`'s per-line separator handling).
fn feed_str(st: &mut ScanState, s: &str) {
    for c in s.chars() {
        st.step(c);
    }
}

/// True if `line` begins (at column 0) a recognized `.lintr` field header —
/// `linters:` or `exclusions:` (see [`is_lintr_field`]). Used by [`dcf_fold`] to
/// stop a bracket-unbalanced field from swallowing the next field as a
/// continuation. Restricted to the *recognized* field names so it can never
/// misfire on an R `a:b` sequence that happens to sit at column 0 inside a
/// multi-line expression. Callers also gate on string state so a header inside a
/// multi-line string literal is treated as content, not a new field.
fn starts_new_field(line: &str) -> bool {
    if line.starts_with(|c: char| c.is_whitespace()) {
        return false;
    }
    match line.find(':') {
        Some(colon) => is_lintr_field(line[..colon].trim()),
        None => false,
    }
}

/// The `.lintr` field names this reader recognizes. Single source of truth for
/// the recognized set shared by [`starts_new_field`] and the dispatch in
/// [`load_str`].
fn is_lintr_field(key: &str) -> bool {
    matches!(key, "linters" | "exclusions")
}

/// Scan the body of `linters: linters_with_defaults(...)` (or a bare expression).
/// Recognizes top-level calls of the shape `name(args)` or `name = NULL`.
fn apply_linters(
    body: &str,
    linting: &mut serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
    unrecognized_constructs: &mut usize,
) {
    let body = strip_comments(body);
    let inner = strip_linters_with_defaults(&body);
    let entries = split_top_level_commas(inner);
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Recognize the function-call shape FIRST so that named-arg linter
        // calls like `assignment_linter(operator = "<-")` aren't
        // misclassified as `name = rhs` and silently dropped. Only when
        // the entry isn't a `name(args)` call do we fall through to the
        // `name = NULL` (rule-disable) shape.
        if let Some(paren_idx) = entry.find('(') {
            if !entry.ends_with(')') {
                *unrecognized_constructs += 1;
                continue;
            }
            let name = entry[..paren_idx].trim();
            // A linter call's name is a bare identifier. A top-level `=` before
            // the `(` means a `lhs = rhs(args)` assignment form (not a linter
            // call), so count it as unrecognized rather than passing a garbled
            // `lhs = rhs` name to apply_linter_call (which could then misfire the
            // per-linter "no Raven equivalent" warning on it).
            if has_unquoted_eq(name) {
                *unrecognized_constructs += 1;
                continue;
            }
            let args = &entry[paren_idx + 1..entry.len() - 1];
            apply_linter_call(name, args, linting, warnings, unrecognized_constructs);
            continue;
        }
        if let Some((name, rhs)) = entry.split_once('=') {
            let name = name.trim();
            let rhs = rhs.trim();
            if rhs == "NULL" {
                disable_rule(name, linting, warnings);
                continue;
            }
            *unrecognized_constructs += 1;
            continue;
        }
        // Bare name with no parens and no `= NULL`: not a known shape.
        *unrecognized_constructs += 1;
    }
}

fn strip_linters_with_defaults(body: &str) -> &str {
    let trimmed = body.trim();
    // `linters_with_defaults` is the canonical (lintr >= 3.0) name; `with_defaults`
    // is the removed pre-3.0 alias, accepted leniently for older `.lintr` files.
    // Try the canonical name first (it is not a prefix of the alias, so ordering
    // is not load-bearing — but this keeps the common case first).
    for name in ["linters_with_defaults", "with_defaults"] {
        if let Some(inner) = strip_named_call(trimmed, name) {
            return inner.trim();
        }
    }
    trimmed
}

fn apply_linter_call(
    name: &str,
    args: &str,
    linting: &mut serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
    unrecognized_constructs: &mut usize,
) {
    match name {
        "line_length_linter" => {
            if let Some(n) = int_arg_or_note(args, "length", unrecognized_constructs) {
                linting.insert("lineLength".into(), json!(n));
            }
        }
        "object_length_linter" => {
            if let Some(n) = int_arg_or_note(args, "length", unrecognized_constructs) {
                linting.insert("objectLength".into(), json!(n));
            }
        }
        "indentation_linter" => {
            // lintr's first positional formal is `indent`, so accept both the
            // named `indent = N` and the positional `N` form (mirroring
            // line_length_linter / object_length_linter).
            if let Some(n) = int_arg_or_note(args, "indent", unrecognized_constructs) {
                linting.insert("indentationUnit".into(), json!(n));
            }
        }
        "assignment_linter" => {
            // lintr's first formal is `operator`, so accept the named
            // `operator = "="` and the positional `assignment_linter("=")`
            // forms (mirroring the other linters). Raven represents a single
            // preferred operator, so only a quoted scalar maps; a vector of
            // allowed operators (`c("<-", "=")`, which lintr permits) has no
            // single-operator equivalent and is flagged unrepresentable rather
            // than silently mapped to a garbage value.
            if let Some(arg) = resolve_arg(args, "operator") {
                let arg = arg.trim();
                if arg.starts_with('"') || arg.starts_with('\'') {
                    linting.insert(
                        "assignmentOperator".into(),
                        json!(arg.trim_matches(|c| c == '"' || c == '\'')),
                    );
                } else {
                    *unrecognized_constructs += 1;
                }
            }
        }
        "object_name_linter" => {
            // lintr's first positional formal is `styles`; accept positional
            // and named, scalar and `c(...)` forms. Known names map to the
            // named-style arrays. Non-empty unknown elements are treated as
            // regexes, a lenient extension over lintr that preserves configs
            // that pass raw regexes through `styles`. Empty elements are still
            // unrepresentable because an empty regex would match every name.
            //
            // `regexes =` is named-only. It must not use `resolve_arg`, whose
            // positional fallback would steal the positional `styles` value.
            let mut accepted_styles = Vec::new();
            let mut accepted_regexes = Vec::new();
            let mut emit_styles = false;
            let mut explicit_empty_styles = false;
            let mut rejected_regex = false;
            if let Some(styles) = parse_object_name_styles(args, unrecognized_constructs) {
                explicit_empty_styles = styles.explicit_empty_vector;
                for style in styles.values {
                    if style.is_empty() {
                        *unrecognized_constructs += 1;
                    } else if crate::linting::ObjectNameStyle::from_config_name(&style).is_some() {
                        accepted_styles.push(style);
                    } else {
                        warn_if_object_name_style_case_hint(&style);
                        rejected_regex |= !accept_object_name_regex(
                            style,
                            &mut accepted_regexes,
                            unrecognized_constructs,
                        );
                    }
                }
                if !accepted_styles.is_empty() || !accepted_regexes.is_empty() {
                    emit_styles = true;
                }
            }
            let mut explicit_empty_regexes = false;
            if let Some(regexes) = parse_object_name_regexes(args, unrecognized_constructs) {
                explicit_empty_regexes = regexes.explicit_empty_vector;
                for regex in regexes.values {
                    // A quoted-empty regex would match every identifier;
                    // count it as unrecognized here — same treatment as an
                    // empty `styles` element — so the batch warning covers it
                    // instead of it being dropped later by a server-side log
                    // the user never sees.
                    if regex.is_empty() {
                        *unrecognized_constructs += 1;
                        rejected_regex = true;
                    } else {
                        rejected_regex |= !accept_object_name_regex(
                            regex,
                            &mut accepted_regexes,
                            unrecognized_constructs,
                        );
                    }
                }
            }
            // An explicit empty `styles` vector emits `[]` so the backend's
            // empty/empty -> disabled and empty+regexes -> regex-only semantics
            // apply, matching raven.toml / VS Code settings. The exception is
            // a regex-only configuration whose patterns were all rejected:
            // leave styles unset so Raven retains its defaults rather than
            // silently disabling the check.
            if emit_styles
                || (explicit_empty_styles && (!accepted_regexes.is_empty() || !rejected_regex))
            {
                emit_object_name_list(linting, "objectNameStyle", &accepted_styles);
            }
            // Like the styles case above, an explicit empty `regexes = c()`
            // emits `[]` so the project layer overrides (clears) any
            // client-layer regexes during config merging, rather than
            // silently leaving them in effect.
            if !accepted_regexes.is_empty() || explicit_empty_regexes {
                emit_object_name_list(linting, "objectNameRegexes", &accepted_regexes);
            }
        }
        "quotes_linter" if args.trim().is_empty() => {
            linting.insert("stringDelimiter".into(), json!("\""));
        }
        "single_quotes_linter" if args.trim().is_empty() => {
            linting.insert("stringDelimiter".into(), json!("'"));
        }
        "quotes_linter" | "single_quotes_linter" => {
            *unrecognized_constructs += 1;
        }
        "trailing_whitespace_linter"
        | "whitespace_linter"
        | "trailing_blank_lines_linter"
        | "infix_spaces_linter"
        | "commented_code_linter"
        | "commas_linter"
        | "T_and_F_symbol_linter"
        | "semicolon_linter"
        | "equals_na_linter"
        | "vector_logic_linter"
        | "function_left_parentheses_linter"
        | "spaces_inside_linter" => {
            // Recognized rule, no parameters to capture; presence in
            // linters_with_defaults() means "leave default severity".
        }
        // Recognized shape, no Raven equivalent.
        _ if name.ends_with("_linter") => {
            warnings.push(format!(
                ".lintr: {} has no Raven equivalent; skipping",
                name
            ));
        }
        _ => {
            *unrecognized_constructs += 1;
        }
    }
}

/// Validate a `.lintr` object-name regex before forwarding it to the backend.
///
/// Invalid Rust regex syntax must use the loader's user-visible warning path;
/// deferring rejection to backend logging can silently disable regex-only
/// configurations after their empty style list has already been applied.
fn accept_object_name_regex(
    source: String,
    accepted: &mut Vec<String>,
    unrecognized_constructs: &mut usize,
) -> bool {
    if crate::linting::CompiledRegex::new(&source).is_err() {
        *unrecognized_constructs += 1;
        return false;
    }
    accepted.push(source);
    true
}

fn disable_rule(
    name: &str,
    linting: &mut serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    let severity_key = match name {
        "line_length_linter" => "lineLengthSeverity",
        "trailing_whitespace_linter" => "trailingWhitespaceSeverity",
        "whitespace_linter" => "noTabSeverity",
        "trailing_blank_lines_linter" => "trailingBlankLinesSeverity",
        "assignment_linter" => "assignmentOperatorSeverity",
        "object_name_linter" => "objectNameSeverity",
        "infix_spaces_linter" => "infixSpacesSeverity",
        "commented_code_linter" => "commentedCodeSeverity",
        "quotes_linter" | "single_quotes_linter" => "quotesSeverity",
        "commas_linter" => "commasSeverity",
        "T_and_F_symbol_linter" => "tAndFSymbolSeverity",
        "semicolon_linter" => "semicolonSeverity",
        "equals_na_linter" => "equalsNaSeverity",
        "object_length_linter" => "objectLengthSeverity",
        "vector_logic_linter" => "vectorLogicSeverity",
        "function_left_parentheses_linter" => "functionLeftParenthesesSeverity",
        "spaces_inside_linter" => "spacesInsideSeverity",
        "indentation_linter" => "indentationSeverity",
        _ => {
            warnings.push(format!(
                ".lintr: cannot disable unknown linter '{}'; skipping",
                name
            ));
            return;
        }
    };
    linting.insert(severity_key.into(), json!("off"));
}

fn apply_exclusions(body: &str, overrides: &mut Vec<Value>, unrecognized_constructs: &mut usize) {
    let body = strip_comments(body);
    let body = body.trim();
    let inner = strip_named_call(body, "list").unwrap_or(body);
    let mut globs = Vec::new();
    for part in split_top_level_commas(inner) {
        let part = part.trim();
        // A top-level `=` *outside* the quotes marks lintr's named/line-range
        // form (`"file" = 1:10`), which Raven has no equivalent for. A `=`
        // *inside* the quotes is just a filename character (e.g. `"a=b.R"`),
        // so test the raw token (via the shared `ScanState`) before stripping
        // quotes — otherwise a legitimate odd filename is silently dropped.
        if has_unquoted_eq(part) {
            *unrecognized_constructs += 1;
            continue;
        }
        let p = part.trim_matches(|c| c == '"' || c == '\'');
        if p.is_empty() {
            continue;
        }
        // Exclusion paths map to globset globs matched against the
        // project-relative file path (see `config_file::overrides`). globset's
        // `<p>/**` matches files *under* `<p>/` but never `<p>` itself. We
        // can't stat the path to learn whether it's a file or a directory, and
        // a dot is not a reliable signal (`foo.R` is a file but `pkg.Rcheck`
        // and `.github` are directories), so:
        //   - trailing `/` → explicit directory → recursive glob only.
        //   - otherwise    → ambiguous file-or-dir: emit BOTH the exact glob
        //                    (matches it as a file) and the recursive glob
        //                    (matches it as a directory's contents). An extra
        //                    `enabled: false` glob only ever disables linting
        //                    on a path that doesn't exist, so emitting both is
        //                    always safe — and it covers extensionless files
        //                    (`NAMESPACE`), dotted files (`foo.R`), and dotted
        //                    directories (`pkg.Rcheck`) uniformly.
        if let Some(dir) = p.strip_suffix('/') {
            globs.push(json!(format!("{}/**", dir.trim_end_matches('/'))));
        } else {
            globs.push(json!(p));
            globs.push(json!(format!("{}/**", p)));
        }
    }
    if !globs.is_empty() {
        overrides.push(json!({
            "files": globs,
            "enabled": false,
        }));
    }
}

/// True if `s` contains a **top-level** `=` outside any string literal or `#`
/// comment — i.e. a named argument / line-range form (`"file" = 1:10`,
/// `regexes = "^x$"`) rather than a `=` that is part of a quoted filename
/// (`"a=b.R"`) or nested inside a call (`c(a = 1)`). Uses the shared
/// [`ScanState`] (depth check included) so it agrees with
/// [`split_top_level_commas`] on what counts as "inside a string" and on bracket
/// depth.
fn has_unquoted_eq(s: &str) -> bool {
    let mut st = ScanState::default();
    for c in s.chars() {
        if st.step(c) && c == '=' && st.depth == 0 {
            return true;
        }
    }
    false
}

/// Shared lexical state for scanning a `.lintr` field value as R-ish text:
/// tracks whether we are inside a string literal (with backslash-escape
/// handling), inside a `#` comment, and the net bracket depth. One state
/// machine so `dcf_fold` (continuation detection), `split_top_level_commas`,
/// and `strip_comments` cannot drift on what counts as "inside a string /
/// comment / bracket".
#[derive(Default)]
struct ScanState {
    /// `Some(quote)` while inside a string literal opened by `quote`.
    in_str: Option<char>,
    /// Inside a string, `true` when the previous char was an unescaped `\`, so
    /// the current char is escaped (and a quote does not close the string).
    escaped: bool,
    /// `true` while inside a `#` comment (until the next newline).
    in_comment: bool,
    /// Net `(`/`[`/`{` minus `)`/`]`/`}`, floored at 0.
    depth: i32,
}

impl ScanState {
    /// Advance over one character `c`. Returns `true` when `c` is a
    /// *structural* character — not inside a string or comment — so callers can
    /// act on `,` / brackets only when this is `true`. Escape state is tracked
    /// internally (no `prev` parameter needed), so a string ending in an
    /// escaped backslash (`"a\\"`) closes correctly.
    fn step(&mut self, c: char) -> bool {
        if self.in_comment {
            if c == '\n' {
                self.in_comment = false;
            }
            return false;
        }
        if let Some(q) = self.in_str {
            if self.escaped {
                self.escaped = false;
            } else if c == '\\' {
                self.escaped = true;
            } else if c == q {
                self.in_str = None;
            }
            return false;
        }
        match c {
            '"' | '\'' => {
                self.in_str = Some(c);
                false
            }
            '#' => {
                self.in_comment = true;
                false
            }
            '(' | '[' | '{' => {
                self.depth += 1;
                true
            }
            ')' | ']' | '}' => {
                self.depth = (self.depth - 1).max(0);
                true
            }
            _ => true,
        }
    }
}

/// True if `s`'s brackets do not nest into matching pairs — an unclosed opener
/// (the common missing-`)` typo), a stray closer with nothing to match
/// (`linters_with_defaults())`), or a closer of the wrong *type*
/// (`linters_with_defaults[...)`). Reuses [`ScanState`] for string/comment-aware
/// scanning (brackets inside a string or `#` comment are not structural) but
/// keeps its own type stack, because [`ScanState::depth`] is a single floored
/// counter that can see neither a net-negative imbalance nor a type mismatch.
/// Used by [`load_str`] to flag a malformed field precisely.
fn brackets_unbalanced(s: &str) -> bool {
    let mut st = ScanState::default();
    let mut stack: Vec<char> = Vec::new();
    for c in s.chars() {
        if !st.step(c) {
            continue;
        }
        match c {
            '(' => stack.push(')'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            // A closer must match the most recent opener; a wrong type or an
            // empty stack (a stray closer) means the brackets are unbalanced.
            ')' | ']' | '}' if stack.pop() != Some(c) => return true,
            _ => {}
        }
    }
    !stack.is_empty()
}

/// Remove `#`-to-end-of-line comments from an R-ish value, preserving any `#`
/// inside a string literal. String state is tracked across the whole input (via
/// the shared [`ScanState`]), so a string spanning multiple newline-joined lines
/// is handled. The terminating newline of each comment is kept so token
/// boundaries created by folding survive.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut st = ScanState::default();
    for c in s.chars() {
        let was_comment = st.in_comment;
        let was_str = st.in_str.is_some();
        st.step(c);
        if was_comment {
            // Drop comment body; keep only the newline that ends it.
            if c == '\n' {
                out.push(c);
            }
        } else if was_str || c != '#' {
            // Inside a string, keep everything (incl. the closing quote);
            // outside, keep everything except a `#` that starts a comment.
            out.push(c);
        }
    }
    out
}

/// Split a token string on commas at depth 0 (ignoring parens / brackets /
/// quotes / `#` comments).
fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut st = ScanState::default();
    let mut start = 0usize;
    for (i, c) in input.char_indices() {
        let structural = st.step(c);
        if structural && c == ',' && st.depth == 0 {
            out.push(&input[start..i]);
            start = i + 1;
        }
    }
    if start <= input.len() {
        out.push(&input[start..]);
    }
    out
}

/// Parse an R unsigned-integer literal: decimal or `0x`/`0X` hexadecimal digits
/// with an optional trailing `L` integer-type suffix (e.g. `120`, `120L`,
/// `0x50`, `0x50L`). R only accepts the uppercase `L` suffix, so we match that
/// exactly. Returns `None` for floats, signed, or anything else.
fn parse_r_uint(s: &str) -> Option<u64> {
    let s = s.trim();
    let body = s.strip_suffix('L').unwrap_or(s);
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        // Reject an empty hex body (`0x`, `0xL`); `from_str_radix` already
        // rejects a leading sign and non-hex digits.
        return u64::from_str_radix(hex, 16).ok();
    }
    body.parse::<u64>().ok()
}

/// Resolve a linter argument by lintr's binding rules: the named form
/// `name = value` if present anywhere in the list, else the first *positional*
/// argument (any named arguments are skipped — they bind by name and leave the
/// positional to fill the formal, e.g. `object_name_linter(regexes = "x",
/// "snake_case")` still fills `styles` from `"snake_case"`). Returns the raw
/// value token (quotes not stripped). Tokenizes the argument list **once**,
/// shared between the named lookup and the positional fallback.
fn resolve_arg<'a>(args: &'a str, name: &str) -> Option<&'a str> {
    let tokens = split_top_level_commas(args);
    if let Some(value) = find_named_arg(&tokens, name) {
        return Some(value);
    }
    tokens
        .into_iter()
        .map(str::trim)
        .find(|tok| !tok.is_empty() && !has_unquoted_eq(tok))
}

/// Find the value of `name = value` among already-split `tokens`, comparing the
/// left-hand side to `name` exactly. The single named-argument matching rule
/// used by [`resolve_arg`], so every linter argument resolves named args the
/// same way. A `split_once('=')` is safe here: a positional `"a=b"` splits to
/// `lhs = "\"a"`, which never equals a bare `name`.
fn find_named_arg<'a>(tokens: &[&'a str], name: &str) -> Option<&'a str> {
    tokens.iter().find_map(|tok| {
        let (lhs, rhs) = tok.split_once('=')?;
        (lhs.trim() == name).then_some(rhs.trim())
    })
}

/// Resolve an unsigned-integer linter argument (named `name = N`, else the first
/// positional integer). See [`resolve_arg`] for the binding rules.
fn parse_int_arg(args: &str, name: &str) -> Option<u64> {
    parse_r_uint(resolve_arg(args, name)?)
}

/// [`parse_int_arg`], but when a parameterized linter is given arguments we
/// can't interpret (e.g. `line_length_linter(width = 120)` — wrong arg name — or
/// `line_length_linter(TRUE)`), count it as an unrecognized construct so the
/// user gets a warning instead of silently falling back to the default limit.
/// The bare `foo_linter()` form (empty args) is a valid "use the default" and
/// stays silent.
fn int_arg_or_note(args: &str, name: &str, unrecognized_constructs: &mut usize) -> Option<u64> {
    let value = parse_int_arg(args, name);
    if value.is_none() && !args.trim().is_empty() {
        *unrecognized_constructs += 1;
    }
    value
}

/// Resolve the `styles` argument of `object_name_linter` into raw string
/// elements. Accepts the named form (`styles = ...`) and, failing that, the
/// first positional argument. Each accepts either a single quoted string or a
/// `c("a", "b")` vector. Returns `None` when there is no styles argument at
/// all (e.g. `object_name_linter()` or `object_name_linter(regexes = ...)`).
fn parse_object_name_styles(
    args: &str,
    unrecognized_constructs: &mut usize,
) -> Option<ObjectNameStringList> {
    // Named `styles = ...`, else the first positional argument (a positional
    // value such as a quoted scalar or `c(...)` vector binds to lintr's first
    // formal `styles`, even when a named arg like `regexes =` precedes it). See
    // [`resolve_arg`].
    let raw = resolve_arg(args, "styles")?.trim();
    Some(parse_object_name_string_list(raw, unrecognized_constructs))
}

/// Resolve the named-only `regexes` argument of `object_name_linter`.
///
/// lintr's `regexes` formal has no positional binding in Raven's loader:
/// positional values belong to `styles`, so this intentionally uses
/// [`find_named_arg`] directly instead of [`resolve_arg`].
fn parse_object_name_regexes(
    args: &str,
    unrecognized_constructs: &mut usize,
) -> Option<ObjectNameStringList> {
    let tokens = split_top_level_commas(args);
    let raw = find_named_arg(&tokens, "regexes")?.trim();
    Some(parse_object_name_string_list(raw, unrecognized_constructs))
}

struct ObjectNameStringList {
    values: Vec<String>,
    explicit_empty_vector: bool,
}

fn parse_object_name_string_list(
    raw: &str,
    unrecognized_constructs: &mut usize,
) -> ObjectNameStringList {
    if let Some(inner) = strip_c_vector(raw) {
        // Drop *syntactically* empty tokens (e.g. a trailing comma) before
        // parsing string literals, so a quoted-empty element `""` survives as
        // a real degenerate value. The caller decides whether that empty string
        // is unrepresentable (`styles`) or a backend-validated regex
        // (`regexes`).
        let mut values = Vec::new();
        let mut had_elements = false;
        for token in split_top_level_commas(inner).into_iter().map(str::trim) {
            if token.is_empty() {
                continue;
            }
            had_elements = true;
            if let Some(value) = parse_r_string_literal(token) {
                values.push(value);
            } else {
                *unrecognized_constructs += 1;
            }
        }
        // Only a genuinely blank `c()` is an explicit empty vector. Inner
        // text that yields no tokens (`c(,)` — an error in real R) is
        // malformed input: warn instead of silently clearing/disabling.
        let inner_blank = inner.trim().is_empty();
        if !had_elements && !inner_blank {
            *unrecognized_constructs += 1;
        }
        ObjectNameStringList {
            values,
            explicit_empty_vector: !had_elements && inner_blank,
        }
    } else if let Some(value) = parse_r_string_literal(raw) {
        ObjectNameStringList {
            values: vec![value],
            explicit_empty_vector: false,
        }
    } else {
        *unrecognized_constructs += 1;
        ObjectNameStringList {
            values: Vec::new(),
            explicit_empty_vector: false,
        }
    }
}

/// Parse a *single, complete* R string literal token and decode its escapes.
///
/// The token must consist of exactly one quoted literal: the opening quote's
/// first unescaped matching quote must be the token's final character.
/// Anything else — a bare identifier, two adjacent literals (`"a" + "b"`), or
/// an unterminated literal whose apparent closing quote is escaped (`"abc\"`)
/// — returns `None` so the caller counts it as an unrecognized construct.
/// R raw string literals (`r"(...)"` and friends) are handled by
/// [`parse_r_raw_string_literal`] first — their content is taken verbatim.
///
/// Decoded escapes: `\\`, `\"`, `\'`, and R's single-char control escapes
/// (`\n`, `\t`, `\r`, `\a`, `\b`, `\f`, `\v`) — the control set matters
/// because the regex engine would otherwise reinterpret sequences like `\b`
/// (R: backspace char) as regex syntax (word boundary). Remaining sequences
/// are preserved verbatim (best-effort): R's `\xNN` / `\uNNNN` escapes reach
/// the regex engine, which interprets them identically, and style names are
/// never written with them in practice. Octal escapes (`\0`–`\7`...) also
/// pass through verbatim — deliberately NOT decoded, because `\0` may start a
/// multi-digit octal like `\056` and a partial decode would corrupt it; the
/// regex engine rejects octal syntax, so such patterns fail closed
/// (warned and dropped) instead of silently changing meaning.
fn parse_r_string_literal(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Some(value) = parse_r_raw_string_literal(raw) {
        return Some(value);
    }
    let quote = raw.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    if raw.len() < 2 || !raw.ends_with(quote) {
        return None;
    }
    let inner = &raw[quote.len_utf8()..raw.len() - quote.len_utf8()];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == quote {
            // Unescaped matching quote before the end of the token: the
            // literal terminated early and the token has trailing content
            // (`"a" + "b"`), so it is not a single string literal.
            return None;
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\u{07}'),
            Some('b') => out.push('\u{08}'),
            Some('f') => out.push('\u{0C}'),
            Some('v') => out.push('\u{0B}'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            // Trailing lone backslash: the "closing" quote is actually
            // escaped (`"abc\"`), i.e. the literal never terminated.
            None => return None,
        }
    }
    Some(out)
}

/// Parse a *single, complete* R raw string literal token (R >= 4.0):
/// `r"(...)"` / `R"(...)"`, with `[`/`{` also allowed as delimiters and an
/// optional run of dashes (`r"---(...)---"`). Content is returned verbatim —
/// no escape processing — which is exactly why raw strings are the idiomatic
/// way to write regexes in R. Returns `None` when the token is not a raw
/// string or the closing delimiter isn't the end of the token.
fn parse_r_raw_string_literal(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix('r').or_else(|| raw.strip_prefix('R'))?;
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[quote.len_utf8()..];
    let dash_count = rest.chars().take_while(|c| *c == '-').count();
    let rest = &rest[dash_count..];
    let open = rest.chars().next()?;
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        _ => return None,
    };
    let body = &rest[open.len_utf8()..];
    let mut terminator = String::with_capacity(dash_count + 2);
    terminator.push(close);
    terminator.extend(std::iter::repeat_n('-', dash_count));
    terminator.push(quote);
    // The closing sequence must terminate the token; a match earlier in the
    // body means the token has trailing content and isn't a single literal.
    let content = body.strip_suffix(terminator.as_str())?;
    if content.contains(terminator.as_str()) {
        return None;
    }
    Some(content.to_string())
}

fn emit_object_name_list(
    linting: &mut serde_json::Map<String, Value>,
    key_prefix: &str,
    values: &[String],
) {
    for suffix in ["Function", "Variable", "Argument"] {
        linting.insert(format!("{key_prefix}{suffix}"), json!(values));
    }
}

fn warn_if_object_name_style_case_hint(value: &str) {
    if let Some(known) = object_name_style_hint(value) {
        log::warn!(
            ".lintr: object_name_linter style '{value}' is not a recognized Raven style name; did you mean '{known}'? Treating it as a regex."
        );
    }
}

fn object_name_style_hint(value: &str) -> Option<&'static str> {
    let key = object_name_style_hint_key(value);
    crate::linting::ObjectNameStyle::config_names()
        .find(|known| object_name_style_hint_key(known) == key)
}

fn object_name_style_hint_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Strip a `name(...)` call wrapper, tolerating whitespace between `name` and
/// `(` (valid R: `linters_with_defaults (x)`). Returns the argument text between
/// the opening `(` and the `)` that *matches* it, or `None` if `s` is not such a
/// call. The required `(` immediately after the (whitespace-trimmed) name
/// prevents a false match on a longer identifier: `strip_named_call("listings(x)",
/// "list")` strips the `list` prefix to `"ings(x)"`, whose next non-space char is
/// not `(`, so it returns `None`.
///
/// Returning the text up to the *matching* `)` (string/comment-aware) — rather
/// than the last `)` — means trailing content is ignored, which is what makes
/// best-effort parsing of a typo'd wrapper work: a stray extra `)`
/// (`linters_with_defaults(line_length_linter(120)))`) still yields the real,
/// balanced argument list, and a value like `f(a) + g(b)` unwraps to `a` (the
/// first call's args) rather than the garbage `a) + g(b`. If the opening `(`
/// never closes, the wrapper genuinely can't be unwrapped and we return `None`.
fn strip_named_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let after = s.trim().strip_prefix(name)?.trim_start();
    let inner = after.strip_prefix('(')?;
    let mut st = ScanState::default();
    let mut depth = 1i32; // the opening `(` we just consumed
    for (i, c) in inner.char_indices() {
        if !st.step(c) {
            continue;
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&inner[..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Strip a `c(...)` vector wrapper, tolerating optional whitespace between the
/// `c` and the `(` so valid R like `c ("snake_case")` parses identically to
/// `c("snake_case")`. Returns the inner argument text, or `None` if `s` is not
/// a `c(...)` call.
fn strip_c_vector(s: &str) -> Option<&str> {
    strip_named_call(s, "c")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_unbalanced_respects_strings_and_comments() {
        assert!(!brackets_unbalanced("f(a, b)"));
        assert!(brackets_unbalanced("f("));
        assert!(brackets_unbalanced("f(g("));
        // A stray extra close (net-negative) is unbalanced even though the
        // floored ScanState depth would read it as 0.
        assert!(brackets_unbalanced("f())"));
        assert!(brackets_unbalanced("f))"));
        // Brackets inside a string literal are not structural.
        assert!(!brackets_unbalanced("f(\"a (b\")"));
        // Brackets inside a `#` comment are not structural; comment ends at \n.
        assert!(!brackets_unbalanced("f( # )(\n)"));
        // A `#` inside a string is not a comment.
        assert!(!brackets_unbalanced("f(\"# (\")"));
        // A string ending in an escaped backslash closes correctly: this is
        // R `f("a\\")` (one backslash in the string), so it is balanced.
        assert!(!brackets_unbalanced("f(\"a\\\\\")"));
        // An escaped quote does NOT close the string: R `f("a\")` leaves the
        // string (and thus the `(`) open, so the `)` is inside the string and
        // the brackets stay unbalanced.
        assert!(brackets_unbalanced("f(\"a\\\")"));
    }

    #[test]
    fn split_top_level_commas_ignores_commas_in_comments() {
        // The comma in the trailing comment must not create a phantom split.
        let parts = split_top_level_commas("a # x, y\nb");
        assert_eq!(parts, vec!["a # x, y\nb"]);
        // Real top-level comma still splits; nested + quoted commas do not.
        let parts = split_top_level_commas("f(1, 2), \"x,y\", g()");
        assert_eq!(parts, vec!["f(1, 2)", " \"x,y\"", " g()"]);
    }

    #[test]
    fn strip_comments_preserves_multibyte_utf8() {
        // strip_comments must not mangle non-ASCII bytes. Regression: a byte-wise
        // rebuild (`b as char` over `as_bytes()`) split each UTF-8 byte into its
        // own scalar, corrupting "café" (0x63 0x61 0x66 0xC3 0xA9) into "cafÃ©".
        // A comment-free value must round-trip verbatim:
        assert_eq!(strip_comments("\"R/café.R\""), "\"R/café.R\"");
        // The non-ASCII character survives when a trailing comment is removed:
        assert_eq!(strip_comments("\"café\" # nöte"), "\"café\" ");
        // A `#` inside a non-ASCII string is preserved, not treated as a comment:
        assert_eq!(strip_comments("\"caf# é\""), "\"caf# é\"");
    }

    #[test]
    fn non_ascii_is_inert_to_the_scanners() {
        // brackets_unbalanced and split_top_level_commas key only off ASCII
        // structure, so a multi-byte char must neither miscount brackets nor
        // land a comma split mid-character.
        assert!(!brackets_unbalanced("f(\"café\")"));
        assert!(brackets_unbalanced("f(café"));
        let parts = split_top_level_commas("\"café\", \"naïve\"");
        assert_eq!(parts, vec!["\"café\"", " \"naïve\""]);
    }

    #[test]
    fn exclusions_with_non_ascii_path_are_not_mangled() {
        // End-to-end: a non-ASCII exclusion path must survive into the override
        // glob unchanged, so it can actually match the real file on disk.
        let out = load_str("exclusions: list(\"R/café.R\", \"naïve/\")\n");
        let overrides = out.settings["linting"]["overrides"]
            .as_array()
            .expect("overrides array");
        let files = overrides[0]["files"].as_array().expect("files array");
        // The non-ASCII path must survive verbatim into a glob. (A path with
        // no trailing slash also emits a harmless `<p>/**`; the trailing-slash
        // directory emits only the recursive form.)
        assert!(files.iter().any(|v| v == &json!("R/café.R")), "{files:?}");
        assert!(files.iter().any(|v| v == &json!("naïve/**")), "{files:?}");
    }

    #[test]
    fn column_zero_closing_paren_folds_and_applies_all_entries() {
        // The user's exact real-world file: closing ')' at column 0.
        let input = "linters: linters_with_defaults(\n\
            line_length_linter(120),\n\
            trailing_whitespace_linter = NULL\n\
            )\n";
        let out = load_str(input);
        let l = &out.settings["linting"];
        assert_eq!(l["lineLength"], json!(120));
        assert_eq!(l["trailingWhitespaceSeverity"], json!("off"));
        // Nothing was lost: no "unrecognized construct" batch warning.
        assert!(
            !out.warnings
                .iter()
                .any(|w| w.contains("unrecognized construct")),
            "column-0 fold must not drop the field: {:?}",
            out.warnings
        );
    }

    #[test]
    fn column_zero_continuation_emits_portability_note() {
        // The one reversible UX knob: accepting a column-0 continuation surfaces
        // a single informational note about lintr's stricter DCF rule.
        let input = "linters: linters_with_defaults(\n    line_length_linter(120)\n)\n";
        let out = load_str(input);
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("column 0") && w.contains("lintr")),
            "expected a lintr-portability note: {:?}",
            out.warnings
        );
    }

    #[test]
    fn portability_note_counts_every_column_zero_continuation_line() {
        // The note reports the exact number of column-0 continuation lines. Here
        // all three (the two entries and the closing `)`) sit at column 0, so the
        // count must be 3 — guarding the "N continuation line(s)" wording against
        // an off-by-one or a "count once per file" regression.
        let input = "linters: linters_with_defaults(\n\
            line_length_linter(120),\n\
            trailing_whitespace_linter = NULL\n\
            )\n";
        let out = load_str(input);
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("accepted 3 continuation line(s)")),
            "expected the note to count all 3 column-0 lines: {:?}",
            out.warnings
        );
    }

    #[test]
    fn indented_closing_paren_does_not_emit_portability_note() {
        // The valid-lintr form (indented ')') must stay silent.
        let input = "linters: linters_with_defaults(\n    line_length_linter(120)\n    )\n";
        let out = load_str(input);
        assert_eq!(out.settings["linting"]["lineLength"], json!(120));
        assert!(
            !out.warnings.iter().any(|w| w.contains("column 0")),
            "indented continuation must not warn: {:?}",
            out.warnings
        );
    }

    #[test]
    fn trailing_comment_in_multiline_value_does_not_eat_following_entries() {
        // A '#' comment after the first linter must not swallow the rest when
        // folding (folding joins with '\n', matching read.dcf).
        let input = "linters: linters_with_defaults(\n\
            line_length_linter(120), # set the limit\n\
            object_length_linter(40)\n\
            )\n";
        let out = load_str(input);
        let l = &out.settings["linting"];
        assert_eq!(l["lineLength"], json!(120));
        assert_eq!(
            l["objectLength"],
            json!(40),
            "comment must not drop object_length"
        );
    }

    #[test]
    fn multiline_exclusions_with_column_zero_close_folds() {
        let input = "exclusions: list(\n    \"R/legacy.R\",\n    \"tests/\"\n)\n";
        let out = load_str(input);
        let overrides = out.settings["linting"]["overrides"].as_array().unwrap();
        let files = overrides[0]["files"].as_array().unwrap();
        assert!(files.iter().any(|v| v == &json!("R/legacy.R")));
        assert!(files.iter().any(|v| v == &json!("tests/**")));
    }

    #[test]
    fn blank_line_inside_open_brackets_is_insignificant() {
        let input = "linters: linters_with_defaults(\n    line_length_linter(120),\n\n    object_length_linter(40)\n)\n";
        let out = load_str(input);
        let l = &out.settings["linting"];
        assert_eq!(l["lineLength"], json!(120));
        assert_eq!(l["objectLength"], json!(40));
    }

    #[test]
    fn reported_column_zero_file_resolves_to_expected_lint_config() {
        // Exactly the file from the bug report (closing ')' at column 0).
        let input = "linters: linters_with_defaults(\n\
            line_length_linter(120),\n\
            commented_code_linter(),\n\
            object_length_linter(40),\n\
            indentation_linter(4),\n\
            trailing_blank_lines_linter = NULL,\n\
            trailing_whitespace_linter = NULL\n\
            )\n";
        let out = load_str(input);
        let cfg = crate::backend::parse_lint_config(&out.settings, true).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.line_length, 120);
        assert_eq!(cfg.object_length, 40);
        assert_eq!(cfg.indentation_unit, 4);
        assert_eq!(cfg.trailing_blank_lines_severity, None);
        assert_eq!(cfg.trailing_whitespace_severity, None);
        // commented_code stays at its default (recognized, not disabled).
        assert!(cfg.commented_code_severity.is_some());
    }

    #[test]
    fn legacy_with_defaults_alias_is_accepted() {
        // lintr removed `with_defaults` after 3.0; Raven accepts it leniently as
        // an alias for `linters_with_defaults`.
        let out = load_str("linters: with_defaults(line_length_linter(120))\n");
        assert_eq!(out.settings["linting"]["lineLength"], json!(120));
    }

    #[test]
    fn whitespace_before_paren_on_wrappers_is_tolerated() {
        // Valid R: space between the function name and '('.
        let out = load_str("linters: linters_with_defaults (line_length_linter(120))\n");
        assert_eq!(out.settings["linting"]["lineLength"], json!(120));
        assert!(
            out.warnings.is_empty(),
            "space before '(' must not warn: {:?}",
            out.warnings
        );

        let out = load_str("exclusions: list (\"R/legacy.R\")\n");
        let files = out.settings["linting"]["overrides"][0]["files"]
            .as_array()
            .unwrap();
        assert!(files.iter().any(|v| v == &json!("R/legacy.R")));
    }

    #[test]
    fn integer_literal_suffix_maps_positional_and_named() {
        let out = load_str("linters: linters_with_defaults(line_length_linter(120L))\n");
        assert_eq!(out.settings["linting"]["lineLength"], json!(120));

        let out = load_str("linters: linters_with_defaults(object_length_linter(length = 40L))\n");
        assert_eq!(out.settings["linting"]["objectLength"], json!(40));

        let out = load_str("linters: linters_with_defaults(indentation_linter(4L))\n");
        assert_eq!(out.settings["linting"]["indentationUnit"], json!(4));
        assert!(
            out.warnings.is_empty(),
            "L-suffixed integers must not warn: {:?}",
            out.warnings
        );
    }

    #[test]
    fn hex_integer_literal_maps() {
        // R hex integer literals (with or without the L suffix) are valid and
        // lintr accepts them; 0x50 == 80.
        let out = load_str("linters: linters_with_defaults(line_length_linter(0x50L))\n");
        assert_eq!(out.settings["linting"]["lineLength"], json!(80));

        let out = load_str("linters: linters_with_defaults(object_length_linter(0X28))\n");
        assert_eq!(out.settings["linting"]["objectLength"], json!(40));
        assert!(
            out.warnings.is_empty(),
            "hex integers must not warn: {:?}",
            out.warnings
        );

        // A bare/empty hex body must not panic or map.
        let out = load_str("linters: linters_with_defaults(line_length_linter(0xL))\n");
        assert!(out.settings["linting"].get("lineLength").is_none());
    }

    #[test]
    fn line_length_param_maps() {
        let out = load_str("linters: linters_with_defaults(line_length_linter(120))\n");
        assert_eq!(out.settings["linting"]["lineLength"], json!(120));
    }

    #[test]
    fn line_length_named_length_param_maps() {
        let out = load_str("linters: linters_with_defaults(line_length_linter(length = 120))\n");
        assert_eq!(out.settings["linting"]["lineLength"], json!(120));
    }

    #[test]
    fn object_length_named_length_param_maps() {
        let out = load_str("linters: linters_with_defaults(object_length_linter(length = 45))\n");
        assert_eq!(out.settings["linting"]["objectLength"], json!(45));
    }

    #[test]
    fn single_quotes_linter_maps_string_delimiter() {
        let out = load_str("linters: linters_with_defaults(single_quotes_linter())\n");
        assert_eq!(out.settings["linting"]["stringDelimiter"], json!("'"));
    }

    #[test]
    fn quotes_linter_maps_string_delimiter() {
        let out = load_str("linters: linters_with_defaults(quotes_linter())\n");
        assert_eq!(out.settings["linting"]["stringDelimiter"], json!("\""));
    }

    #[test]
    fn parameterized_quotes_linters_are_unsupported_not_misread() {
        let out = load_str("linters: linters_with_defaults(quotes_linter(delimiter = \"'\"))\n");
        assert!(
            out.settings
                .get("linting")
                .and_then(|linting| linting.get("stringDelimiter"))
                .is_none(),
            "unsupported quotes_linter args must not be mapped to double quotes"
        );
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("unrecognized construct")),
            "unsupported quotes_linter args should produce the batch warning"
        );

        let out = load_str("linters: linters_with_defaults(single_quotes_linter(TRUE))\n");
        assert!(
            out.settings
                .get("linting")
                .and_then(|linting| linting.get("stringDelimiter"))
                .is_none(),
            "unsupported single_quotes_linter args must not be mapped to single quotes"
        );
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("unrecognized construct")),
            "unsupported single_quotes_linter args should produce the batch warning"
        );
    }

    #[test]
    fn null_disables_rule() {
        let out = load_str("linters: linters_with_defaults(commented_code_linter = NULL)\n");
        assert_eq!(
            out.settings["linting"]["commentedCodeSeverity"],
            json!("off")
        );
    }

    #[test]
    fn multi_line_dcf_field_is_folded() {
        let input = "linters: linters_with_defaults(\n    line_length_linter(140),\n    semicolon_linter = NULL\n  )\n";
        let out = load_str(input);
        assert_eq!(out.settings["linting"]["lineLength"], json!(140));
        assert_eq!(out.settings["linting"]["semicolonSeverity"], json!("off"));
    }

    #[test]
    fn unknown_linter_warns_once() {
        let out = load_str("linters: linters_with_defaults(cyclocomp_linter())\n");
        assert!(out.warnings.iter().any(|w| w.contains("cyclocomp_linter")));
    }

    #[test]
    fn exclusions_become_disabled_overrides() {
        let out = load_str("exclusions: list(\"R/legacy.R\", \"tests/\")\n");
        let overrides = out.settings["linting"]["overrides"].as_array().unwrap();
        assert_eq!(overrides.len(), 1);
        let entry = &overrides[0];
        assert_eq!(entry["enabled"], json!(false));
        let files = entry["files"].as_array().unwrap();
        assert!(files.iter().any(|v| v == &json!("R/legacy.R")));
        assert!(files.iter().any(|v| v == &json!("tests/**")));
    }

    /// Collect the glob strings of the single `enabled: false` exclusion
    /// override emitted by `apply_exclusions`.
    fn exclusion_globs(out: &LoadedLintr) -> Vec<String> {
        out.settings["linting"]["overrides"][0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn exclusions_extensionless_file_excludes_the_file() {
        // `NAMESPACE` (and `DESCRIPTION`, `Makefile`, `LICENSE`, …) is a real
        // file with no extension. The exclusion must emit an exact glob so
        // linting is disabled on the file itself — globset's `NAMESPACE/**`
        // matches only files *under* a `NAMESPACE/` directory, never the file.
        let out = load_str("exclusions: list(\"NAMESPACE\")\n");
        let files = exclusion_globs(&out);
        assert!(
            files.contains(&"NAMESPACE".to_string()),
            "expected an exact glob matching the file: {files:?}"
        );
        // The entry is ambiguous (could name a directory), so the recursive
        // form is emitted too.
        assert!(
            files.contains(&"NAMESPACE/**".to_string()),
            "expected a recursive glob for the ambiguous dir case: {files:?}"
        );
    }

    #[test]
    fn exclusions_directory_forms_still_recursive() {
        // A trailing slash (`R/`) is an explicit directory; a bare name
        // (`tests`) is ambiguous but commonly a directory — both must still
        // disable linting on their contents.
        let out = load_str("exclusions: list(\"R/\", \"tests\")\n");
        let files = exclusion_globs(&out);
        assert!(files.contains(&"R/**".to_string()), "{files:?}");
        assert!(files.contains(&"tests/**".to_string()), "{files:?}");
        // A trailing-slash directory is unambiguous — no bogus exact glob.
        assert!(
            !files.contains(&"R/".to_string()),
            "trailing-slash directory must not emit an exact glob: {files:?}"
        );
    }

    #[test]
    fn exclusions_file_with_extension_still_excludes_the_file() {
        // A path with an extension is still excluded as a file. We can't stat
        // it, so per the uniform "emit both unless trailing slash" rule the
        // recursive glob is emitted too (harmless: nothing lives under a file).
        let out = load_str("exclusions: list(\"R/foo.R\")\n");
        let files = exclusion_globs(&out);
        assert!(files.contains(&"R/foo.R".to_string()), "{files:?}");
        assert!(files.contains(&"R/foo.R/**".to_string()), "{files:?}");
    }

    #[test]
    fn exclusions_dotted_directory_name_is_recursive() {
        // A directory whose name contains a dot (`pkg.Rcheck` — the R CMD
        // check output dir — `.github`, …) must still exclude its contents.
        // A naive "contains '.' => file" heuristic would emit only the exact
        // glob and silently lint everything inside.
        let out = load_str("exclusions: list(\"pkg.Rcheck\", \".github\")\n");
        let files = exclusion_globs(&out);
        assert!(
            files.contains(&"pkg.Rcheck/**".to_string()),
            "dotted directory must emit a recursive glob: {files:?}"
        );
        assert!(
            files.contains(&".github/**".to_string()),
            "hidden directory must emit a recursive glob: {files:?}"
        );
    }

    #[test]
    fn exclusions_filename_with_equals_is_a_file_not_named_form() {
        // A quoted filename containing `=` (rare) is a positional file entry,
        // not lintr's unsupported `"file" = lines` named form — the `=` lives
        // inside the quotes. It must be excluded as a file, not dropped.
        let out = load_str("exclusions: list(\"a=b.R\")\n");
        let files = exclusion_globs(&out);
        assert!(files.contains(&"a=b.R".to_string()), "{files:?}");
        assert!(
            !out.warnings
                .iter()
                .any(|w| w.contains("unrecognized construct")),
            "a quoted filename with `=` must not be treated as unsupported: {:?}",
            out.warnings
        );
    }

    #[test]
    fn exclusions_named_line_range_form_is_unsupported() {
        // lintr's `list("file.R" = 1:10)` (exclude specific lines) has no
        // Raven equivalent. The top-level `=` *outside* the quotes marks it
        // unsupported — it must not produce a glob, and it warns.
        let out = load_str("exclusions: list(\"R/foo.R\" = 1:10)\n");
        let has_override = out
            .settings
            .get("linting")
            .and_then(|l| l.get("overrides"))
            .is_some();
        assert!(
            !has_override,
            "named line-range form must not produce a glob: {:?}",
            out.settings
        );
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("unrecognized construct")),
            "named line-range form should produce the batch warning: {:?}",
            out.warnings
        );
    }

    #[test]
    fn out_of_grammar_yields_batch_warning() {
        let out = load_str("linters: linters_with_defaults(linters_with_tags(\"default\"))\n");
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("unrecognized construct"))
        );
    }

    // --- Task 2: indentation_linter positional support ---

    #[test]
    fn indentation_positional_param_maps() {
        let out = load_str("linters: linters_with_defaults(indentation_linter(4))\n");
        assert_eq!(out.settings["linting"]["indentationUnit"], json!(4));
        assert!(out.warnings.is_empty(), "positional indent must not warn");
    }

    #[test]
    fn indentation_named_param_still_maps() {
        let out = load_str("linters: linters_with_defaults(indentation_linter(indent = 4))\n");
        assert_eq!(out.settings["linting"]["indentationUnit"], json!(4));
    }

    // --- Task 3: object_name_linter positional/single styles + warn ---

    #[test]
    fn object_name_positional_single_style_maps() {
        let out = load_str("linters: linters_with_defaults(object_name_linter(\"camelCase\"))\n");
        let l = &out.settings["linting"];
        assert_eq!(l["objectNameStyleFunction"], json!(["camelCase"]));
        assert_eq!(l["objectNameStyleVariable"], json!(["camelCase"]));
        assert_eq!(l["objectNameStyleArgument"], json!(["camelCase"]));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_named_single_style_maps() {
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = \"UPPER_CASE\"))\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["objectNameStyleFunction"], json!(["UPPER_CASE"]));
        assert_eq!(l["objectNameStyleVariable"], json!(["UPPER_CASE"]));
        assert_eq!(l["objectNameStyleArgument"], json!(["UPPER_CASE"]));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_single_element_vector_maps_named_and_positional() {
        // Named single-element vector.
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = c(\"dotted.case\")))\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["objectNameStyleFunction"], json!(["dotted.case"]));
        assert_eq!(l["objectNameStyleVariable"], json!(["dotted.case"]));
        assert_eq!(l["objectNameStyleArgument"], json!(["dotted.case"]));
        assert!(out.warnings.is_empty());

        // Positional single-element vector.
        let out =
            load_str("linters: linters_with_defaults(object_name_linter(c(\"lowercase\")))\n");
        let l = &out.settings["linting"];
        assert_eq!(l["objectNameStyleFunction"], json!(["lowercase"]));
        assert_eq!(l["objectNameStyleVariable"], json!(["lowercase"]));
        assert_eq!(l["objectNameStyleArgument"], json!(["lowercase"]));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_c_vector_tolerates_space_before_paren() {
        // Valid R allows whitespace between `c` and `(`.
        let out =
            load_str("linters: linters_with_defaults(object_name_linter(c (\"camelCase\")))\n");
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!(["camelCase"])
        );
        assert!(out.warnings.is_empty());

        // Same tolerance on the named-arg path.
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = c (\"snake_case\")))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!(["snake_case"])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_multi_style_vector_maps_to_arrays() {
        for (body, expected) in [
            (
                "object_name_linter(styles = c(\"dotted.case\", \"snake_case\"))",
                json!(["dotted.case", "snake_case"]),
            ),
            (
                "object_name_linter(c(\"snake_case\", \"camelCase\"))",
                json!(["snake_case", "camelCase"]),
            ),
        ] {
            let out = load_str(&format!("linters: linters_with_defaults({body})\n"));
            assert_eq!(
                out.settings["linting"]["objectNameStyleFunction"], expected,
                "multi-style vector must map all styles ({body})"
            );
            assert!(out.warnings.is_empty(), "must not warn ({body})");
        }
    }

    #[test]
    fn object_name_regex_style_maps_to_regexes_not_styles() {
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(\"^[a-z][a-z0-9_]*(\\\\.([a-z][a-z0-9_]*))*$\"))\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["objectNameStyleFunction"], json!([]));
        assert_eq!(
            l["objectNameRegexesFunction"],
            json!(["^[a-z][a-z0-9_]*(\\.([a-z][a-z0-9_]*))*$"])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_no_args_keeps_defaults_silently() {
        let out = load_str("linters: linters_with_defaults(object_name_linter())\n");
        assert!(
            out.settings
                .get("linting")
                .and_then(|l| l.get("objectNameStyleFunction"))
                .is_none(),
            "object_name_linter() with no styles leaves Raven defaults in place"
        );
        assert!(
            out.warnings.is_empty(),
            "the bare no-arg form must not warn"
        );
    }

    /// Helper: did the load surface the batch warning for unrepresentable input?
    fn has_unrecognized_warning(out: &LoadedLintr) -> bool {
        out.warnings
            .iter()
            .any(|w| w.contains("unrecognized construct"))
    }

    /// Helper: did the load map any object-name style?
    fn mapped_object_name_style(out: &LoadedLintr) -> bool {
        out.settings
            .get("linting")
            .and_then(|l| l.get("objectNameStyleFunction"))
            .is_some()
    }

    #[test]
    fn object_name_style_hint_matches_case_and_separator_typos() {
        assert_eq!(object_name_style_hint("SnakeCase"), Some("snake_case"));
        assert_eq!(object_name_style_hint("camelcase"), Some("camelCase"));
        assert_eq!(object_name_style_hint("snake-case"), Some("snake_case"));
        assert_eq!(object_name_style_hint("dotted-case"), Some("dotted.case"));
        assert_eq!(object_name_style_hint("UPPER-CASE"), Some("UPPER_CASE"));
        assert_eq!(object_name_style_hint("totally-not-a-style"), None);
    }

    #[test]
    fn object_name_invalid_positional_regex_warns_without_disabling_check() {
        // A positional raw regex that contains '=' (e.g. a lookahead) must
        // still bind to `styles` rather than being mistaken for a named arg.
        // Raven rejects unsupported lookahead through the visible `.lintr`
        // warning path and leaves its default styles in place.
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(\"^(?=.*[A-Z])[a-z]+$\"))\n",
        );
        let linting = &out.settings["linting"];
        assert!(linting.get("objectNameRegexesFunction").is_none());
        assert!(linting.get("objectNameStyleFunction").is_none());
        assert!(has_unrecognized_warning(&out));

        let cfg = crate::backend::parse_lint_config(&out.settings, true).unwrap();
        assert_eq!(
            cfg.object_name_style_function,
            vec![crate::linting::ObjectNameStyle::SnakeCase]
        );
        assert!(cfg.object_name_regexes_function.is_empty());
    }

    #[test]
    fn object_name_explicit_regex_only_invalid_regex_keeps_default_styles() {
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = c(), regexes = \"(?=bad)\"))\n",
        );
        let linting = &out.settings["linting"];
        assert!(linting.get("objectNameStyleFunction").is_none());
        assert!(linting.get("objectNameRegexesFunction").is_none());
        assert!(has_unrecognized_warning(&out));

        let cfg = crate::backend::parse_lint_config(&out.settings, true).unwrap();
        assert_eq!(
            cfg.object_name_style_function,
            vec![crate::linting::ObjectNameStyle::SnakeCase]
        );
        assert!(cfg.object_name_regexes_function.is_empty());
    }

    #[test]
    fn object_name_positional_regex_with_comma_in_quotes_maps() {
        // The comma lives inside the quoted string, so split_top_level_commas
        // must keep it as one entry.
        let out = load_str("linters: linters_with_defaults(object_name_linter(\"^[a-z,]+$\"))\n");
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!(["^[a-z,]+$"])
        );
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!([])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_regexes_named_arg_maps() {
        let out =
            load_str("linters: linters_with_defaults(object_name_linter(regexes = \"^x$\"))\n");
        assert!(!mapped_object_name_style(&out));
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!(["^x$"])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_regexes_named_vector_maps() {
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(regexes = c(\"^x$\", \"Bar\")))\n",
        );
        assert!(!mapped_object_name_style(&out));
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!(["^x$", "Bar"])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_r_string_escapes_round_trip() {
        let out = load_str(
            r#"linters: linters_with_defaults(object_name_linter(regexes = "^\\.on[A-Z]"))
"#,
        );
        assert!(!mapped_object_name_style(&out));
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!([r"^\.on[A-Z]"])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_malformed_string_tokens_warn_and_skip() {
        // Two adjacent literals joined by an operator are not a single string
        // literal: the interior unescaped quote must reject the token instead
        // of silently absorbing `" + "` into a regex.
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = c(\"a\" + \"b\", \"snake_case\")))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!(["snake_case"])
        );
        assert!(
            out.settings["linting"]
                .get("objectNameRegexesFunction")
                .is_none()
        );
        assert!(has_unrecognized_warning(&out));
    }

    #[test]
    fn object_name_quoted_empty_regex_warns_and_skips() {
        // Same treatment as an empty `styles` element: the degenerate ""
        // regex is counted in the batch warning and never emitted, rather
        // than deferred to a server-side log the user never sees.
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(regexes = c(\"\", \"^x$\")))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!(["^x$"])
        );
        assert!(has_unrecognized_warning(&out));

        let out = load_str("linters: linters_with_defaults(object_name_linter(regexes = \"\"))\n");
        assert!(
            out.settings["linting"]
                .get("objectNameRegexesFunction")
                .is_none()
        );
        assert!(has_unrecognized_warning(&out));
    }

    #[test]
    fn parse_r_string_literal_requires_single_complete_literal() {
        // Adjacent literals joined by an operator: interior unescaped quote.
        assert_eq!(parse_r_string_literal(r#""a" + "b""#), None);
        // Unterminated literal whose apparent closing quote is escaped.
        assert_eq!(parse_r_string_literal(r#""abc\""#), None);
        // Escaped interior quote is still valid content.
        assert_eq!(
            parse_r_string_literal(r#""a\"b""#),
            Some("a\"b".to_string())
        );
        // Opposite quote chars are plain content.
        assert_eq!(
            parse_r_string_literal(r#"'say "hi"'"#),
            Some("say \"hi\"".to_string())
        );
        // Bare identifiers are not string literals.
        assert_eq!(parse_r_string_literal("my_regex_var"), None);
        // R's control escapes decode to their characters; `\b` in particular
        // must become a literal backspace, not reach the regex engine as a
        // word-boundary metacharacter.
        assert_eq!(
            parse_r_string_literal(r#""a\bz""#),
            Some("a\u{08}z".to_string())
        );
        // A doubled backslash still yields a regex-visible escape: R `"\\b"`
        // denotes the two chars `\b`, i.e. a word boundary to the engine.
        assert_eq!(parse_r_string_literal(r#""\\b""#), Some(r"\b".to_string()));
        // The remaining single-char control escapes decode too.
        assert_eq!(
            parse_r_string_literal(r#""\a\f\v""#),
            Some("\u{07}\u{0C}\u{0B}".to_string())
        );
        // Octal escapes pass through verbatim — `\0` must NOT be decoded in
        // isolation, or a multi-digit octal like `\056` (R for `.`) would be
        // corrupted into NUL + "56". Verbatim octal fails regex compilation
        // downstream, which is the fail-closed path.
        assert_eq!(
            parse_r_string_literal(r#""\056""#),
            Some(r"\056".to_string())
        );
    }

    #[test]
    fn parse_r_raw_string_literals() {
        // The idiomatic way to write regexes in R >= 4.0: no escape
        // processing, content taken verbatim.
        assert_eq!(
            parse_r_string_literal(r#"r"(^\.on[A-Z])""#),
            Some(r"^\.on[A-Z]".to_string())
        );
        // Uppercase R prefix, dash-extended delimiters, and the alternative
        // bracket/brace delimiter pairs.
        assert_eq!(
            parse_r_string_literal(r#"R"---(a)b)---""#),
            Some("a)b".to_string())
        );
        assert_eq!(parse_r_string_literal(r#"r"[x]""#), Some("x".to_string()));
        assert_eq!(parse_r_string_literal(r#"r'{x}'"#), Some("x".to_string()));
        // Trailing content after the closing delimiter is not a single
        // literal; neither is an unterminated raw string.
        assert_eq!(parse_r_string_literal(r#"r"(a)" + 1"#), None);
        assert_eq!(parse_r_string_literal(r#"r"(a"#), None);
        // A bare `r` identifier is not a raw string.
        assert_eq!(parse_r_string_literal("r"), None);
    }

    #[test]
    fn object_name_raw_string_regex_maps() {
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(regexes = r\"(^\\.on[A-Z])\"))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!([r"^\.on[A-Z]"])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_unquoted_string_list_elements_warn_and_skip() {
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = c(my_style_var, \"snake_case\"), regexes = c(my_regex_var, \"^x$\")))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!(["snake_case"])
        );
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!(["^x$"])
        );
        assert!(has_unrecognized_warning(&out));

        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = my_style_var, regexes = my_regex_var))\n",
        );
        assert!(!mapped_object_name_style(&out));
        assert!(
            out.settings["linting"]
                .get("objectNameRegexesFunction")
                .is_none()
        );
        assert!(has_unrecognized_warning(&out));
    }

    #[test]
    fn object_name_empty_styles_with_regexes_is_regex_only() {
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = c(), regexes = \"^x$\"))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!([])
        );
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!(["^x$"])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_invalid_empty_style_element_with_regexes_leaves_styles_unset() {
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(c(\"\"), regexes = \"^x$\"))\n",
        );
        assert!(!mapped_object_name_style(&out));
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!(["^x$"])
        );
        assert!(has_unrecognized_warning(&out));
    }

    #[test]
    fn object_name_empty_vector_disables_kind() {
        // An explicit empty styles vector maps to empty style arrays, which
        // the backend resolves (with no regexes) to "check disabled" — the
        // same meaning the equivalent raven.toml / VS Code configuration has.
        let out = load_str("linters: linters_with_defaults(object_name_linter(c()))\n");
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!([])
        );
        assert!(
            out.settings["linting"]
                .get("objectNameRegexesFunction")
                .is_none()
        );
        assert!(out.warnings.is_empty());

        // Maximally explicit form: empty styles AND empty regexes. Both emit
        // `[]` — the explicit empty regexes vector must override (clear) any
        // client-layer regexes during config merging.
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(styles = c(), regexes = c()))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!([])
        );
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!([])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_all_empty_tokens_vector_warns_not_explicit_empty() {
        // `c(,)` is an error in real R, not an empty vector: warn and emit
        // nothing rather than silently clearing/disabling.
        let out = load_str("linters: linters_with_defaults(object_name_linter(regexes = c(,)))\n");
        assert!(
            out.settings["linting"]
                .get("objectNameRegexesFunction")
                .is_none()
        );
        assert!(has_unrecognized_warning(&out));

        let out = load_str("linters: linters_with_defaults(object_name_linter(styles = c(,)))\n");
        assert!(!mapped_object_name_style(&out));
        assert!(has_unrecognized_warning(&out));
    }

    #[test]
    fn object_name_explicit_empty_regexes_emits_empty_array() {
        // `regexes = c()` alone: styles stay unset (Raven defaults apply) but
        // the regexes keys emit `[]` so a project .lintr clears client-layer
        // regexes instead of silently inheriting them.
        let out = load_str("linters: linters_with_defaults(object_name_linter(regexes = c()))\n");
        assert!(!mapped_object_name_style(&out));
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!([])
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn object_name_quoted_empty_element_is_unrepresentable() {
        // A quoted-empty element is a real (degenerate) element: it must not
        // vanish. It warns and never becomes a regex, because an empty regex
        // would match every identifier.
        let out = load_str("linters: linters_with_defaults(object_name_linter(c(\"\")))\n");
        assert!(!mapped_object_name_style(&out));
        assert!(
            out.settings["linting"]
                .get("objectNameRegexesFunction")
                .is_none()
        );
        assert!(has_unrecognized_warning(&out));

        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(c(\"\", \"snake_case\")))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!(["snake_case"])
        );
        assert!(
            out.settings["linting"]
                .get("objectNameRegexesFunction")
                .is_none()
        );
        assert!(has_unrecognized_warning(&out));
    }

    // --- Task 4: the full user example (loader JSON layer) ---

    #[test]
    fn user_example_full_block_maps_each_entry() {
        let input = "linters: linters_with_defaults(\n    \
            line_length_linter(80),\n    \
            commented_code_linter(),\n    \
            object_length_linter(40),\n    \
            indentation_linter(4),\n    \
            object_name_linter(\"^[a-z][a-z0-9_]*(\\\\.([a-z][a-z0-9_]*))*$\"),\n    \
            trailing_blank_lines_linter = NULL,\n    \
            trailing_whitespace_linter = NULL\n    )\n";
        let out = load_str(input);
        let linting = &out.settings["linting"];

        // Positional numeric params.
        assert_eq!(linting["lineLength"], json!(80));
        assert_eq!(linting["objectLength"], json!(40));
        assert_eq!(linting["indentationUnit"], json!(4));

        // Recognized no-arg linter: default severity left intact (no "off").
        assert!(linting.get("commentedCodeSeverity").is_none());

        // Positional regex object-name style: regex-only arrays.
        assert_eq!(linting["objectNameStyleFunction"], json!([]));
        assert_eq!(
            linting["objectNameRegexesFunction"],
            json!(["^[a-z][a-z0-9_]*(\\.([a-z][a-z0-9_]*))*$"])
        );

        // `= NULL` disables.
        assert_eq!(linting["trailingBlankLinesSeverity"], json!("off"));
        assert_eq!(linting["trailingWhitespaceSeverity"], json!("off"));

        assert!(out.warnings.is_empty());
    }

    // --- Task 5: combination & no-override coverage ---

    #[test]
    fn empty_linters_with_defaults_expresses_intent_via_empty_linting_object() {
        // `linters_with_defaults()` sets no individual keys, but it IS a
        // recognized `linters:` directive — so the loader emits an (empty)
        // `linting` object as the "expresses linting intent" marker that
        // distinguishes it from a blank file. See `config_file`.
        let out = load_str("linters: linters_with_defaults()\n");
        let linting = out
            .settings
            .get("linting")
            .expect("a linters: directive must contribute the intent marker");
        assert_eq!(linting, &json!({}), "no overrides means an empty object");
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn blank_lintr_contributes_no_linting_object() {
        // A blank / whitespace-only / unknown-fields-only file expresses no
        // linting intent, so no `linting` object is emitted (the signal a
        // discovered .lintr uses to decide auto-enable).
        assert!(load_str("").settings.get("linting").is_none());
        assert!(load_str("\n  \n").settings.get("linting").is_none());
        let unknown = load_str("encoding: UTF-8\n");
        assert!(unknown.settings.get("linting").is_none());
        assert!(
            unknown.warnings.iter().any(|w| w.contains("unknown field")),
            "an unknown field still warns, but does not express linting intent"
        );
    }

    #[test]
    fn all_named_numeric_params_map() {
        let out = load_str(
            "linters: linters_with_defaults(line_length_linter(length = 100), object_length_linter(length = 50), indentation_linter(indent = 8))\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["lineLength"], json!(100));
        assert_eq!(l["objectLength"], json!(50));
        assert_eq!(l["indentationUnit"], json!(8));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn all_positional_numeric_params_map() {
        let out = load_str(
            "linters: linters_with_defaults(line_length_linter(100), object_length_linter(50), indentation_linter(8))\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["lineLength"], json!(100));
        assert_eq!(l["objectLength"], json!(50));
        assert_eq!(l["indentationUnit"], json!(8));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn multiple_null_disables_map_each() {
        let out = load_str(
            "linters: linters_with_defaults(commented_code_linter = NULL, trailing_blank_lines_linter = NULL, object_name_linter = NULL)\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["commentedCodeSeverity"], json!("off"));
        assert_eq!(l["trailingBlankLinesSeverity"], json!("off"));
        assert_eq!(l["objectNameSeverity"], json!("off"));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn assignment_and_quotes_map() {
        let out = load_str(
            "linters: linters_with_defaults(assignment_linter(operator = \"=\"), single_quotes_linter())\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["assignmentOperator"], json!("="));
        assert_eq!(l["stringDelimiter"], json!("'"));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn mixed_combination_positional_named_null_and_noarg() {
        let out = load_str(
            "linters: linters_with_defaults(line_length_linter(120), object_name_linter(styles = \"snake_case\"), infix_spaces_linter(), semicolon_linter = NULL, indentation_linter(indent = 2))\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["lineLength"], json!(120));
        assert_eq!(l["objectNameStyleFunction"], json!(["snake_case"]));
        assert_eq!(l["indentationUnit"], json!(2));
        assert_eq!(l["semicolonSeverity"], json!("off"));
        // infix_spaces_linter() is recognized no-arg: no severity override.
        assert!(l.get("infixSpacesSeverity").is_none());
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn bare_linters_with_defaults_without_wrapper_call_still_parses() {
        // A bare expression (no `linters_with_defaults(...)` wrapper) is also a
        // documented form; confirm a single linter call still maps.
        let out = load_str("linters: line_length_linter(90)\n");
        assert_eq!(out.settings["linting"]["lineLength"], json!(90));
    }

    // --- Task 6: end-to-end (.lintr JSON -> LintConfig) ---

    #[test]
    fn empty_defaults_enable_all_defaults_when_discovered() {
        // `linters_with_defaults()` with no overrides + a discovered .lintr
        // means "linting on, every rule at its default" — verify the resolved
        // LintConfig.
        let out = load_str("linters: linters_with_defaults()\n");
        let cfg = crate::backend::parse_lint_config(&out.settings, true).unwrap();
        let default = crate::linting::LintConfig::default();
        assert!(cfg.enabled, "a discovered .lintr resolves Auto -> on");
        assert_eq!(cfg.line_length, default.line_length);
        assert_eq!(cfg.object_length, default.object_length);
        assert_eq!(cfg.indentation_unit, default.indentation_unit);
        assert_eq!(cfg.commented_code_severity, default.commented_code_severity);
        assert_eq!(
            cfg.object_name_style_function,
            default.object_name_style_function
        );
    }

    #[test]
    fn user_example_resolves_to_expected_lint_config() {
        let input = "linters: linters_with_defaults(\n    \
            line_length_linter(80),\n    \
            commented_code_linter(),\n    \
            object_length_linter(40),\n    \
            indentation_linter(4),\n    \
            object_name_linter(\"^[a-z][a-z0-9_]*(\\\\.([a-z][a-z0-9_]*))*$\"),\n    \
            trailing_blank_lines_linter = NULL,\n    \
            trailing_whitespace_linter = NULL\n    )\n";
        let out = load_str(input);
        let cfg = crate::backend::parse_lint_config(&out.settings, true).unwrap();

        assert!(cfg.enabled);
        assert_eq!(cfg.line_length, 80);
        assert_eq!(cfg.object_length, 40);
        assert_eq!(cfg.indentation_unit, 4);

        // commented_code stays at its default severity (recognized, not
        // disabled).
        assert_eq!(
            cfg.commented_code_severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::INFORMATION)
        );

        // regex object-name style maps to regex-only mode.
        assert_eq!(
            cfg.object_name_style_function,
            Vec::<crate::linting::ObjectNameStyle>::new()
        );
        assert_eq!(
            cfg.object_name_regexes_function
                .iter()
                .map(|regex| regex.source())
                .collect::<Vec<_>>(),
            vec!["^[a-z][a-z0-9_]*(\\.([a-z][a-z0-9_]*))*$"]
        );

        // `= NULL` rules disabled (severity None).
        assert_eq!(cfg.trailing_blank_lines_severity, None);
        assert_eq!(cfg.trailing_whitespace_severity, None);
    }

    #[test]
    fn valid_object_name_style_resolves_into_lint_config() {
        let out = load_str("linters: linters_with_defaults(object_name_linter(\"camelCase\"))\n");
        let cfg = crate::backend::parse_lint_config(&out.settings, true).unwrap();
        assert_eq!(
            cfg.object_name_style_function,
            vec![crate::linting::ObjectNameStyle::CamelCase]
        );
        assert_eq!(
            cfg.object_name_style_variable,
            vec![crate::linting::ObjectNameStyle::CamelCase]
        );
        assert_eq!(
            cfg.object_name_style_argument,
            vec![crate::linting::ObjectNameStyle::CamelCase]
        );
    }

    #[test]
    fn unbalanced_field_does_not_swallow_following_field() {
        // A missing `)` in `linters:` must NOT fold the following `exclusions:`
        // field into the linters value (silent data loss). The `exclusions:`
        // field is recovered and applied; the malformed `linters:` field is
        // flagged specifically so the typo is pointed at.
        let input = "linters: linters_with_defaults(\n\
            line_length_linter(120)\n\
            exclusions: list(\"a.R\")\n";
        let out = load_str(input);
        // The exclusions field survived the unbalanced linters field.
        let files = out.settings["linting"]["overrides"][0]["files"]
            .as_array()
            .expect("exclusions must survive an unbalanced preceding field");
        assert!(
            files.iter().any(|v| v == &json!("a.R")),
            "exclusions lost: {files:?}"
        );
        // The malformed field is called out specifically (not just a generic
        // "unrecognized construct" batch note).
        assert!(
            out.warnings.iter().any(|w| w.contains("unbalanced")),
            "expected an unbalanced-brackets warning: {:?}",
            out.warnings
        );
    }

    #[test]
    fn column_zero_comment_line_is_not_counted_as_continuation() {
        // A column-0 `#` comment line inside an open bracket is valid DCF (lintr
        // keeps comment lines), so it must NOT inflate the lintr-portability
        // continuation count: here only the linter entry and the closing `)`
        // are real column-0 continuations.
        let input = "linters: linters_with_defaults(\n\
            # a comment\n\
            line_length_linter(120)\n\
            )\n";
        let out = load_str(input);
        assert_eq!(out.settings["linting"]["lineLength"], json!(120));
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("accepted 2 continuation line(s)")),
            "a column-0 comment line must not be counted: {:?}",
            out.warnings
        );
    }

    #[test]
    fn parameterized_linter_with_unparseable_arg_warns() {
        // A recognized parameterized linter given an arg we can't interpret
        // (wrong arg name, or a non-integer) must warn rather than silently
        // applying the default — otherwise the user gets unexpected diagnostics
        // with no clue why.
        for input in [
            "linters: linters_with_defaults(line_length_linter(width = 120))\n",
            "linters: linters_with_defaults(line_length_linter(TRUE))\n",
            "linters: linters_with_defaults(indentation_linter(soft = 2))\n",
        ] {
            let out = load_str(input);
            assert!(
                out.warnings
                    .iter()
                    .any(|w| w.contains("unrecognized construct")),
                "expected a warning for {input:?}: {:?}",
                out.warnings
            );
        }
        // The bare no-arg form is a valid "use the default" and stays silent.
        let out = load_str("linters: linters_with_defaults(line_length_linter())\n");
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }

    #[test]
    fn assignment_linter_positional_operator_maps() {
        // lintr's first formal is `operator`, so a positional
        // `assignment_linter("=")` must map just like the named form, not be
        // silently dropped.
        let out = load_str("linters: linters_with_defaults(assignment_linter(\"=\"))\n");
        assert_eq!(out.settings["linting"]["assignmentOperator"], json!("="));
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }

    #[test]
    fn assignment_linter_operator_vector_is_unrepresentable() {
        // Raven stores a single preferred operator; a vector of allowed
        // operators has no equivalent and must be flagged, not mapped to a
        // garbage value.
        let out = load_str("linters: linters_with_defaults(assignment_linter(c(\"<-\", \"=\")))\n");
        assert!(
            out.settings
                .get("linting")
                .and_then(|l| l.get("assignmentOperator"))
                .is_none(),
            "a vector operator must not be mapped: {:?}",
            out.settings
        );
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("unrecognized construct")),
            "a vector operator must produce the batch warning: {:?}",
            out.warnings
        );
    }

    #[test]
    fn extra_closing_paren_is_flagged_unbalanced() {
        // A stray extra `)` is net-negative; the floored scan depth would read
        // it as balanced, so the precise "unbalanced" diagnostic must use the
        // non-floored imbalance check.
        let out = load_str("linters: linters_with_defaults())\n");
        assert!(
            out.warnings.iter().any(|w| w.contains("unbalanced")),
            "an extra ')' must be flagged as unbalanced: {:?}",
            out.warnings
        );
    }

    #[test]
    fn malformed_field_still_opts_in_and_warns() {
        // A recognized field expresses linting intent even when its value has a
        // typo: it still opts the project in (so a single missing `)` cannot
        // silently turn ALL linting off and leave the user thinking their code
        // is clean), and the imbalance is flagged so they can find the typo.
        let out = load_str("linters: linters_with_defaults(\n");
        assert!(
            out.settings.get("linting").is_some(),
            "a recognized field must opt in even when malformed: {:?}",
            out.settings
        );
        assert!(
            out.warnings.iter().any(|w| w.contains("unbalanced")),
            "the imbalance must be flagged: {:?}",
            out.warnings
        );
    }

    #[test]
    fn well_formed_entries_survive_a_sibling_unbalanced_field() {
        // An unbalanced field is parsed best-effort, not discarded wholesale: a
        // following well-formed field still applies. (The unbalanced field here
        // is recovered as its own field by dcf_fold's header break.)
        let input = "linters: linters_with_defaults(line_length_linter(120)\n\
            exclusions: list(\"a.R\")\n";
        let out = load_str(input);
        let files = out.settings["linting"]["overrides"][0]["files"]
            .as_array()
            .expect("the well-formed exclusions field must still apply");
        assert!(files.iter().any(|v| v == &json!("a.R")), "{files:?}");
    }

    #[test]
    fn mismatched_bracket_type_is_flagged_unbalanced() {
        // A closer of the wrong type (`}` where `)` was meant) nets to zero with
        // a plain counter but is still malformed; the type-aware check catches
        // it instead of silently parsing garbage.
        assert!(brackets_unbalanced("f[a)"));
        assert!(brackets_unbalanced(
            "linters_with_defaults(line_length_linter(120)}"
        ));
        assert!(!brackets_unbalanced("f(a[1])"));
    }

    #[test]
    fn strip_named_call_unwraps_to_the_matching_paren() {
        // Normal calls unwrap to their full argument list.
        assert_eq!(strip_named_call("f(a, b)", "f"), Some("a, b"));
        assert_eq!(strip_named_call("f(g(1), h(2))", "f"), Some("g(1), h(2)"));
        // Content after the matching `)` is ignored, so a typo'd wrapper still
        // yields its real argument list: a stray extra `)` and trailing junk.
        assert_eq!(strip_named_call("f(g(1)))", "f"), Some("g(1)"));
        assert_eq!(strip_named_call("f(a) + g(b)", "f"), Some("a"));
        // An opening `(` that never closes can't be unwrapped.
        assert_eq!(strip_named_call("f(a", "f"), None);
        // A `)` inside a string is not the matching close.
        assert_eq!(strip_named_call("f(\")\")", "f"), Some("\")\""));
    }

    #[test]
    fn stray_extra_paren_field_recovers_entries_best_effort() {
        // A stray extra `)` makes the field bracket-unbalanced (warned), but its
        // real, balanced argument list is still recovered and applied —
        // best-effort parsing, not wholesale discard.
        let out = load_str(
            "linters: linters_with_defaults(line_length_linter(120), object_length_linter(40)))\n",
        );
        let l = &out.settings["linting"];
        assert_eq!(l["lineLength"], json!(120));
        assert_eq!(l["objectLength"], json!(40));
        assert!(
            out.warnings.iter().any(|w| w.contains("unbalanced")),
            "the stray paren is still flagged: {:?}",
            out.warnings
        );
    }

    #[test]
    fn assigned_linter_call_is_unrecognized_not_garbled() {
        // `name = some_linter(args)` is an assignment form, not a linter call;
        // it must count as an unrecognized construct, not emit a per-linter
        // warning with the garbled name `name = some_linter`.
        let out =
            load_str("linters: linters_with_defaults(x = trailing_whitespace_linter(TRUE))\n");
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("unrecognized construct")),
            "{:?}",
            out.warnings
        );
        assert!(
            !out.warnings
                .iter()
                .any(|w| w.contains("no Raven equivalent")),
            "must not emit a garbled per-linter warning: {:?}",
            out.warnings
        );
    }

    #[test]
    fn recognized_header_inside_string_does_not_break_fold() {
        // A column-0 `exclusions:` that falls *inside* an open string literal is
        // string content, not a new field header: dcf_fold must keep folding and
        // not mis-report the field as unbalanced (the broken-off field would).
        let input = "linters: linters_with_defaults(object_name_linter(\"\n\
            exclusions: x\"))\n";
        let out = load_str(input);
        assert!(
            !out.warnings.iter().any(|w| w.contains("unbalanced")),
            "a header inside a string must not break the fold: {:?}",
            out.warnings
        );
    }

    #[test]
    fn trailing_comment_then_new_field_still_breaks() {
        // A `#` comment ending the previous physical line must not leak its
        // "in comment" state into the next line's new-field decision (a comment
        // ends at the newline), so the following `exclusions:` field is still
        // recovered from an unbalanced `linters:` field.
        let input = "linters: linters_with_defaults( # note\n\
            exclusions: list(\"a.R\")\n";
        let out = load_str(input);
        let files = out.settings["linting"]["overrides"][0]["files"]
            .as_array()
            .expect("exclusions recovered despite a trailing comment");
        assert!(files.iter().any(|v| v == &json!("a.R")), "{files:?}");
    }

    #[test]
    fn object_name_positional_style_after_named_arg_maps() {
        // lintr binds `"snake_case"` positionally to the first formal `styles`
        // even when a different named arg (`regexes =`) precedes it. Raven must
        // do the same, not silently drop the style.
        let out = load_str(
            "linters: linters_with_defaults(object_name_linter(regexes = \"^x$\", \"snake_case\"))\n",
        );
        assert_eq!(
            out.settings["linting"]["objectNameStyleFunction"],
            json!(["snake_case"])
        );
        assert_eq!(
            out.settings["linting"]["objectNameRegexesFunction"],
            json!(["^x$"])
        );
        assert!(
            out.warnings.is_empty(),
            "a recognized positional style after a named arg must map cleanly: {:?}",
            out.warnings
        );
    }
}
