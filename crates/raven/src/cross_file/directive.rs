//
// cross_file/directive.rs
//
// Directive parsing for cross-file awareness
//

use regex::Regex;
use std::sync::OnceLock;

use super::types::{
    BackwardDirective, CallSiteSpec, CrossFileMetadata, DeclaredSymbol, ForwardSource,
    LineSuppression, NseDeclaration, NseScope, SuppressionDirective, SuppressionFlavor,
    SuppressionRange,
};

/// Compiled regex patterns for directive parsing
struct DirectivePatterns {
    backward: Regex,
    forward: Regex,
    working_dir: Regex,
    ignore: Regex,
    ignore_next: Regex,
    raven_ignore: Regex,
    raven_ignore_next: Regex,
    raven_ignore_start: Regex,
    raven_ignore_end: Regex,
    raven_ignore_file: Regex,
    declare_var: Regex,
    declare_func: Regex,
    nse: Regex,
    standalone: Regex,
}

/// Convert an optional `[code]` selector capture into a [`LineSuppression`].
///
/// `None` (no brackets) and an empty/blank bracket body both mean a blanket
/// ignore. Otherwise the body is split on commas and each code normalized to
/// its canonical kebab-case spelling (so `[line_length, undefined-variable]`
/// works).
fn parse_suppression_codes(raw: Option<&str>) -> LineSuppression {
    match raw {
        None => LineSuppression::All,
        Some(body) => {
            let codes: Vec<String> = body
                .split(',')
                .map(crate::diagnostic_code::normalize)
                .filter(|c| !c.is_empty())
                .collect();
            if codes.is_empty() {
                LineSuppression::All
            } else {
                LineSuppression::Codes(codes)
            }
        }
    }
}

/// Return the substring of `line` starting at the first `#` that is outside any
/// single- or double-quoted string literal, or `None` if no such `#` exists.
///
/// The same-line ignore regexes (`patterns.ignore`, `patterns.raven_ignore`) are
/// not start-anchored and have no string awareness, so a line that *opens* a
/// multi-line string and ends with the marker text inside that string —
/// `x <- foo + "abc # @lsp-ignore` — would otherwise match and silence a genuine
/// diagnostic. Gating those regexes on this comment region prevents that: a `#`
/// living inside an open string is not a comment start, so no marker is found.
///
/// This is a deliberate local copy of `linting::nolint::first_hash_body`; the two
/// must stay in parity (same single-/double-quote and backslash-escape
/// bookkeeping). It is copied rather than imported to avoid a cross-module
/// dependency between the analyzer and lint tracks for a few lines of byte scan.
///
/// The parity with `nolint::first_hash_body` is enforced by a property test in
/// `cross_file::property_tests`, which reaches this scanner through the
/// `#[cfg(test)]` re-export `comment_region_outside_strings_for_parity_test`
/// below.
fn comment_region_outside_strings(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && (in_single || in_double) {
            i += 2;
            continue;
        }
        if !in_single && b == b'"' {
            in_double = !in_double;
        } else if !in_double && b == b'\'' {
            in_single = !in_single;
        } else if !in_single && !in_double && b == b'#' {
            return Some(&line[i..]);
        }
        i += 1;
    }
    None
}

/// Test-only crate-visible accessor for [`comment_region_outside_strings`] so
/// the cross-parser parity property test in `cross_file::property_tests` can
/// compare it against `nolint::first_hash_body` directly.
#[cfg(test)]
pub(crate) fn comment_region_outside_strings_for_parity_test(line: &str) -> Option<&str> {
    comment_region_outside_strings(line)
}

/// Map the flavor capture (`ignore` | `expect`) into a [`SuppressionFlavor`].
/// Defaults to [`SuppressionFlavor::Ignore`] when absent or unrecognized.
fn parse_flavor(raw: Option<&str>) -> SuppressionFlavor {
    match raw {
        Some("expect") => SuppressionFlavor::Expect,
        _ => SuppressionFlavor::Ignore,
    }
}

/// Insert a line→suppression mapping, merging with any existing entry so a line
/// carrying two directives accumulates both (and `All` stays absorbing).
fn insert_line_suppression(
    map: &mut std::collections::HashMap<u32, LineSuppression>,
    line: u32,
    what: LineSuppression,
) {
    map.entry(line)
        .and_modify(|existing| existing.merge(what.clone()))
        .or_insert(what);
}

/// Extract path from capture groups (double-quoted, single-quoted, or unquoted)
fn capture_path(caps: &regex::Captures, base_group: usize) -> Option<String> {
    // Try double-quoted (base_group)
    if let Some(m) = caps.get(base_group)
        && !m.as_str().is_empty()
    {
        return Some(m.as_str().to_string());
    }
    // Try single-quoted (base_group + 1)
    if let Some(m) = caps.get(base_group + 1)
        && !m.as_str().is_empty()
    {
        return Some(m.as_str().to_string());
    }
    // Try unquoted (base_group + 2)
    if let Some(m) = caps.get(base_group + 2)
        && !m.as_str().is_empty()
    {
        return Some(m.as_str().to_string());
    }
    None
}

/// Extract symbol name from capture groups (double-quoted, single-quoted, or unquoted).
/// Returns None if the symbol name is empty or whitespace-only.
/// Requirements: 1.4, 1.5, 2.4, 2.5, 3.4
fn capture_symbol_name(caps: &regex::Captures, base_group: usize) -> Option<String> {
    let name = capture_path(caps, base_group)?;
    // Skip empty or whitespace-only symbol names
    if name.trim().is_empty() {
        return None;
    }
    Some(name)
}

/// Split a directive parameter list body (the text between `(` and `)`) into
/// trimmed, non-empty formal names. Returns `None` when the body has no usable
/// names (so an empty `()` is treated as whole-call by callers) or when it is
/// malformed (see below).
///
/// Each comma-separated entry is reduced to its formal *name* by dropping any
/// `= default` suffix, so a user can paste a real signature
/// (`# raven: func f(data, x = NULL)`) and still get `["data", "x"]` for
/// positional matching. Defaults containing commas or `)` (e.g. `x = c(1, 2)`)
/// are out of scope — the surrounding regex captures only up to the first `)`,
/// and the comma split would mis-segment them — but simple literal defaults
/// (`= NULL`, `= TRUE`, `= 10`) are handled. Entries that are not syntactic R
/// names (e.g. the `2` left over from a mis-segmented `c(1, 2)` default) are
/// dropped via [`is_formal_name`] rather than recorded as bogus formals.
///
/// A *blank* slot — an empty comma-separated entry (`f(x,,y)`, `f(a,)`) or one
/// whose name part before `=` is empty (`f(= 5)`) — means the directive is
/// malformed: returning a partial list would record a wrong formal order or
/// suppress the wrong argument positions, so the whole list is rejected
/// (`None`). Callers map that to whole-call NSE / "no declared formals" rather
/// than an authoritative partial policy. This is distinct from a non-name
/// *leftover* (the `2)` fragment of a mis-segmented `c(1, 2)` default), which is
/// still dropped without rejecting the list.
fn split_formal_list(body: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for segment in body.split(',') {
        let name = segment
            .split_once('=')
            .map_or(segment, |(name, _)| name)
            .trim();
        if name.is_empty() {
            // Blank slot ⇒ malformed directive; reject the whole list.
            return None;
        }
        if is_formal_name(name) {
            names.push(name.to_string());
        }
        // else: a non-name leftover from a mis-segmented default (`c(1, 2)` →
        // the `2)` fragment) — drop it, as before, without rejecting the list.
    }
    if names.is_empty() { None } else { Some(names) }
}

/// Whether `s` is a syntactic R formal name: the dots `...`, or an identifier
/// starting with a letter or `.` followed by name characters. Used to discard
/// non-name tokens that fall out of a mis-segmented default (a bare number,
/// `c(1`, etc.) instead of treating them as formals. Letters/digits are tested
/// with the Unicode predicates (not the ASCII-only variants) so a legitimate
/// non-ASCII R identifier such as `données` is kept in a UTF-8 locale.
///
/// This is intentionally laxer than [`crate::r_names::is_syntactic_r_name`]: it
/// accepts `...` and a leading-dot-digit (`.2way`) and skips the reserved-word
/// check, because its only job is to drop junk tokens, not to decide quoting.
/// One consequence: a captured/formal name that is NOT a syntactic R name (a
/// leading-dot-digit like `.2way`, or a reserved word) is stored verbatim and is
/// therefore matched only POSITIONALLY — at a call site such an argument's NAME
/// carries backticks (`` `.2way` = x ``), which this bare token never equals, so
/// a NAMED such argument is not suppressed. This is a rare, hand-written-directive
/// edge and errs toward over-flagging (never hiding a real undefined variable);
/// the directive grammar has no way to quote a formal name, so positional
/// matching or the whole-call form is the supported route for such callees.
fn is_formal_name(s: &str) -> bool {
    if s == "..." {
        return true;
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '.' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '.' || c == '_')
}

/// Whether a declared callee name has a well-formed namespace qualifier. A
/// `pkg::name` qualifier must have both halves non-empty and neither half may
/// itself contain a colon — so a malformed `pkg:::name` / `pkg::a::b` (which the
/// quoted forms `"..."`/`'...'` would otherwise accept verbatim) is rejected
/// before it can be stored as a symbol that `declared_name_matches`'s
/// `rsplit_once("::")` would mis-split and mis-pair. A name with NO `::` is
/// always accepted: a lone `:` (e.g. a backtick-quoted R symbol `` `a:b` ``,
/// declared via the quoted form) is not a namespace qualifier and `rsplit_once`
/// leaves it intact, so it cannot mis-pair.
fn is_well_formed_callee_name(name: &str) -> bool {
    match name.split_once("::") {
        Some((pkg, bare)) => {
            !pkg.is_empty() && !bare.is_empty() && !pkg.contains(':') && !bare.contains(':')
        }
        None => true,
    }
}

/// Store a declared callee `name` (the bare part, never the `pkg::` qualifier)
/// in the form it appears at a call site. A non-syntactic R name (spaces,
/// operators, a leading digit, a leading-dot digit like `.2way`, a reserved word
/// like `if`, …) must be backtick-quoted in source, so its tree-sitter
/// `node_text` carries the backticks; a directive captures the name WITHOUT its
/// quoting delimiters, so we re-add backticks for a non-syntactic name to keep
/// the stored key aligned with the callee text matched at use sites
/// (`# raven: nse "my fn"(x)` must govern a `` `my fn`(x) `` call). Syntactic
/// names — including non-ASCII identifiers in a UTF-8 locale — are stored bare,
/// matching their unquoted source spelling.
///
/// The syntactic-name test is [`crate::r_names::is_syntactic_r_name`], the same
/// rule the completion path uses to decide member-insert quoting, NOT the
/// laxer [`is_formal_name`] (which accepts `...` and leading-dot digits because
/// it only filters bogus tokens out of a mis-segmented default list).
pub(crate) fn callee_name_for_match(name: &str) -> String {
    if crate::r_names::is_syntactic_r_name(name) {
        name.to_string()
    } else {
        format!("`{name}`")
    }
}

/// Keyword alternation for forward source directives: `@lsp-source`, `@lsp-run`,
/// `@lsp-include` (and their `# raven: source` / `run` / `include` aliases).
/// This is the inner body of the `(?:@lsp-|raven:\s*)(?:…)` group only — the
/// surrounding regex (prefix, anchoring, separator, capture groups) is supplied
/// by each call site.
///
/// Single source of truth: the directive vocabulary is recognized by two
/// independent regex sets — the full parser in [`patterns`] (capture groups,
/// BOM-stripped input) and the column-aligned, BOM-tolerant prefix matchers in
/// `file_path_intellisense::directive_path_patterns`. Those sets differ
/// deliberately in everything *except* the keyword vocabulary, so only the
/// alternation bodies are shared here to keep the recognized keywords from
/// drifting between them. Both prefixes (`@lsp-` and `# raven:`) are likewise
/// accepted by both sets; `@lsp-` is a permanent alias of the canonical
/// `# raven:` form (#421).
pub(crate) const FORWARD_DIRECTIVE_KEYWORDS: &str = "source|run|include";

/// Keyword alternation for backward provenance directives: `@lsp-sourced-by`,
/// `@lsp-run-by`, `@lsp-included-by`. See [`FORWARD_DIRECTIVE_KEYWORDS`] for why
/// this is factored out.
pub(crate) const BACKWARD_DIRECTIVE_KEYWORDS: &str = "sourced-by|run-by|included-by";

/// Prefix alternation accepted by every structural directive family: the
/// canonical `# raven:` form and the permanent `@lsp-` alias (#421). It is the
/// regex slice that sits between the `#\s*` comment opener and the keyword
/// group, e.g. `^\s*#\s*` + `DIRECTIVE_PREFIX` + `(?:source|run|include)`.
///
/// Shared — for the same no-drift reason as [`FORWARD_DIRECTIVE_KEYWORDS`] —
/// across both directive regex sets (the full parser in [`patterns`] and the
/// path-context matcher in `file_path_intellisense::directive_path_patterns`),
/// so the two seams cannot disagree about which prefixes are accepted. The
/// suppression patterns are deliberately *not* built from this const: they keep
/// separate `@lsp-`/`raven:` regexes because their shapes diverge (e.g.
/// `@lsp-ignore` permits a trailing `:?`, and the block/file forms exist only
/// under `raven:`).
pub(crate) const DIRECTIVE_PREFIX: &str = r"(?:@lsp-|raven:\s*)";

/// Shared callee-name capture for the `# raven: func` and `# raven: nse`
/// directives: double-quoted (group 1), single-quoted (group 2), or an unquoted
/// bare / single-`pkg::name` qualifier (group 3). Held as one constant so both
/// directives accept exactly the same name shape — their doc comments assert this
/// parity, and the formal-order pairing in `declared_name_matches` relies on it.
/// (The `var` directive deliberately uses a laxer `(\S+)` unquoted form and does
/// NOT share this; see its regex below.)
///
/// The unquoted class is Unicode-aware (`\p{Alphabetic}`/`\p{N}`, mirroring the
/// `is_alphabetic()`/`is_alphanumeric()` predicates in [`is_formal_name`] and
/// `crate::r_names::is_syntactic_r_name`), so a valid non-ASCII R identifier such
/// as `données` can be declared unquoted — `# raven: nse données(x)` — without an
/// ASCII-only capture truncating it (issue #460 Codex review). Quoting still works
/// for genuinely non-syntactic names.
const CALLEE_NAME_CAPTURE: &str =
    r#"(?:"([^"]+)"|'([^']+)'|([\p{Alphabetic}\p{N}._]+(?:::[\p{Alphabetic}\p{N}._]+)?))"#;

/// Shared OPTIONAL formal-list capture for the `# raven: func` and `# raven: nse`
/// directives — the parenthesized `(formals…)` that may follow the callee name
/// (capture group 4 holds the body). Held as one constant so both directives
/// accept the same shape.
///
/// The body allows ONE level of nested parentheses (`(?:[^()]|\([^()]*\))*`) so a
/// default that itself contains a `)` — most commonly a comma-bearing default
/// like `x = c(1, 2)` — does not truncate the list at the inner paren and
/// silently drop the formals after it (issue #460 review). `split_formal_list`
/// then still reduces each entry to its formal name and drops the now-balanced
/// default fragment. Deeper nesting (`x = c(f(1), 2)`) remains out of scope: the
/// optional group simply fails to match, so the directive records no formals
/// (named-only) rather than a truncated prefix.
const FORMAL_LIST_CAPTURE: &str = r"(?:\s*\(((?:[^()]|\([^()]*\))*)\))?";

fn patterns() -> &'static DirectivePatterns {
    static PATTERNS: OnceLock<DirectivePatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // Path pattern: "quoted with spaces" or 'single quoted' or unquoted
        // Groups: 1=double-quoted, 2=single-quoted, 3=unquoted
        // All directive regexes are anchored to start of line (^\s*) except
        // @lsp-ignore, which can appear as a trailing comment (e.g., x <- foo # @lsp-ignore).
        // The forward/backward keyword alternations are shared with
        // file_path_intellisense via {FORWARD,BACKWARD}_DIRECTIVE_KEYWORDS,
        // plugged into the middle of each pattern by concatenation.
        // Every structural family accepts either prefix via the shared
        // [`DIRECTIVE_PREFIX`] alternation: `# raven:` is the canonical
        // user-facing form and `@lsp-` a permanent alias (#421). The keyword
        // groups are disjoint from the suppression verbs (`ignore`/`expect`),
        // so `# raven: ignore` still routes to the suppression patterns below
        // and `# raven: source` to `forward`.
        DirectivePatterns {
            backward: Regex::new(
                &[
                    r"^\s*#\s*",
                    DIRECTIVE_PREFIX,
                    r"(?:",
                    BACKWARD_DIRECTIVE_KEYWORDS,
                    r#")\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+line\s*=\s*(\d+|eof|end))?(?:\s+match\s*=\s*["']([^"']+)["'])?"#,
                ]
                .concat(),
            )
            .unwrap(),
            forward: Regex::new(
                &[
                    r"^\s*#\s*",
                    DIRECTIVE_PREFIX,
                    r"(?:",
                    FORWARD_DIRECTIVE_KEYWORDS,
                    r#")(?:\s+:?\s*|:\s*)(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+line\s*=\s*(\d+|eof|end))?"#,
                ]
                .concat(),
            )
            .unwrap(),
            working_dir: Regex::new(
                &[
                    r"^\s*#\s*",
                    DIRECTIVE_PREFIX,
                    r#"(?:working-directory|working-dir|current-directory|current-dir|cd|wd)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))"#,
                ]
                .concat(),
            )
            .unwrap(),
            ignore: Regex::new(
                r"#\s*@lsp-(ignore|expect)(?:\[([^\]]*)\])?\s*:?\s*$"
            ).unwrap(),
            ignore_next: Regex::new(
                r"^\s*#\s*@lsp-(ignore|expect)-next(?:\[([^\]]*)\])?\s*:?\s*$"
            ).unwrap(),
            // `# raven:` is the primary suppression namespace (F2). For the
            // analyzer track it aliases the line / next-line ignore forms
            // (`@lsp-ignore` parity), plus block (`-start`/`-end`) and
            // file-level (`-file`) forms. Each form takes an optional `[code]`
            // selector (comma-separated codes) that targets specific diagnostic
            // codes; absent brackets mean a blanket ignore. Each form also
            // comes in two flavors — `ignore` (silent) and `expect` (asserts a
            // suppression, warns via `unused-suppression` if none occurred) —
            // captured as group 1; the `[code]` body is group 2. The same-line
            // form excludes `-next`/`-start`/`-end`/`-file` by requiring the
            // verb to be followed only by an optional `[code]` + EOL.
            raven_ignore: Regex::new(
                r"#\s*raven:\s*(ignore|expect)(?:\[([^\]]*)\])?\s*$"
            ).unwrap(),
            raven_ignore_next: Regex::new(
                r"^\s*#\s*raven:\s*(ignore|expect)-next(?:\[([^\]]*)\])?\s*$"
            ).unwrap(),
            raven_ignore_start: Regex::new(
                r"^\s*#\s*raven:\s*(ignore|expect)-start(?:\[([^\]]*)\])?\s*$"
            ).unwrap(),
            raven_ignore_end: Regex::new(
                r"^\s*#\s*raven:\s*(?:ignore|expect)-end\s*$"
            ).unwrap(),
            raven_ignore_file: Regex::new(
                r"^\s*#\s*raven:\s*(ignore|expect)-file(?:\[([^\]]*)\])?\s*$"
            ).unwrap(),
            // Declaration directives for variables
            // Synonyms: @lsp-declare-variable, @lsp-declare-var, @lsp-variable, @lsp-var
            // Groups: 1=double-quoted, 2=single-quoted, 3=unquoted
            // Requirements: 1.1, 1.2, 1.3
            declare_var: Regex::new(
                &[
                    r"^\s*#\s*",
                    DIRECTIVE_PREFIX,
                    r#"(?:declare-variable|declare-var|variable|var)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))"#,
                ]
                .concat(),
            ).unwrap(),
            // Declaration directives for functions
            // Synonyms: @lsp-declare-function, @lsp-declare-func, @lsp-function, @lsp-func
            // Groups: 1=double-quoted, 2=single-quoted, 3=unquoted, 4=optional formal list body
            // Requirements: 2.1, 2.2, 2.3
            //
            // The unquoted name accepts a bare `name` or a single `pkg::name`
            // qualifier only — the same shape as the `nse` directive below. A
            // name with characters outside `[A-Za-z0-9._]` (operators,
            // replacement functions, spaces) must use the quoted form. Quoted
            // forms accept any content, so malformed `::` qualifiers (`"pkg:::x"`,
            // `"pkg::a::b"`) and unquoted truncations (`pkg:::x` -> `pkg`,
            // `some-func` -> `some`) are rejected at parse time via
            // `is_well_formed_callee_name` + the adjacent-non-separator check,
            // keeping every stored name well-formed for `declared_name_matches`'s
            // `rsplit_once("::")`. (A lone `:` with no `::`, e.g. a backtick
            // symbol `` `a:b` `` declared as `"a:b"`, is accepted — it cannot
            // mis-pair.)
            declare_func: Regex::new(
                &[
                    r"^\s*#\s*",
                    DIRECTIVE_PREFIX,
                    r"(?:declare-function|declare-func|function|func)\s*:?\s*",
                    CALLEE_NAME_CAPTURE,
                    FORMAL_LIST_CAPTURE,
                ]
                .concat(),
            ).unwrap(),
            // NSE contract directive: `# raven: nse [pkg::]name[(formals…)]`
            // Groups: 1=double-quoted name, 2=single-quoted name, 3=unquoted
            // (bare or `pkg::name`) name, 4=optional formal list body.
            //
            // As with `func`, a callee whose name has characters outside
            // `[A-Za-z0-9._]` (e.g. a name with spaces, called `` `my fn`(x) ``)
            // uses the quoted form. Note the NSE policy is consulted only for
            // ordinary `callee(args)` calls, so it cannot apply to operators
            // (`a %+% b` is not a call) — an `nse` policy for an operator parses
            // but is inert.
            //
            // A separator (whitespace or `:`) is REQUIRED after the `nse`
            // keyword, so a run-together `nseg(col)` is not misread as a
            // declaration for callee `g`. The pattern is end-anchored (only an
            // optional trailing `# comment` may follow): an unclosed `(`, a
            // truncated unquoted name (`nse some-func` leaves trailing `-func`),
            // or any unexpected trailing text fails to match, so a malformed
            // payload is ignored — intentionally stricter than the
            // prefix-tolerant `func`/`var` directives.
            nse: Regex::new(
                &[
                    r"^\s*#\s*",
                    DIRECTIVE_PREFIX,
                    r"nse(?:\s+|\s*:\s*)",
                    CALLEE_NAME_CAPTURE,
                    FORMAL_LIST_CAPTURE,
                    r"\s*(?:#.*)?$",
                ]
                .concat(),
            ).unwrap(),
            // Callee-side "self-contained source" directive (issue #479):
            // `# raven: self-contained` / `# raven: standalone` (and their
            // `@lsp-` aliases). Header-only, file-level, no payload.
            // End-anchored with an optional trailing `# comment`, so stray
            // trailing text fails to match (a malformed payload is ignored
            // rather than half-parsed).
            standalone: Regex::new(
                &[
                    r"^\s*#\s*",
                    DIRECTIVE_PREFIX,
                    r"(?:self-contained|standalone)\s*(?:#.*)?$",
                ]
                .concat(),
            )
            .unwrap(),
        }
    })
}

/// Parse directives from file content.
/// Extracts @lsp-* directives including sourced-by, source, working-directory, and ignore directives.
///
/// Backward directives (`@lsp-sourced-by`, `@lsp-run-by`, `@lsp-included-by`) and working
/// directory directives (`@lsp-cd`, etc.) are **header-only**: they must appear before any code.
/// The header is the region of consecutive blank and comment lines from the start of the file;
/// it ends at the first non-blank, non-comment line.
///
/// Forward, declaration, and ignore directives are recognized anywhere in the file.
pub fn parse_directives(content: &str) -> CrossFileMetadata {
    log::trace!("Starting directive parsing");
    let patterns = patterns();
    let mut meta = CrossFileMetadata::default();

    // BOM-tolerant scan anchor (see `strip_leading_bom_for_scan`). Directive
    // positions are line-based — column is always 0 — so this shifts nothing. #346.
    let content = crate::utf16::strip_leading_bom_for_scan(content);

    // Header tracking: backward and working-dir directives are only recognized
    // in the file header (consecutive blank/comment lines from the start).
    let mut in_header = true;

    // Open `# raven: ignore-start` block, if any: (start_line, what, flavor).
    // Closed by `# raven: ignore-end`; an unterminated block extends to EOF
    // (mirrors the lint track's `# nolint start` behavior).
    let mut open_block: Option<(u32, LineSuppression, SuppressionFlavor)> = None;
    let mut total_lines: u32 = 0;

    for (line_num, line) in content.lines().enumerate() {
        let line_num = line_num as u32;
        total_lines = line_num + 1;

        // Track header boundary before the @lsp- pre-filter so that code lines
        // without directives still end the header region.
        if in_header {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                in_header = false;
            }
        }

        // Fast pre-filter: skip lines that can't contain any directive.
        // All directives require "@lsp-" (or the "raven:" suppression
        // namespace) so a cheap contains() check avoids running the regex
        // battery on the vast majority of lines.
        if !line.contains("@lsp-") && !line.contains("raven:") {
            continue;
        }

        // Header-only: backward directives
        if in_header && let Some(caps) = patterns.backward.captures(line) {
            let path = capture_path(&caps, 1).unwrap_or_default();
            let call_site = if let Some(line_match) = caps.get(4) {
                let line_str = line_match.as_str();
                if line_str == "eof" || line_str == "end" {
                    // Use u32::MAX as EOF sentinel
                    CallSiteSpec::Line(u32::MAX)
                } else {
                    // Convert 1-based user input to 0-based internal
                    let user_line: u32 = line_str.parse().unwrap_or(1);
                    CallSiteSpec::Line(user_line.saturating_sub(1))
                }
            } else if let Some(match_pattern) = caps.get(5) {
                CallSiteSpec::Match(match_pattern.as_str().to_string())
            } else {
                CallSiteSpec::Default
            };
            log::trace!(
                "  Parsed backward directive at line {}: path='{}' call_site={:?}",
                line_num,
                path,
                call_site
            );
            meta.sourced_by.push(BackwardDirective {
                path,
                call_site,
                directive_line: line_num,
            });
            continue;
        }

        // Header-only: working directory directive
        if in_header && let Some(caps) = patterns.working_dir.captures(line) {
            let path = capture_path(&caps, 1).unwrap_or_default();
            log::trace!(
                "  Parsed working directory directive at line {}: path='{}'",
                line_num,
                path
            );
            meta.working_directory = Some(path);
            continue;
        }

        // Header-only: self-contained source directive (issue #479).
        // File-level, no payload. Like the backward/working-dir directives it
        // is only honored in the header so a later `self-contained` /
        // `standalone` token (e.g. inside a string or a later comment) cannot
        // retroactively change the file's cross-file scoping semantics.
        if in_header && patterns.standalone.is_match(line) {
            log::trace!(
                "  Parsed self-contained/standalone directive at line {}",
                line_num
            );
            meta.standalone = true;
            continue;
        }

        // Full-file: forward directive
        if let Some(caps) = patterns.forward.captures(line) {
            let path = capture_path(&caps, 1).unwrap_or_default();
            // Parse line=N parameter from capture group 4 if present
            // Convert from 1-based user input to 0-based internal (N-1)
            // Use directive's own line when no line= parameter
            let (call_site_line, has_explicit_line, is_line_zero) =
                if let Some(line_match) = caps.get(4) {
                    let line_str = line_match.as_str();
                    if line_str == "eof" || line_str == "end" {
                        // Use u32::MAX as EOF sentinel
                        (u32::MAX, true, false)
                    } else {
                        let user_line: u32 = line_str.parse().unwrap_or(0);
                        let is_zero = user_line == 0;
                        // For line=0, treat as line=1 (internal 0) but flag it as invalid
                        let effective_line = if user_line == 0 {
                            0
                        } else {
                            user_line.saturating_sub(1)
                        };
                        (effective_line, true, is_zero)
                    }
                } else {
                    (line_num, false, false)
                };
            log::trace!(
                "  Parsed forward directive at line {}: path='{}' call_site_line={} explicit_line={} user_line_zero={}",
                line_num,
                path,
                call_site_line,
                has_explicit_line,
                is_line_zero
            );
            meta.sources.push(ForwardSource {
                path,
                line: call_site_line,
                column: 0, // Always 0 for directive-based sources
                is_directive: true,
                locality: super::types::SourceLocality::Global,
                chdir: false,
                is_sys_source: false,
                explicit_line: has_explicit_line,
                directive_line: line_num,
                user_line_zero: is_line_zero,
                // `@lsp-source` directives are treated as load-time sources
                // and are never function-scoped. Note: unlike backward
                // (`@lsp-sourced-by`) and working-directory (`@lsp-cd`)
                // directives, forward directives (`@lsp-source`,
                // `@lsp-run`, `@lsp-include`) are NOT strictly header-only
                // in this parser — see the parse_directives docstring above
                // and the "Backward directives and working directory
                // directives are header-only" learning in AGENTS.md. The
                // load-time semantics here come from the parser, not from
                // a header-only restriction: this branch runs in the full-
                // file pass, and `is_directive: true` (combined with this
                // `is_function_scoped: false`) is what tags the resulting
                // ForwardSource as a load-time ordering constraint.
                is_function_scoped: false,
                system_file: None,
                resolved_uri: None,
            });
            continue;
        }

        // Full-file: ignore directives. Each captures an optional flavor
        // (`ignore`|`expect`, group 1) and `[code]` selector (group 2) that
        // narrows what it suppresses. The same-line form is not start-anchored,
        // so it is matched only against the comment region (the substring from
        // the first `#` outside any string literal) — otherwise a marker that
        // lives inside an *open* multi-line string would wrongly suppress.
        if let Some(caps) = comment_region_outside_strings(line)
            .and_then(|comment| patterns.ignore.captures(comment))
        {
            log::trace!("  Parsed @lsp-ignore/expect directive at line {}", line_num);
            let flavor = parse_flavor(caps.get(1).map(|m| m.as_str()));
            let what = parse_suppression_codes(caps.get(2).map(|m| m.as_str()));
            insert_line_suppression(&mut meta.ignored_lines, line_num, what.clone());
            meta.suppression_directives.push(SuppressionDirective {
                directive_line: line_num,
                target_start: line_num,
                target_end: line_num,
                what,
                flavor,
            });
            continue;
        }

        if let Some(caps) = patterns.ignore_next.captures(line) {
            log::trace!(
                "  Parsed @lsp-ignore/expect-next directive at line {}",
                line_num
            );
            let flavor = parse_flavor(caps.get(1).map(|m| m.as_str()));
            let what = parse_suppression_codes(caps.get(2).map(|m| m.as_str()));
            insert_line_suppression(&mut meta.ignored_next_lines, line_num + 1, what.clone());
            meta.suppression_directives.push(SuppressionDirective {
                directive_line: line_num,
                target_start: line_num + 1,
                target_end: line_num + 1,
                what,
                flavor,
            });
            continue;
        }

        // Full-file: `# raven:` primary-namespace ignore aliases (F2). The
        // block (`-start`/`-end`) and file (`-file`) forms are checked first
        // since `raven_ignore` (the same-line form) does not match them.
        if let Some(caps) = patterns.raven_ignore_start.captures(line) {
            log::trace!(
                "  Parsed `# raven: ignore/expect-start` at line {}",
                line_num
            );
            // A nested start is ignored until the current block closes (mirrors
            // the lint track, which does not nest).
            if open_block.is_none() {
                let flavor = parse_flavor(caps.get(1).map(|m| m.as_str()));
                let what = parse_suppression_codes(caps.get(2).map(|m| m.as_str()));
                open_block = Some((line_num, what, flavor));
            }
            continue;
        }

        if patterns.raven_ignore_end.is_match(line) {
            log::trace!("  Parsed `# raven: ignore/expect-end` at line {}", line_num);
            if let Some((start, what, flavor)) = open_block.take() {
                meta.ignored_ranges.push(SuppressionRange {
                    start,
                    end: line_num,
                    what: what.clone(),
                });
                meta.suppression_directives.push(SuppressionDirective {
                    directive_line: start,
                    target_start: start,
                    target_end: line_num,
                    what,
                    flavor,
                });
            }
            continue;
        }

        if let Some(caps) = patterns.raven_ignore_file.captures(line) {
            log::trace!(
                "  Parsed `# raven: ignore/expect-file` at line {}",
                line_num
            );
            let flavor = parse_flavor(caps.get(1).map(|m| m.as_str()));
            let what = parse_suppression_codes(caps.get(2).map(|m| m.as_str()));
            match &mut meta.ignored_file {
                Some(existing) => existing.merge(what.clone()),
                None => meta.ignored_file = Some(what.clone()),
            }
            meta.suppression_directives.push(SuppressionDirective {
                directive_line: line_num,
                target_start: 0,
                target_end: u32::MAX,
                what,
                flavor,
            });
            continue;
        }

        if let Some(caps) = patterns.raven_ignore_next.captures(line) {
            log::trace!(
                "  Parsed `# raven: ignore/expect-next` directive at line {}",
                line_num
            );
            let flavor = parse_flavor(caps.get(1).map(|m| m.as_str()));
            let what = parse_suppression_codes(caps.get(2).map(|m| m.as_str()));
            insert_line_suppression(&mut meta.ignored_next_lines, line_num + 1, what.clone());
            meta.suppression_directives.push(SuppressionDirective {
                directive_line: line_num,
                target_start: line_num + 1,
                target_end: line_num + 1,
                what,
                flavor,
            });
            continue;
        }

        // Same-line form (not start-anchored); see `patterns.ignore` above for
        // why this is gated on the comment region rather than the raw line.
        if let Some(caps) = comment_region_outside_strings(line)
            .and_then(|comment| patterns.raven_ignore.captures(comment))
        {
            log::trace!(
                "  Parsed `# raven: ignore/expect` directive at line {}",
                line_num
            );
            let flavor = parse_flavor(caps.get(1).map(|m| m.as_str()));
            let what = parse_suppression_codes(caps.get(2).map(|m| m.as_str()));
            insert_line_suppression(&mut meta.ignored_lines, line_num, what.clone());
            meta.suppression_directives.push(SuppressionDirective {
                directive_line: line_num,
                target_start: line_num,
                target_end: line_num,
                what,
                flavor,
            });
            continue;
        }

        // Check variable declaration directives (@lsp-var, @lsp-variable, etc.)
        // Requirements: 1.1, 1.2, 1.3, 1.4, 1.5
        if let Some(caps) = patterns.declare_var.captures(line) {
            if let Some(name) = capture_symbol_name(&caps, 1) {
                // Store the name in call-site form (backtick-wrapped if
                // non-syntactic), mirroring `declare_func`/`nse`, so a declared
                // non-syntactic variable (`# raven: var "my fn"`) aligns with the
                // backticked usage text `` `my fn` `` at BOTH the
                // undefined-variable check and go-to-definition. (The
                // undefined-variable path also has an unquote fallback, but
                // go-to-definition compares the stored name exactly, so without
                // the wrap it would miss a non-syntactic declared variable.)
                // Variables are not package-qualified, so there is no `::` split.
                let name = callee_name_for_match(&name);
                log::trace!(
                    "  Parsed variable declaration directive at line {}: name='{}'",
                    line_num,
                    name
                );
                meta.declared_variables.push(DeclaredSymbol {
                    name,
                    line: line_num,
                    is_function: false,
                    formals: None,
                });
            }
            continue;
        }

        // Check function declaration directives (@lsp-func, @lsp-function, etc.)
        // Requirements: 2.1, 2.2, 2.3, 2.4, 2.5
        if let Some(caps) = patterns.declare_func.captures(line) {
            if let Some(name) = capture_symbol_name(&caps, 1) {
                // Reject malformed names. The quoted forms accept any content,
                // so a malformed `"pkg:::my_func"` is caught by
                // `is_well_formed_callee_name`. For the unquoted form the regex
                // stops at the first byte outside its name class, so a
                // NON-SEPARATOR character immediately after the captured name
                // means the regex truncated a longer (non-syntactic) name:
                // `some-func` -> `some`, `obj$method` -> `obj`, `pkg:::name` ->
                // `pkg`. Storing that truncated prefix would silently declare the
                // WRONG symbol, so drop it — a non-syntactic name must use the
                // quoted form. Only an *adjacent* offending byte counts: a
                // separator (whitespace, the `(` formals opener, or a `#`
                // comment) is ordinary trailing text the prefix-tolerant `func`
                // regex legitimately ignores. Together with
                // `is_well_formed_callee_name` this keeps every stored name
                // well-formed for `declared_name_matches`'s `rsplit_once`.
                let unquoted_truncated = caps.get(3).is_some_and(|m| {
                    line[m.end()..]
                        .chars()
                        .next()
                        .is_some_and(|c| !c.is_whitespace() && c != '(' && c != '#')
                });
                if !is_well_formed_callee_name(&name) || unquoted_truncated {
                    continue;
                }
                // Store the bare name in call-site form (backtick-wrapped if
                // non-syntactic) so it matches the callee text at use sites and
                // pairs with a `# raven: nse` reference written the same way.
                let name = match name.split_once("::") {
                    Some((p, n)) => format!("{p}::{}", callee_name_for_match(n)),
                    None => callee_name_for_match(&name),
                };
                log::trace!(
                    "  Parsed function declaration directive at line {}: name='{}'",
                    line_num,
                    name
                );
                let formals = caps.get(4).and_then(|m| split_formal_list(m.as_str()));
                meta.declared_functions.push(DeclaredSymbol {
                    name,
                    line: line_num,
                    is_function: true,
                    formals,
                });
            }
            continue;
        }

        // `# raven: nse [pkg::]name[(formals…)]` — NSE argument-policy
        // declaration. Position-aware (applies to calls after this line).
        if let Some(caps) = patterns.nse.captures(line) {
            // Name from the double-quoted / single-quoted / unquoted groups
            // (1/2/3), then reject a malformed `::` qualifier the same way the
            // `func` directive does — the quoted forms accept any content, so a
            // `"pkg:::x"` must be screened out before it could mis-pair.
            if let Some(raw) =
                capture_symbol_name(&caps, 1).filter(|n| is_well_formed_callee_name(n))
            {
                let (package, name) = match raw.split_once("::") {
                    // `is_well_formed_callee_name` guarantees both halves are
                    // non-empty and colon-free, so any `::` split is a valid
                    // qualifier. The bare name is stored in call-site form
                    // (backtick-wrapped if non-syntactic) so it matches the
                    // callee text at use sites.
                    Some((p, n)) => (Some(p.to_string()), callee_name_for_match(n)),
                    None => (None, callee_name_for_match(&raw)),
                };
                // No parentheses (`nse f`) and EMPTY parentheses (`nse f()`) both
                // mean whole-call NSE — `f()` lists zero captured formals, read as
                // "same as no parens" rather than a silent no-op; both are
                // deliberate, well-formed forms. A non-empty list is per-formal.
                //
                // A non-empty but MALFORMED list (a blank slot like `f(x,,y)`, or
                // no syntactic formal names at all) makes `split_formal_list`
                // return `None`: DROP the whole directive rather than broadening
                // it to whole-call. A typo'd capture list must not silently
                // suppress every argument and hide real undefined-variable findings
                // (issue #460 review) — dropping it leaves the callee checked
                // normally so the user notices and corrects the directive.
                let scope = match caps.get(4).map(|b| b.as_str()) {
                    None => Some(NseScope::WholeCall),
                    Some(body) if body.trim().is_empty() => Some(NseScope::WholeCall),
                    Some(body) => split_formal_list(body).map(NseScope::Formals),
                };
                if let Some(scope) = scope {
                    meta.nse_declarations.push(NseDeclaration {
                        name,
                        package,
                        scope,
                        line: line_num,
                    });
                }
            }
            continue;
        }
    }

    // An unterminated `# raven: ignore-start` extends to EOF (mirrors the lint
    // track's `# nolint start` behavior so a missing `-end` doesn't silently
    // lose coverage).
    if let Some((start, what, flavor)) = open_block.take() {
        let end = total_lines.saturating_sub(1);
        meta.ignored_ranges.push(SuppressionRange {
            start,
            end,
            what: what.clone(),
        });
        meta.suppression_directives.push(SuppressionDirective {
            directive_line: start,
            target_start: start,
            target_end: end,
            what,
            flavor,
        });
    }

    log::trace!(
        "Completed directive parsing: {} backward directives, {} forward directives, working_dir={:?}, {} ignored lines, {} declared vars, {} declared funcs",
        meta.sourced_by.len(),
        meta.sources.len(),
        meta.working_directory,
        meta.ignored_lines.len() + meta.ignored_next_lines.len(),
        meta.declared_variables.len(),
        meta.declared_functions.len()
    );

    meta
}

/// Check if a line should have a diagnostic with `code` suppressed.
///
/// Considers, in turn: a file-level ignore, a line-scoped ignore on `line`, a
/// next-line ignore targeting `line`, and any block range covering `line`. A
/// blanket directive (`LineSuppression::All`) covers any code; a code-scoped
/// directive covers only diagnostics whose code is
/// [`suppresses`](crate::diagnostic_code::suppresses)-covered by one of its
/// listed codes (and only when `code` is `Some`).
pub fn is_line_ignored_for_code(
    metadata: &CrossFileMetadata,
    line: u32,
    code: Option<&str>,
) -> bool {
    if let Some(file) = &metadata.ignored_file
        && file.covers(code)
    {
        return true;
    }
    if let Some(s) = metadata.ignored_lines.get(&line)
        && s.covers(code)
    {
        return true;
    }
    if let Some(s) = metadata.ignored_next_lines.get(&line)
        && s.covers(code)
    {
        return true;
    }
    metadata
        .ignored_ranges
        .iter()
        .any(|r| line >= r.start && line <= r.end && r.what.covers(code))
}

/// Check if a line carries a *blanket* ignore (covers any diagnostic code).
///
/// Equivalent to [`is_line_ignored_for_code`] with `code: None`: only
/// `LineSuppression::All` entries match, since a code-scoped directive cannot
/// suppress an unknown code. Retained for callers that have no diagnostic code
/// in hand.
pub fn is_line_ignored(metadata: &CrossFileMetadata, line: u32) -> bool {
    is_line_ignored_for_code(metadata, line, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backward_directive_basic() {
        let content = "# @lsp-sourced-by ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Default);
    }

    #[test]
    fn test_backward_directive_with_colon() {
        let content = "# @lsp-sourced-by: ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_backward_directive_quoted() {
        let content = r#"# @lsp-sourced-by "../main.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_backward_directive_single_quoted() {
        let content = "# @lsp-sourced-by '../main.R'";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_backward_directive_with_line() {
        let content = "# @lsp-sourced-by ../main.R line=15";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Line(14)); // 0-based
    }

    #[test]
    fn test_backward_directive_with_line_eof() {
        let content = "# @lsp-sourced-by ../main.R line=eof";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Line(u32::MAX));
    }

    #[test]
    fn test_backward_directive_with_line_end() {
        let content = "# @lsp-sourced-by ../main.R line=end";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Line(u32::MAX));
    }

    #[test]
    fn test_backward_directive_with_match() {
        let content = r#"# @lsp-sourced-by ../main.R match="source(""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(
            meta.sourced_by[0].call_site,
            CallSiteSpec::Match("source(".to_string())
        );
    }

    #[test]
    fn test_backward_directive_synonyms() {
        let content = "# @lsp-run-by ../main.R\n# @lsp-included-by ../other.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 2);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
        assert_eq!(meta.sourced_by[1].path, "../other.R");
    }

    #[test]
    fn test_forward_directive() {
        let content = "# @lsp-source utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert!(meta.sources[0].is_directive);
    }

    #[test]
    fn test_forward_directive_with_colon_and_quotes() {
        let content = r#"# @lsp-source: "utils/helpers.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils/helpers.R");
    }

    #[test]
    fn test_working_directory_directive() {
        let content = "# @lsp-working-directory /data/scripts";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, Some("/data/scripts".to_string()));
    }

    #[test]
    fn test_working_directory_synonyms() {
        for directive in [
            "@lsp-wd",
            "@lsp-cd",
            "@lsp-current-directory",
            "@lsp-current-dir",
            "@lsp-working-dir",
        ] {
            let content = format!("# {} /data", directive);
            let meta = parse_directives(&content);
            assert_eq!(
                meta.working_directory,
                Some("/data".to_string()),
                "Failed for {}",
                directive
            );
        }
    }

    #[test]
    fn test_ignore_directive() {
        let content = "x <- 1\n# @lsp-ignore\ny <- undefined";
        let meta = parse_directives(content);
        assert!(meta.ignored_lines.contains_key(&1));
    }

    #[test]
    fn test_ignore_next_directive() {
        let content = "# @lsp-ignore-next\ny <- undefined";
        let meta = parse_directives(content);
        assert!(meta.ignored_next_lines.contains_key(&1));
    }

    #[test]
    fn test_is_line_ignored() {
        let content = "# @lsp-ignore\nx <- 1\n# @lsp-ignore-next\ny <- 2";
        let meta = parse_directives(content);
        assert!(is_line_ignored(&meta, 0)); // @lsp-ignore line
        assert!(!is_line_ignored(&meta, 1)); // x <- 1
        assert!(!is_line_ignored(&meta, 2)); // @lsp-ignore-next line
        assert!(is_line_ignored(&meta, 3)); // y <- 2 (next line after ignore-next)
    }

    /// F2: `# raven: ignore` / `ignore-next` alias the analyzer-track ignore
    /// lines exactly like `@lsp-ignore` / `@lsp-ignore-next`.
    #[test]
    fn test_raven_ignore_aliases_analyzer_track() {
        let content =
            "x <- undefined # raven: ignore\n# raven: ignore-next\ny <- undefined2\nz <- ok";
        let meta = parse_directives(content);
        assert!(is_line_ignored(&meta, 0)); // trailing `# raven: ignore`
        assert!(!is_line_ignored(&meta, 1)); // the directive line itself
        assert!(is_line_ignored(&meta, 2)); // targeted by ignore-next
        assert!(!is_line_ignored(&meta, 3));
    }

    /// F2: a `[code]` selector on the analyzer-track alias targets only the
    /// listed code — it suppresses `undefined-variable` but not other codes,
    /// and is *not* a blanket ignore.
    #[test]
    fn test_raven_ignore_with_code_selector() {
        let content = "x <- undefined # raven: ignore[undefined-variable]";
        let meta = parse_directives(content);
        assert!(is_line_ignored_for_code(
            &meta,
            0,
            Some("undefined-variable")
        ));
        assert!(!is_line_ignored_for_code(
            &meta,
            0,
            Some("package-not-installed")
        ));
        // Not a blanket ignore: a code-less query does not match.
        assert!(!is_line_ignored(&meta, 0));
    }

    /// F2: a bare `# raven: ignore` (no `[code]`) is a blanket ignore that
    /// covers any code, including a code-less query.
    #[test]
    fn test_raven_ignore_blanket_covers_any_code() {
        let content = "x <- undefined # raven: ignore";
        let meta = parse_directives(content);
        assert!(is_line_ignored(&meta, 0));
        assert!(is_line_ignored_for_code(
            &meta,
            0,
            Some("undefined-variable")
        ));
        assert!(is_line_ignored_for_code(&meta, 0, Some("anything-at-all")));
    }

    /// F2: block and file forms on the analyzer track.
    #[test]
    fn test_raven_ignore_block_and_file_forms() {
        let block = "# raven: ignore-start\na <- b\nc <- d\n# raven: ignore-end\ne <- f";
        let meta = parse_directives(block);
        // Inclusive of the start/end directive lines, mirroring `# nolint
        // start`/`end`. The directive lines are comments, so this is harmless.
        assert!(is_line_ignored(&meta, 0));
        assert!(is_line_ignored(&meta, 1));
        assert!(is_line_ignored(&meta, 2));
        assert!(is_line_ignored(&meta, 3));
        assert!(!is_line_ignored(&meta, 4));

        let file = "# raven: ignore-file[undefined-variable]\nx <- undefined";
        let meta = parse_directives(file);
        assert!(is_line_ignored_for_code(
            &meta,
            1,
            Some("undefined-variable")
        ));
        assert!(!is_line_ignored_for_code(
            &meta,
            1,
            Some("package-not-installed")
        ));
    }

    /// F2 Step 3: `expect` is recognized as a suppressing directive on the
    /// analyzer track (suppresses identically to `ignore`) and is enumerated
    /// with `flavor = Expect`.
    #[test]
    fn test_raven_expect_suppresses_and_records_flavor() {
        let content = "x <- undefined # raven: expect[undefined-variable]";
        let meta = parse_directives(content);
        // Suppresses just like ignore.
        assert!(is_line_ignored_for_code(
            &meta,
            0,
            Some("undefined-variable")
        ));
        // Enumerated as an Expect directive at line 0.
        assert_eq!(meta.suppression_directives.len(), 1);
        let d = &meta.suppression_directives[0];
        assert_eq!(d.flavor, SuppressionFlavor::Expect);
        assert_eq!(d.directive_line, 0);
        assert_eq!(d.target_start, 0);
        assert_eq!(d.target_end, 0);
    }

    /// F2 Step 3: a plain `ignore` is enumerated with `flavor = Ignore`.
    #[test]
    fn test_raven_ignore_records_ignore_flavor() {
        let content = "x <- undefined # raven: ignore";
        let meta = parse_directives(content);
        assert_eq!(meta.suppression_directives.len(), 1);
        assert_eq!(
            meta.suppression_directives[0].flavor,
            SuppressionFlavor::Ignore
        );
    }

    /// F2 Step 3: `@lsp-expect` / `@lsp-expect-next` aliases.
    #[test]
    fn test_lsp_expect_aliases() {
        let meta = parse_directives("x <- undefined # @lsp-expect[undefined-variable]");
        assert_eq!(meta.suppression_directives.len(), 1);
        assert_eq!(
            meta.suppression_directives[0].flavor,
            SuppressionFlavor::Expect
        );
        assert!(is_line_ignored_for_code(
            &meta,
            0,
            Some("undefined-variable")
        ));

        let meta = parse_directives("# @lsp-expect-next\ny <- undefined");
        assert_eq!(meta.suppression_directives.len(), 1);
        let d = &meta.suppression_directives[0];
        assert_eq!(d.flavor, SuppressionFlavor::Expect);
        assert_eq!(d.directive_line, 0);
        assert_eq!(d.target_start, 1);
        assert_eq!(d.target_end, 1);
        assert!(is_line_ignored(&meta, 1));
    }

    /// F2 Step 3: `expect-start` … `ignore-end` forms a range directive with
    /// the start line as the hint anchor and the whole block as target.
    #[test]
    fn test_raven_expect_block_enumeration() {
        let content = "# raven: expect-start[undefined-variable]\na <- b\nc <- d\n# raven: ignore-end\ne <- f";
        let meta = parse_directives(content);
        let block: Vec<_> = meta
            .suppression_directives
            .iter()
            .filter(|d| d.target_start != d.target_end || d.target_start == 0)
            .collect();
        assert_eq!(block.len(), 1);
        let d = block[0];
        assert_eq!(d.flavor, SuppressionFlavor::Expect);
        assert_eq!(d.directive_line, 0);
        assert_eq!(d.target_start, 0);
        assert_eq!(d.target_end, 3);
    }

    /// F2 Step 3: `expect-file` covers every line (target_end = u32::MAX).
    #[test]
    fn test_raven_expect_file_enumeration() {
        let content = "# raven: expect-file[undefined-variable]\nx <- undefined";
        let meta = parse_directives(content);
        assert_eq!(meta.suppression_directives.len(), 1);
        let d = &meta.suppression_directives[0];
        assert_eq!(d.flavor, SuppressionFlavor::Expect);
        assert_eq!(d.target_start, 0);
        assert_eq!(d.target_end, u32::MAX);
    }

    #[test]
    fn test_multiple_directives() {
        let content = r#"# @lsp-sourced-by ../main.R line=10
# @lsp-working-directory /data
source("utils.R")
# @lsp-source helpers.R
# @lsp-ignore
x <- undefined"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sources.len(), 1); // Only directive, not source() call
        assert_eq!(meta.working_directory, Some("/data".to_string()));
        assert!(meta.ignored_lines.contains_key(&4));
    }

    // Tests for quoted paths with spaces (Requirements 2.1-2.6)
    #[test]
    fn test_backward_directive_double_quoted_with_spaces() {
        let content = r#"# @lsp-sourced-by "path with spaces/main.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "path with spaces/main.R");
    }

    #[test]
    fn test_backward_directive_single_quoted_with_spaces() {
        let content = "# @lsp-sourced-by 'path with spaces/main.R'";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "path with spaces/main.R");
    }

    #[test]
    fn test_backward_directive_with_colon_and_spaces() {
        let content = r#"# @lsp-sourced-by: "my folder/main.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "my folder/main.R");
    }

    #[test]
    fn test_backward_directive_with_spaces_and_line() {
        let content = r#"# @lsp-sourced-by "path with spaces/main.R" line=15"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "path with spaces/main.R");
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Line(14));
    }

    #[test]
    fn test_forward_directive_double_quoted_with_spaces() {
        let content = r#"# @lsp-source "utils folder/helpers.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils folder/helpers.R");
    }

    #[test]
    fn test_forward_directive_single_quoted_with_spaces() {
        let content = "# @lsp-source 'utils folder/helpers.R'";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils folder/helpers.R");
    }

    // Tests for forward directive synonyms (@lsp-run, @lsp-include)
    #[test]
    fn test_forward_directive_lsp_run_synonym() {
        let content = "# @lsp-run utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert!(meta.sources[0].is_directive);
    }

    #[test]
    fn test_forward_directive_lsp_include_synonym() {
        let content = "# @lsp-include utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert!(meta.sources[0].is_directive);
    }

    #[test]
    fn test_forward_directive_synonyms_all() {
        // Test all three synonyms produce identical results
        for directive in ["@lsp-source", "@lsp-run", "@lsp-include"] {
            let content = format!("# {} utils.R", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(meta.sources[0].path, "utils.R", "Failed for {}", directive);
            assert!(meta.sources[0].is_directive, "Failed for {}", directive);
        }
    }

    #[test]
    fn test_forward_directive_synonyms_no_at_prefix_not_recognized() {
        for directive in ["lsp-source", "lsp-run", "lsp-include"] {
            let content = format!("# {} utils.R", directive);
            let meta = parse_directives(&content);
            assert_eq!(
                meta.sources.len(),
                0,
                "Should not recognize {} without @ prefix",
                directive
            );
        }
    }

    #[test]
    fn test_forward_directive_synonyms_with_colon() {
        for directive in ["@lsp-source:", "@lsp-run:", "@lsp-include:"] {
            let content = format!("# {} utils.R", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(meta.sources[0].path, "utils.R", "Failed for {}", directive);
        }
    }

    #[test]
    fn test_forward_directive_synonyms_with_quotes() {
        for directive in ["@lsp-source", "@lsp-run", "@lsp-include"] {
            let content = format!(r#"# {} "path/to/file.R""#, directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(
                meta.sources[0].path, "path/to/file.R",
                "Failed for {}",
                directive
            );
        }
    }

    // Tests for forward directive line=N parameter (regex capture verification)
    #[test]
    fn test_forward_directive_line_param_regex_capture() {
        // Verify the regex correctly captures the line=N parameter
        // The actual parsing of line= is done in task 1.2, but we verify the regex here
        let patterns = patterns();

        // Test with line= parameter
        let line = "# @lsp-source utils.R line=15";
        let caps = patterns.forward.captures(line).expect("Should match");

        // Path should be in group 3 (unquoted)
        assert_eq!(caps.get(3).map(|m| m.as_str()), Some("utils.R"));
        // Line should be in group 4
        assert_eq!(caps.get(4).map(|m| m.as_str()), Some("15"));
    }

    #[test]
    fn test_forward_directive_line_param_regex_capture_all_synonyms() {
        let patterns = patterns();

        for directive in ["@lsp-source", "@lsp-run", "@lsp-include"] {
            let line = format!("# {} utils.R line=42", directive);
            let caps = patterns
                .forward
                .captures(&line)
                .unwrap_or_else(|| panic!("Should match for {}", directive));

            // Path should be in group 3 (unquoted)
            assert_eq!(
                caps.get(3).map(|m| m.as_str()),
                Some("utils.R"),
                "Path failed for {}",
                directive
            );
            // Line should be in group 4
            assert_eq!(
                caps.get(4).map(|m| m.as_str()),
                Some("42"),
                "Line failed for {}",
                directive
            );
        }
    }

    #[test]
    fn test_forward_directive_line_param_regex_capture_with_quotes() {
        let patterns = patterns();

        // Test with double-quoted path and line=
        let line = r#"# @lsp-source "path/to/file.R" line=10"#;
        let caps = patterns.forward.captures(line).expect("Should match");

        // Path should be in group 1 (double-quoted)
        assert_eq!(caps.get(1).map(|m| m.as_str()), Some("path/to/file.R"));
        // Line should be in group 4
        assert_eq!(caps.get(4).map(|m| m.as_str()), Some("10"));
    }

    #[test]
    fn test_forward_directive_line_param_regex_capture_with_colon() {
        let patterns = patterns();

        // Test with colon and line=
        let line = "# @lsp-source: utils.R line=5";
        let caps = patterns.forward.captures(line).expect("Should match");

        // Path should be in group 3 (unquoted)
        assert_eq!(caps.get(3).map(|m| m.as_str()), Some("utils.R"));
        // Line should be in group 4
        assert_eq!(caps.get(4).map(|m| m.as_str()), Some("5"));
    }

    #[test]
    fn test_forward_directive_line_param_regex_capture_without_line() {
        let patterns = patterns();

        // Test without line= parameter (should still match, group 4 should be None)
        let line = "# @lsp-source utils.R";
        let caps = patterns.forward.captures(line).expect("Should match");

        // Path should be in group 3 (unquoted)
        assert_eq!(caps.get(3).map(|m| m.as_str()), Some("utils.R"));
        // Line should be None (not present)
        assert_eq!(caps.get(4), None);
    }

    // Tests for forward directive line=N parameter parsing (Requirements 2.1, 2.2, 2.3, 2.4)
    #[test]
    fn test_forward_directive_line_param_parsing_basic() {
        // Requirement 2.1: Convert from 1-based user input to 0-based internal (N-1)
        let content = "# @lsp-source utils.R line=15";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].line, 14); // 15 - 1 = 14 (0-based)
        assert_eq!(meta.sources[0].column, 0); // Requirement 2.4: column=0 for directives
        assert!(meta.sources[0].is_directive);
    }

    #[test]
    fn test_forward_directive_line_param_parsing_line_1() {
        // Edge case: line=1 should become 0
        let content = "# @lsp-source utils.R line=1";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].line, 0); // 1 - 1 = 0
        assert!(meta.sources[0].explicit_line);
        assert!(!meta.sources[0].user_line_zero);
    }

    #[test]
    fn test_forward_directive_line_param_parsing_line_0() {
        // Edge case: line=0 is invalid (1-based numbering), should be flagged
        let content = "# @lsp-source utils.R line=0";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].line, 0); // Treated as line 1 (internal 0)
        assert!(meta.sources[0].explicit_line);
        assert!(meta.sources[0].user_line_zero); // Flag that line=0 was specified
        assert_eq!(meta.sources[0].directive_line, 0); // Directive is on line 0
    }

    #[test]
    fn test_forward_directive_without_line_param_uses_directive_line() {
        // Requirement 2.2: Use directive's own line when no line= parameter
        let content = "x <- 1\ny <- 2\n# @lsp-source utils.R\nz <- 3";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].line, 2); // Directive is on line 2 (0-based)
        assert_eq!(meta.sources[0].column, 0);
    }

    #[test]
    fn test_forward_directive_with_line_eof() {
        let content = "# @lsp-source utils.R line=eof";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].line, u32::MAX);
        assert!(meta.sources[0].explicit_line);
        assert!(!meta.sources[0].user_line_zero);
    }

    #[test]
    fn test_forward_directive_with_line_end() {
        let content = "# @lsp-source utils.R line=end";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].line, u32::MAX);
        assert!(meta.sources[0].explicit_line);
        assert!(!meta.sources[0].user_line_zero);
    }

    #[test]
    fn test_forward_directive_line_param_all_synonyms() {
        // Verify line= works with all synonyms
        for directive in ["@lsp-source", "@lsp-run", "@lsp-include"] {
            let content = format!("# {} utils.R line=10", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(
                meta.sources[0].line,
                9, // 10 - 1 = 9 (0-based)
                "Line conversion failed for {}",
                directive
            );
            assert_eq!(
                meta.sources[0].column, 0,
                "Column should be 0 for {}",
                directive
            );
        }
    }

    #[test]
    fn test_forward_directive_line_param_with_quotes() {
        // Test line= with quoted path
        let content = r#"# @lsp-source "path/to/file.R" line=20"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "path/to/file.R");
        assert_eq!(meta.sources[0].line, 19); // 20 - 1 = 19
        assert_eq!(meta.sources[0].column, 0);
    }

    #[test]
    fn test_forward_directive_line_param_with_colon() {
        // Test line= with colon separator
        let content = "# @lsp-source: utils.R line=5";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].line, 4); // 5 - 1 = 4
    }

    #[test]
    fn test_forward_directive_multiple_with_different_lines() {
        // Requirement 2.3: Multiple directives create separate ForwardSource entries
        let content = "# @lsp-source a.R line=10\n# @lsp-source b.R line=20\n# @lsp-source c.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 3);

        // First directive: explicit line=10 -> 9
        assert_eq!(meta.sources[0].path, "a.R");
        assert_eq!(meta.sources[0].line, 9);

        // Second directive: explicit line=20 -> 19
        assert_eq!(meta.sources[1].path, "b.R");
        assert_eq!(meta.sources[1].line, 19);

        // Third directive: no line=, uses directive's own line (2)
        assert_eq!(meta.sources[2].path, "c.R");
        assert_eq!(meta.sources[2].line, 2);
    }

    #[test]
    fn test_forward_directive_line_param_large_value() {
        // Test with a large line number
        let content = "# @lsp-source utils.R line=1000";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].line, 999); // 1000 - 1 = 999
    }

    #[test]
    fn test_forward_directive_column_always_zero() {
        // Requirement 2.4: column=0 for all directive-based sources
        let content = "    # @lsp-source utils.R line=5"; // Indented directive
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].column, 0); // Column is always 0, not the indentation
    }

    #[test]
    fn test_working_dir_double_quoted_with_spaces() {
        let content = r#"# @lsp-cd "/data/my project""#;
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, Some("/data/my project".to_string()));
    }

    #[test]
    fn test_working_dir_single_quoted_with_spaces() {
        let content = "# @lsp-wd '/data/my project'";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, Some("/data/my project".to_string()));
    }

    // Tests that directives without '@' prefix are NOT recognized
    #[test]
    fn test_backward_directive_no_at_prefix_not_recognized() {
        let content = "# lsp-sourced-by ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 0);
    }

    #[test]
    fn test_backward_directive_no_at_prefix_with_colon_not_recognized() {
        let content = "# lsp-sourced-by: ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 0);
    }

    #[test]
    fn test_backward_directive_no_at_prefix_synonyms_not_recognized() {
        let content = "# lsp-run-by ../main.R\n# lsp-included-by ../other.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 0);
    }

    #[test]
    fn test_forward_directive_no_at_prefix_not_recognized() {
        let content = "# lsp-source utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 0);
    }

    #[test]
    fn test_working_dir_no_at_prefix_not_recognized() {
        let content = "# lsp-wd /data/scripts";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, None);
    }

    #[test]
    fn test_working_dir_no_at_prefix_synonyms_not_recognized() {
        for directive in [
            "lsp-cd",
            "lsp-working-directory",
            "lsp-working-dir",
            "lsp-current-directory",
            "lsp-current-dir",
        ] {
            let content = format!("# {} /data", directive);
            let meta = parse_directives(&content);
            assert_eq!(
                meta.working_directory, None,
                "Should not recognize {} without @ prefix",
                directive
            );
        }
    }

    #[test]
    fn test_ignore_directive_no_at_prefix_not_recognized() {
        let content = "x <- 1\n# lsp-ignore\ny <- undefined";
        let meta = parse_directives(content);
        assert!(!meta.ignored_lines.contains_key(&1));
    }

    #[test]
    fn test_ignore_next_directive_no_at_prefix_not_recognized() {
        let content = "# lsp-ignore-next\ny <- undefined";
        let meta = parse_directives(content);
        assert!(!meta.ignored_next_lines.contains_key(&1));
    }

    // ============================================================================
    // Tests for start-of-line anchoring
    // Directives (except @lsp-ignore) must appear at the start of a line,
    // not as trailing comments.
    // ============================================================================

    #[test]
    fn test_trailing_comment_backward_not_recognized() {
        let content = r#"x <- 1 # @lsp-sourced-by ../main.R"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 0);
    }

    #[test]
    fn test_trailing_comment_forward_not_recognized() {
        let content = "x <- 1 # @lsp-source utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 0);
    }

    #[test]
    fn test_trailing_comment_working_dir_not_recognized() {
        let content = "x <- 1 # @lsp-cd /data";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, None);
    }

    #[test]
    fn test_trailing_comment_ignore_next_not_recognized() {
        let content = "x <- 1 # @lsp-ignore-next\ny <- undefined";
        let meta = parse_directives(content);
        assert!(!meta.ignored_next_lines.contains_key(&1));
    }

    #[test]
    fn test_trailing_comment_declare_var_not_recognized() {
        let content = "x <- 1 # @lsp-var myvar";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 0);
    }

    #[test]
    fn test_trailing_comment_declare_func_not_recognized() {
        let content = "x <- 1 # @lsp-func myfunc";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 0);
    }

    #[test]
    fn test_trailing_comment_ignore_is_recognized() {
        // @lsp-ignore is the exception: it works as a trailing comment
        // so you can write `x <- foo # @lsp-ignore` to suppress diagnostics on that line
        let content = "x <- foo # @lsp-ignore";
        let meta = parse_directives(content);
        assert!(meta.ignored_lines.contains_key(&0));
    }

    // ============================================================================
    // Tests for comment_region_outside_strings: a marker inside an OPEN
    // multi-line string must not be parsed (the `#` is string content, not a
    // comment start), while a genuine trailing comment after a CLOSED string
    // still is. End-to-end coverage lives in tests/suppression_per_code.rs.
    // ============================================================================

    #[test]
    fn test_lsp_ignore_marker_inside_open_string_is_not_parsed() {
        let content = "x <- foo + \"abc # @lsp-ignore\nstill string\"\ny <- undefined\n";
        let meta = parse_directives(content);
        assert!(!meta.ignored_lines.contains_key(&0));
    }

    #[test]
    fn test_raven_ignore_marker_inside_open_string_is_not_parsed() {
        let content = "x <- foo + \"abc # raven: ignore\nstill string\"\ny <- undefined\n";
        let meta = parse_directives(content);
        assert!(!meta.ignored_lines.contains_key(&0));
    }

    #[test]
    fn test_marker_after_closed_string_is_still_parsed() {
        let content = "x <- foo + \"abc\" # @lsp-ignore\ny <- \"def\" # raven: ignore\n";
        let meta = parse_directives(content);
        assert!(meta.ignored_lines.contains_key(&0));
        assert!(meta.ignored_lines.contains_key(&1));
    }

    #[test]
    fn test_comment_region_outside_strings_helper() {
        // Open string: the `#` is inside the unterminated literal -> None.
        assert_eq!(
            comment_region_outside_strings("x <- foo + \"abc # @lsp-ignore"),
            None
        );
        assert_eq!(
            comment_region_outside_strings("x <- foo + \"abc # raven: ignore"),
            None
        );
        // Closed string: the `#` after it is a real comment start.
        assert_eq!(
            comment_region_outside_strings("x <- foo + \"abc\" # @lsp-ignore"),
            Some("# @lsp-ignore")
        );
        // Escaped quote does not close the string.
        assert_eq!(
            comment_region_outside_strings("x <- \"a\\\" # raven: ignore"),
            None
        );
    }

    #[test]
    fn test_indented_directive_recognized() {
        // Directives with leading whitespace (indented code) should still work
        let content = "    # @lsp-source utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
    }

    // ============================================================================
    // Tests for header-only constraint
    // Backward directives and @lsp-cd must appear in the file header (before
    // any code). Forward, declaration, and ignore directives work anywhere.
    // ============================================================================

    #[test]
    fn test_backward_after_code_not_recognized() {
        let content = "x <- 1\n# @lsp-sourced-by ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 0);
        assert_eq!(
            meta.sources.len(),
            0,
            "backward directive must not be misinterpreted as forward"
        );
    }

    #[test]
    fn test_working_dir_after_code_not_recognized() {
        let content = "x <- 1\n# @lsp-cd /data";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, None);
    }

    #[test]
    fn test_backward_in_header_with_blanks_and_comments() {
        // Header can contain blank lines and comments before the directive
        let content = "\n# some comment\n\n# @lsp-sourced-by ../main.R\nx <- 1";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_working_dir_in_header_recognized() {
        let content = "# @lsp-cd /data\nx <- 1";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, Some("/data".to_string()));
    }

    #[test]
    fn test_forward_after_code_still_recognized() {
        let content = "x <- 1\n# @lsp-source utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
    }

    #[test]
    fn test_declaration_after_code_still_recognized() {
        let content = "x <- 1\n# @lsp-var myvar";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "myvar");
    }

    #[test]
    fn test_ignore_after_code_still_recognized() {
        let content = "x <- 1\n# @lsp-ignore\ny <- undefined";
        let meta = parse_directives(content);
        assert!(meta.ignored_lines.contains_key(&1));
    }

    #[test]
    fn test_backward_and_forward_mixed_header() {
        // Backward in header, forward after code — both recognized appropriately
        let content = "# @lsp-sourced-by ../main.R\nx <- 1\n# @lsp-source helpers.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sources.len(), 1);
    }

    #[test]
    fn test_backward_after_blank_line_before_code_recognized() {
        // Blank lines don't end the header
        let content = "\n\n# @lsp-sourced-by ../main.R\n\nx <- 1";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
    }

    // ============================================================================
    // Tests for declaration directives (@lsp-var, @lsp-func, etc.)
    // Requirements: 1.1-1.6, 2.1-2.6, 3.4
    // ============================================================================

    // Variable declaration directive tests
    #[test]
    fn test_declare_var_basic() {
        let content = "# @lsp-var myvar";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "myvar");
        assert_eq!(meta.declared_variables[0].line, 0);
        assert!(!meta.declared_variables[0].is_function);
    }

    #[test]
    fn test_declare_var_with_colon() {
        let content = "# @lsp-var: myvar";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "myvar");
    }

    #[test]
    fn test_declare_var_double_quoted() {
        let content = r#"# @lsp-var "my.var""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "my.var");
    }

    #[test]
    fn test_declare_var_single_quoted() {
        let content = "# @lsp-var 'my.var'";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "my.var");
    }

    #[test]
    fn declare_var_wraps_nonsyntactic_name() {
        // A non-syntactic variable name is stored backtick-wrapped (call-site
        // form), mirroring `declare_func`/`nse`, so go-to-definition (which
        // compares the stored name exactly to the usage `node_text`) locates a
        // `# raven: var "my fn"` declaration from a `` `my fn` `` usage. A
        // syntactic name is still stored bare.
        let meta = parse_directives("# raven: var \"my fn\"\n");
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "`my fn`");
        let meta = parse_directives("# raven: var \"my.var\"\n");
        assert_eq!(meta.declared_variables[0].name, "my.var");
    }

    #[test]
    fn test_declare_var_all_synonyms() {
        // Test all 4 synonym forms: @lsp-declare-variable, @lsp-declare-var, @lsp-variable, @lsp-var
        for directive in [
            "@lsp-declare-variable",
            "@lsp-declare-var",
            "@lsp-variable",
            "@lsp-var",
        ] {
            let content = format!("# {} myvar", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.declared_variables.len(), 1, "Failed for {}", directive);
            assert_eq!(
                meta.declared_variables[0].name, "myvar",
                "Failed for {}",
                directive
            );
            assert!(
                !meta.declared_variables[0].is_function,
                "Should be variable for {}",
                directive
            );
        }
    }

    #[test]
    fn test_declare_var_line_number_recorded() {
        let content = "x <- 1\ny <- 2\n# @lsp-var myvar\nz <- 3";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].line, 2); // 0-based line number
    }

    #[test]
    fn test_declare_var_multiple() {
        let content = "# @lsp-var var1\n# @lsp-var var2\n# @lsp-var var3";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 3);
        assert_eq!(meta.declared_variables[0].name, "var1");
        assert_eq!(meta.declared_variables[0].line, 0);
        assert_eq!(meta.declared_variables[1].name, "var2");
        assert_eq!(meta.declared_variables[1].line, 1);
        assert_eq!(meta.declared_variables[2].name, "var3");
        assert_eq!(meta.declared_variables[2].line, 2);
    }

    #[test]
    fn test_declare_var_with_special_chars_quoted() {
        // R allows special characters in symbol names when quoted
        let content = r#"# @lsp-var "my.special_var""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "my.special_var");
    }

    #[test]
    fn test_declare_var_no_at_prefix_not_recognized() {
        // Requirement 1.6: Directives without @ prefix should NOT be recognized
        let content = "# lsp-var myvar";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 0);
    }

    // Function declaration directive tests
    #[test]
    fn test_declare_func_basic() {
        let content = "# @lsp-func myfunc";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "myfunc");
        assert_eq!(meta.declared_functions[0].line, 0);
        assert!(meta.declared_functions[0].is_function);
    }

    #[test]
    fn test_declare_func_with_colon() {
        let content = "# @lsp-func: myfunc";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "myfunc");
    }

    #[test]
    fn test_declare_func_double_quoted() {
        let content = r#"# @lsp-func "my.func""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my.func");
    }

    #[test]
    fn test_declare_func_single_quoted() {
        let content = "# @lsp-func 'my.func'";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my.func");
    }

    #[test]
    fn test_declare_func_all_synonyms() {
        // Test all 4 synonym forms: @lsp-declare-function, @lsp-declare-func, @lsp-function, @lsp-func
        for directive in [
            "@lsp-declare-function",
            "@lsp-declare-func",
            "@lsp-function",
            "@lsp-func",
        ] {
            let content = format!("# {} myfunc", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.declared_functions.len(), 1, "Failed for {}", directive);
            assert_eq!(
                meta.declared_functions[0].name, "myfunc",
                "Failed for {}",
                directive
            );
            assert!(
                meta.declared_functions[0].is_function,
                "Should be function for {}",
                directive
            );
        }
    }

    #[test]
    fn test_declare_func_line_number_recorded() {
        let content = "x <- 1\ny <- 2\n# @lsp-func myfunc\nz <- 3";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].line, 2); // 0-based line number
    }

    #[test]
    fn test_declare_func_multiple() {
        let content = "# @lsp-func func1\n# @lsp-func func2\n# @lsp-func func3";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 3);
        assert_eq!(meta.declared_functions[0].name, "func1");
        assert_eq!(meta.declared_functions[0].line, 0);
        assert_eq!(meta.declared_functions[1].name, "func2");
        assert_eq!(meta.declared_functions[1].line, 1);
        assert_eq!(meta.declared_functions[2].name, "func3");
        assert_eq!(meta.declared_functions[2].line, 2);
    }

    #[test]
    fn test_declare_func_with_special_chars_quoted() {
        // R allows special characters in symbol names when quoted
        let content = r#"# @lsp-func "my.special_func""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my.special_func");
    }

    #[test]
    fn test_declare_func_no_at_prefix_not_recognized() {
        // Requirement 2.6: Directives without @ prefix should NOT be recognized
        let content = "# lsp-func myfunc";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 0);
    }

    // Mixed declaration tests
    #[test]
    fn test_declare_mixed_vars_and_funcs() {
        let content = "# @lsp-var myvar\n# @lsp-func myfunc\n# @lsp-variable another_var\n# @lsp-function another_func";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 2);
        assert_eq!(meta.declared_functions.len(), 2);
        assert_eq!(meta.declared_variables[0].name, "myvar");
        assert_eq!(meta.declared_variables[1].name, "another_var");
        assert_eq!(meta.declared_functions[0].name, "myfunc");
        assert_eq!(meta.declared_functions[1].name, "another_func");
    }

    #[test]
    fn test_declare_with_other_directives() {
        let content = r#"# @lsp-sourced-by ../main.R
# @lsp-var myvar
# @lsp-cd /data
# @lsp-func myfunc
# @lsp-ignore"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.working_directory, Some("/data".to_string()));
        assert!(meta.ignored_lines.contains_key(&4));
    }

    // Edge cases
    #[test]
    fn test_declare_var_empty_name_skipped() {
        // Empty symbol names should be skipped
        let content = "# @lsp-var ";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 0);
    }

    #[test]
    fn test_declare_func_empty_name_skipped() {
        // Empty symbol names should be skipped
        let content = "# @lsp-func ";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 0);
    }

    #[test]
    fn test_declare_var_whitespace_only_quoted_skipped() {
        // Whitespace-only quoted names should be skipped
        let content = r#"# @lsp-var "   ""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 0);
    }

    #[test]
    fn test_declare_func_whitespace_only_quoted_skipped() {
        // Whitespace-only quoted names should be skipped
        let content = r#"# @lsp-func "   ""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 0);
    }

    #[test]
    fn test_declare_var_colon_and_quotes() {
        let content = r#"# @lsp-var: "my.var""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "my.var");
    }

    #[test]
    fn test_declare_func_colon_and_quotes() {
        let content = r#"# @lsp-func: "my.func""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my.func");
    }

    // Issue #346: a raw leading U+FEFF (BOM) in in-memory text must not hide a
    // first-line directive. tree-sitter-r skips the BOM as whitespace, but
    // Rust's `\s`/`str::trim` follow Unicode `White_Space`, which excludes
    // U+FEFF, so the column-0 scan anchor here would otherwise miss the `#`.
    #[test]
    fn bom_prefixed_forward_directive_on_first_line_parses() {
        let content = "\u{FEFF}# @lsp-source utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].directive_line, 0);
    }

    #[test]
    fn bom_prefixed_backward_directive_on_first_line_parses() {
        let content = "\u{FEFF}# @lsp-sourced-by ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Default);
    }

    #[test]
    fn bom_on_first_line_does_not_prematurely_end_header() {
        // The BOM-prefixed first comment line must still count as header, so a
        // backward directive on the next line is still recognised.
        let content = "\u{FEFF}# a header comment\n# @lsp-sourced-by ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    // ---- #421: `# raven:` aliases for all structural directive families ----
    //
    // `# raven:` is the canonical user-facing prefix; `@lsp-` remains a
    // permanent alias. The structural families share their keyword vocabulary
    // and grammar with the `@lsp-` forms, so these tests assert parity (same
    // parse result for both prefixes) plus a near-miss matrix.

    #[test]
    fn raven_forward_directive_parity() {
        let lsp = parse_directives("# @lsp-source utils.R");
        let raven = parse_directives("# raven: source utils.R");
        assert_eq!(raven.sources.len(), 1);
        assert_eq!(raven.sources[0].path, "utils.R");
        assert!(raven.sources[0].is_directive);
        assert_eq!(raven.sources[0].path, lsp.sources[0].path);
    }

    #[test]
    fn raven_forward_directive_colon_quotes_line() {
        let meta = parse_directives(r#"# raven: source: "utils/helpers.R" line=20"#);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils/helpers.R");
        assert_eq!(meta.sources[0].line, 19); // 1-based 20 -> 0-based 19
    }

    #[test]
    fn raven_forward_directive_no_space_after_colon() {
        // `raven:\s*` permits zero spaces, mirroring the suppression grammar.
        let meta = parse_directives("# raven:source utils.R");
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
    }

    #[test]
    fn raven_forward_synonyms_all() {
        for kw in FORWARD_DIRECTIVE_KEYWORDS.split('|') {
            let meta = parse_directives(&format!("# raven: {kw} utils.R"));
            assert_eq!(meta.sources.len(), 1, "raven: {kw} failed");
            assert_eq!(meta.sources[0].path, "utils.R", "raven: {kw} path");
        }
    }

    #[test]
    fn raven_backward_directive_parity() {
        let lsp = parse_directives("# @lsp-sourced-by ../main.R line=15");
        let raven = parse_directives("# raven: sourced-by ../main.R line=15");
        assert_eq!(raven.sourced_by.len(), 1);
        assert_eq!(raven.sourced_by[0].path, "../main.R");
        assert_eq!(raven.sourced_by[0].call_site, CallSiteSpec::Line(14));
        assert_eq!(raven.sourced_by[0].call_site, lsp.sourced_by[0].call_site);
    }

    #[test]
    fn raven_backward_synonyms_and_match() {
        for kw in BACKWARD_DIRECTIVE_KEYWORDS.split('|') {
            let meta = parse_directives(&format!(r#"# raven: {kw} ../main.R match="source(""#));
            assert_eq!(meta.sourced_by.len(), 1, "raven: {kw} failed");
            assert_eq!(
                meta.sourced_by[0].call_site,
                CallSiteSpec::Match("source(".to_string()),
                "raven: {kw} match"
            );
        }
    }

    #[test]
    fn raven_working_directory_parity_and_synonyms() {
        for kw in [
            "working-directory",
            "working-dir",
            "current-directory",
            "current-dir",
            "cd",
            "wd",
        ] {
            let meta = parse_directives(&format!("# raven: {kw} /data/scripts"));
            assert_eq!(
                meta.working_directory,
                Some("/data/scripts".to_string()),
                "raven: {kw} failed"
            );
        }
    }

    #[test]
    fn raven_declaration_directives_parity_and_synonyms() {
        for kw in ["var", "variable", "declare-var", "declare-variable"] {
            let meta = parse_directives(&format!("# raven: {kw} myvar"));
            assert_eq!(meta.declared_variables.len(), 1, "raven: {kw} failed");
            assert_eq!(meta.declared_variables[0].name, "myvar");
            assert!(!meta.declared_variables[0].is_function);
        }
        for kw in ["func", "function", "declare-func", "declare-function"] {
            let meta = parse_directives(&format!("# raven: {kw} myfunc"));
            assert_eq!(meta.declared_functions.len(), 1, "raven: {kw} failed");
            assert_eq!(meta.declared_functions[0].name, "myfunc");
            assert!(meta.declared_functions[0].is_function);
        }
    }

    /// `# raven: ignore` must still route to suppression, not the structural
    /// branches — the structural keyword groups are disjoint from ignore/expect.
    #[test]
    fn raven_ignore_still_routes_to_suppression_not_structural() {
        let meta = parse_directives("x <- undefined # raven: ignore");
        assert!(is_line_ignored(&meta, 0));
        assert_eq!(meta.sources.len(), 0);
        assert_eq!(meta.declared_variables.len(), 0);
        assert_eq!(meta.declared_functions.len(), 0);
    }

    /// Header-only parity: `# raven: sourced-by` / `# raven: cd` after code are
    /// ignored, exactly like their `@lsp-` forms.
    #[test]
    fn raven_backward_and_cd_are_header_only() {
        let meta = parse_directives("x <- 1\n# raven: sourced-by ../main.R\n# raven: cd /data");
        assert_eq!(meta.sourced_by.len(), 0);
        assert_eq!(meta.working_directory, None);
    }

    /// Forward directives are not header-only: `# raven: source` is recognised
    /// mid-file, matching `@lsp-source`.
    #[test]
    fn raven_forward_recognised_after_code() {
        let meta = parse_directives("x <- 1\n# raven: source utils.R");
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
    }

    /// Near-miss matrix: lookalikes that must NOT parse as raven directives.
    #[test]
    fn raven_structural_near_misses_do_not_match() {
        let cases = [
            "# raven source utils.R",   // missing colon
            "# ravens: source utils.R", // wrong namespace
            "# ravenx: cd /data",       // wrong namespace
            "# raven :source utils.R",  // space before colon
            "# raven: sourc utils.R",   // not a keyword
        ];
        for c in cases {
            let meta = parse_directives(c);
            assert_eq!(meta.sources.len(), 0, "should not match forward: {c:?}");
            assert_eq!(meta.working_directory, None, "should not set wd: {c:?}");
            assert_eq!(meta.sourced_by.len(), 0, "should not match backward: {c:?}");
            assert_eq!(
                meta.declared_variables.len(),
                0,
                "should not declare var: {c:?}"
            );
        }
    }

    #[test]
    fn parses_nse_whole_call() {
        let meta = parse_directives("# raven: nse my_func\nmy_func(x > 1)\n");
        assert_eq!(meta.nse_declarations.len(), 1);
        let d = &meta.nse_declarations[0];
        assert_eq!(d.name, "my_func");
        assert_eq!(d.package, None);
        assert_eq!(d.scope, crate::cross_file::types::NseScope::WholeCall);
        assert_eq!(d.line, 0);
    }

    #[test]
    fn parses_nse_per_formal_and_variants() {
        use crate::cross_file::types::NseScope;
        let cases = [
            ("# raven: nse my_func(x)", "my_func", None, vec!["x"]),
            (
                "# raven: nse my_func(x, y)",
                "my_func",
                None,
                vec!["x", "y"],
            ),
            ("# raven:nse my_func(x)", "my_func", None, vec!["x"]),
            ("# raven: nse: my_func(x)", "my_func", None, vec!["x"]),
            ("# @lsp-nse my_func(x)", "my_func", None, vec!["x"]),
            (
                "# raven: nse pkg::my_func(x, y)",
                "my_func",
                Some("pkg"),
                vec!["x", "y"],
            ),
        ];
        for (line, name, pkg, formals) in cases {
            let meta = parse_directives(&format!("{line}\n"));
            assert_eq!(meta.nse_declarations.len(), 1, "case: {line}");
            let d = &meta.nse_declarations[0];
            assert_eq!(d.name, name, "case: {line}");
            assert_eq!(d.package.as_deref(), pkg, "case: {line}");
            assert_eq!(
                d.scope,
                NseScope::Formals(formals.iter().map(|s| s.to_string()).collect()),
                "case: {line}"
            );
        }
    }

    #[test]
    fn malformed_nse_is_ignored() {
        for line in [
            "# raven: nse",
            "# raven: nse ()",
            "# raven: nse[my_func(x)]",
        ] {
            let meta = parse_directives(&format!("{line}\n"));
            assert!(meta.nse_declarations.is_empty(), "case: {line}");
        }
    }

    #[test]
    fn nse_empty_parens_is_whole_call() {
        // `nse f()` lists zero captured formals — treated as whole-call, the
        // same as the parenless `nse f` form.
        let meta = parse_directives("# raven: nse my_func()\n");
        assert_eq!(meta.nse_declarations.len(), 1);
        assert_eq!(meta.nse_declarations[0].name, "my_func");
        assert_eq!(
            meta.nse_declarations[0].scope,
            crate::cross_file::types::NseScope::WholeCall
        );
    }

    #[test]
    fn nse_blank_formal_slot_is_dropped_not_broadened() {
        // A blank slot (double comma / trailing comma) is malformed: the directive
        // must NOT become a partial per-formal policy (wrong positions) NOR a
        // whole-call policy (would hide real undefined-variable findings). It is
        // dropped entirely, leaving the callee checked normally (issue #460 review).
        for line in ["# raven: nse my_func(x,,y)", "# raven: nse my_func(a,)"] {
            let meta = parse_directives(&format!("{line}\n"));
            assert!(
                meta.nse_declarations.is_empty(),
                "malformed parenthesized nse must be dropped, not broadened; case: {line}: {meta:?}"
            );
        }
    }

    #[test]
    fn func_blank_formal_slot_records_no_formals() {
        // A blank slot in a func declaration must not record a wrong formal
        // order; the declaration keeps its existence with no formals (so NSE
        // matching for it stays named-only).
        let meta = parse_directives("# raven: func my_func(a,)\n");
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].formals, None);
    }

    #[test]
    fn nse_non_name_leftover_drops_not_rejects() {
        use crate::cross_file::types::NseScope;
        // A non-name token (`2`) is dropped via `is_formal_name`, NOT treated as a
        // blank slot that rejects the whole list — so the valid `x` is still kept.
        let meta = parse_directives("# raven: nse my_func(x, 2)\n");
        assert_eq!(meta.nse_declarations.len(), 1);
        assert_eq!(
            meta.nse_declarations[0].scope,
            NseScope::Formals(vec!["x".to_string()])
        );
    }

    #[test]
    fn nse_and_func_accept_unquoted_unicode_callee() {
        use crate::cross_file::types::NseScope;
        // A valid non-ASCII R identifier (e.g. `données`) must parse unquoted, not
        // be truncated by an ASCII-only capture (issue #460 Codex review).
        let meta = parse_directives("# raven: func données(x, y)\n# raven: nse données(x)\n");
        assert_eq!(meta.declared_functions.len(), 1, "func: {meta:?}");
        assert_eq!(meta.declared_functions[0].name, "données");
        assert_eq!(
            meta.declared_functions[0].formals,
            Some(vec!["x".to_string(), "y".to_string()])
        );
        assert_eq!(meta.nse_declarations.len(), 1, "nse: {meta:?}");
        assert_eq!(meta.nse_declarations[0].name, "données");
        assert_eq!(
            meta.nse_declarations[0].scope,
            NseScope::Formals(vec!["x".to_string()])
        );
    }

    #[test]
    fn func_formals_with_comma_bearing_default_are_not_truncated() {
        // A default containing a `)` (e.g. `c(1, 2)`) must not truncate the formal
        // list at the inner paren and drop the formals after it (issue #460
        // review); both `data` and `expr` must be captured.
        let meta = parse_directives("# raven: func f(data = c(1, 2), expr)\n");
        assert_eq!(meta.declared_functions.len(), 1, "{meta:?}");
        assert_eq!(
            meta.declared_functions[0].formals,
            Some(vec!["data".to_string(), "expr".to_string()])
        );
    }

    #[test]
    fn nse_formals_with_comma_bearing_default_parse() {
        use crate::cross_file::types::NseScope;
        let meta = parse_directives("# raven: nse f(data = c(1, 2), expr)\n");
        assert_eq!(meta.nse_declarations.len(), 1, "{meta:?}");
        assert_eq!(
            meta.nse_declarations[0].scope,
            NseScope::Formals(vec!["data".to_string(), "expr".to_string()])
        );
    }

    #[test]
    fn func_directive_captures_formals() {
        let meta = parse_directives("# raven: func my_func(data, x, y)\n");
        assert_eq!(meta.declared_functions.len(), 1);
        let f = &meta.declared_functions[0];
        assert_eq!(f.name, "my_func");
        assert_eq!(
            f.formals,
            Some(vec!["data".to_string(), "x".to_string(), "y".to_string()])
        );
    }

    #[test]
    fn nse_tolerates_trailing_comment() {
        use crate::cross_file::types::NseScope;
        let meta = parse_directives("# raven: nse my_func(x)   # why this is captured\n");
        assert_eq!(meta.nse_declarations.len(), 1);
        assert_eq!(
            meta.nse_declarations[0].scope,
            NseScope::Formals(vec!["x".to_string()])
        );
    }

    #[test]
    fn nse_unclosed_paren_is_ignored() {
        // A trailing comment is allowed, but a malformed (unclosed) payload must
        // not silently become a whole-call declaration.
        let meta = parse_directives("# raven: nse my_func(x\n");
        assert!(meta.nse_declarations.is_empty());
    }

    #[test]
    fn func_directive_without_formals_keeps_none() {
        let meta = parse_directives("# raven: func my_func\n");
        assert_eq!(meta.declared_functions[0].formals, None);
    }

    #[test]
    fn func_directive_strips_default_values_from_formals() {
        // A pasted signature with defaults keeps only the formal names.
        let meta = parse_directives("# raven: func my_func(data, x = NULL, n = 10)\n");
        assert_eq!(
            meta.declared_functions[0].formals,
            Some(vec!["data".to_string(), "x".to_string(), "n".to_string()])
        );
    }

    #[test]
    fn nse_requires_separator_after_keyword() {
        // Without a separator, `nse` run together with a name must NOT be read
        // as a declaration for a truncated callee (`nseg(col)` -> `g`).
        for line in ["# raven: nseg(col)", "# raven: nse_helper(x)"] {
            let meta = parse_directives(&format!("{line}\n"));
            assert!(meta.nse_declarations.is_empty(), "case: {line}");
        }
        // A real separator (space or colon) still parses.
        for line in ["# raven: nse my_func(x)", "# raven: nse:my_func(x)"] {
            let meta = parse_directives(&format!("{line}\n"));
            assert_eq!(meta.nse_declarations.len(), 1, "case: {line}");
            assert_eq!(meta.nse_declarations[0].name, "my_func", "case: {line}");
        }
    }

    #[test]
    fn nse_accepts_quoted_nonsyntactic_name() {
        use crate::cross_file::types::NseScope;
        // A callee whose name has characters outside `[A-Za-z0-9._]` (e.g. a
        // name with spaces, called `` `my data fn`(x) ``) uses the quoted form,
        // mirroring `func`. The name is stored backtick-wrapped so it matches
        // the call-site callee text.
        let meta = parse_directives("# raven: nse \"my data fn\"(x)\n");
        assert_eq!(meta.nse_declarations.len(), 1);
        assert_eq!(meta.nse_declarations[0].name, "`my data fn`");
        assert_eq!(meta.nse_declarations[0].package, None);
        assert_eq!(
            meta.nse_declarations[0].scope,
            NseScope::Formals(vec!["x".to_string()])
        );
        // A quoted name with no formals is whole-call.
        let meta = parse_directives("# raven: nse 'my data fn'\n");
        assert_eq!(meta.nse_declarations.len(), 1);
        assert_eq!(meta.nse_declarations[0].name, "`my data fn`");
        assert_eq!(meta.nse_declarations[0].scope, NseScope::WholeCall);
        // A quoted syntactic name is stored bare (no backticks needed).
        let meta = parse_directives("# raven: nse \"my_func\"(x)\n");
        assert_eq!(meta.nse_declarations[0].name, "my_func");
        // A malformed `::` qualifier is still rejected even when quoted.
        let meta = parse_directives("# raven: nse \"pkg:::x\"(a)\n");
        assert!(meta.nse_declarations.is_empty());
    }

    #[test]
    fn nse_wraps_dot_digit_and_reserved_callee() {
        use crate::cross_file::types::NseScope;
        // A leading-dot digit name (`.2way`) is NOT a syntactic R name, so the
        // call site is `` `.2way`(x) `` (node_text carries backticks). The stored
        // key must be wrapped to match — `is_formal_name` used to accept it bare
        // and the directive silently never governed the call.
        let meta = parse_directives("# raven: nse \".2way\"(x)\n");
        assert_eq!(meta.nse_declarations.len(), 1);
        assert_eq!(meta.nse_declarations[0].name, "`.2way`");
        assert_eq!(
            meta.nse_declarations[0].scope,
            NseScope::Formals(vec!["x".to_string()])
        );
        // A reserved word used as a callee (`` `if`(...) ``) is likewise
        // non-syntactic and must be wrapped.
        let meta = parse_directives("# raven: nse \"if\"(cond)\n");
        assert_eq!(meta.nse_declarations[0].name, "`if`");
        // A non-ASCII but syntactic identifier is stored bare — it appears
        // without backticks in a UTF-8 source file.
        let meta = parse_directives("# raven: nse \"données\"(x)\n");
        assert_eq!(meta.nse_declarations[0].name, "données");
        // Same rule on the `func` directive's stored callee name.
        let meta = parse_directives("# raven: func \".2way\"(a)\n");
        assert_eq!(meta.declared_functions[0].name, "`.2way`");
    }

    #[test]
    fn nse_rejects_unquoted_truncation() {
        // An UNQUOTED non-syntactic callee leaves trailing text the end-anchored
        // regex can't match, so the directive is dropped rather than stored as a
        // truncated prefix; such names must use the quoted form instead.
        for line in [
            "# raven: nse some-func(x)", // would truncate to `some`
            "# raven: nse obj$method(x)",
            "# raven: nse pkg:::x(a)",
        ] {
            let meta = parse_directives(&format!("{line}\n"));
            assert!(meta.nse_declarations.is_empty(), "case: {line}");
        }
    }

    #[test]
    fn func_rejects_malformed_namespace_qualifier() {
        // A stray-colon name must never be stored as a mis-splittable symbol
        // that `declared_name_matches`' rsplit_once("::") would later mis-pair
        // against a well-formed `# raven: nse pkg::name`. This holds for both
        // the unquoted truncation path and the quoted (verbatim) path.
        for line in [
            "# raven: func pkg:::my_func(x)", // unquoted -> regex truncates to `pkg`, trailing `:`
            "# raven: func ::my_func(x)",     // leading `::`, no bare name
            "# raven: func a:b",              // unquoted single colon (not a bare R name)
            "# raven: func \"pkg:::my_func\"(x)", // quoted malformed qualifier
            "# raven: func \"pkg::a::b\"",    // quoted multi-`::` qualifier
        ] {
            let meta = parse_directives(&format!("{line}\n"));
            assert!(
                meta.declared_functions.is_empty(),
                "malformed name stored for `{line}`: {:?}",
                meta.declared_functions
            );
        }
        // A well-formed qualified name is still captured whole, with formals.
        let meta = parse_directives("# raven: func pkg::my_func(a, b)\n");
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "pkg::my_func");
        assert_eq!(
            meta.declared_functions[0].formals,
            Some(vec!["a".to_string(), "b".to_string()])
        );
        // Non-`::` special characters are still accepted via the quoted form,
        // including a lone `:` (a valid backtick-quoted R symbol `` `a:b` ``):
        // it has no `::` qualifier, so it cannot mis-pair. Such names are stored
        // backtick-wrapped (their call-site form) so they match usages.
        for (line, expected) in [
            ("# raven: func \"my-helper\"\n", "`my-helper`"),
            ("# raven: func \"a:b\"\n", "`a:b`"),
        ] {
            let meta = parse_directives(line);
            assert_eq!(meta.declared_functions.len(), 1, "case: {line}");
            assert_eq!(meta.declared_functions[0].name, expected, "case: {line}");
        }
    }

    #[test]
    fn func_rejects_unquoted_nonsyntactic_truncation() {
        // An unquoted name with a non-`[A-Za-z0-9._]` character (which the regex
        // would silently truncate) must be dropped, not stored as the wrong
        // truncated prefix. Non-syntactic names belong in the quoted form.
        for line in [
            "# raven: func some-func",  // would truncate to `some`
            "# raven: func obj$method", // would truncate to `obj`
            "# raven: func a%b",        // would truncate to `a`
        ] {
            let meta = parse_directives(&format!("{line}\n"));
            assert!(
                meta.declared_functions.is_empty(),
                "truncated name stored for `{line}`: {:?}",
                meta.declared_functions
            );
        }
        // But a separator after the name is ordinary ignored trailing text.
        let meta = parse_directives("# raven: func my_func   trailing note\n");
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my_func");
    }

    #[test]
    fn func_keeps_non_ascii_formal() {
        // A legitimate non-ASCII R identifier in the formal list must be kept
        // (regression: an ASCII-only validator would drop it).
        let meta = parse_directives("# raven: func f(données, x)\n");
        assert_eq!(
            meta.declared_functions[0].formals,
            Some(vec!["données".to_string(), "x".to_string()])
        );
    }

    #[test]
    fn func_default_with_comma_drops_bogus_formal() {
        // A default containing a comma/parens is out of scope (the regex
        // captures only up to the first `)`), but the leftover token must be
        // dropped rather than recorded as a bogus formal name like `2`.
        let meta = parse_directives("# raven: func f(a, x = c(1, 2), y)\n");
        let formals = meta.declared_functions[0].formals.clone().unwrap();
        assert!(
            !formals.iter().any(|f| f == "2"),
            "bogus formal recorded: {formals:?}"
        );
        assert!(
            formals.iter().all(|f| is_formal_name(f)),
            "non-name formal recorded: {formals:?}"
        );
    }

    // ---- issue #479: `# raven: standalone` ----

    #[test]
    fn standalone_directive_sets_flag() {
        assert!(parse_directives("# raven: standalone\nx <- 1\n").standalone);
    }

    #[test]
    fn self_contained_directive_sets_flag() {
        assert!(parse_directives("# raven: self-contained\nx <- 1\n").standalone);
    }

    #[test]
    fn standalone_directive_lsp_alias_sets_flag() {
        assert!(parse_directives("# @lsp-standalone\nx <- 1\n").standalone);
    }

    #[test]
    fn self_contained_directive_lsp_alias_sets_flag() {
        assert!(parse_directives("# @lsp-self-contained\nx <- 1\n").standalone);
    }

    #[test]
    fn standalone_directive_allows_trailing_comment() {
        assert!(parse_directives("# raven: standalone  # a module\n").standalone);
    }

    #[test]
    fn self_contained_directive_allows_trailing_comment() {
        assert!(parse_directives("# raven: self-contained  # a module\n").standalone);
    }

    #[test]
    fn standalone_directive_is_header_only() {
        // A `standalone` token after code must NOT set the flag (header-only,
        // like the backward/working-dir directives).
        assert!(!parse_directives("x <- 1\n# raven: standalone\n").standalone);
    }

    #[test]
    fn self_contained_directive_is_header_only() {
        // A `self-contained` token after code must NOT set the flag
        // (header-only, like the backward/working-dir directives).
        assert!(!parse_directives("x <- 1\n# raven: self-contained\n").standalone);
    }

    #[test]
    fn standalone_directive_absent_by_default() {
        assert!(!parse_directives("x <- 1\n").standalone);
    }

    #[test]
    fn standalone_directive_rejects_payload() {
        // End-anchored: an unexpected payload fails to match rather than being
        // half-parsed.
        assert!(!parse_directives("# raven: standalone foo\n").standalone);
    }

    #[test]
    fn self_contained_directive_rejects_payload() {
        // End-anchored: an unexpected payload fails to match rather than being
        // half-parsed.
        assert!(!parse_directives("# raven: self-contained foo\n").standalone);
    }
}
