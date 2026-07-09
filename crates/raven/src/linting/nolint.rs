//! Lint suppression parsing.
//!
//! Recognised markers:
//! * `# raven: ignore` / `-next` / `-start` / `-end` / `-file` — Raven's
//!   primary suppression namespace (F2), optionally with a `[code]` selector.
//!   A `[code]` selector is enforced per rule: `parse_colon_codes` returns
//!   `LineSuppression::Codes` and `is_suppressed_code` matches against it, so
//!   `# raven: ignore[line-length]` suppresses only `line-length`.
//! * `# nolint` (or `# nolint: rule_a, rule_b`) — lintr-style line-level
//!   suppression on the same source line.
//! * `# nolint start` / `# nolint end` — lintr-style block suppression,
//!   inclusive of the start and end lines.
//! * `# @lsp-ignore` / `# @lsp-ignore-next` — permanent `@lsp-` aliases for
//!   `# raven: ignore` / `# raven: ignore-next`; they parse and behave
//!   identically (the `-next` form suppresses the *following* source line).
//!
//! Same-line markers nested inside a commented-code line — `# x <- 1 # nolint`
//! — are also honoured via a parse-gated fallback (issue #242). The fallback
//! is motivated by and primarily benefits the `commented_code` rule, but is
//! deliberately rule-agnostic so suppression behaviour doesn't depend on
//! which lints happen to be enabled.
//!
//! Rule-name filters (the `: rule_a, rule_b` suffix on `# nolint`) are
//! enforced per rule — `# nolint: line_length` suppresses only the
//! `line_length` lint, leaving others in place. Both kebab-case and lintr
//! `snake_case` spellings are accepted.

use std::collections::HashMap;

use crate::cross_file::types::LineSuppression;
use crate::linting::parse_gate::looks_like_code;

/// Pre-computed line-suppression map for a document: each suppressed line maps
/// to *what* it suppresses (a blanket [`LineSuppression::All`] or a code-scoped
/// set). Code-scoped entries come from a `[code]` selector
/// (`# raven: ignore[line-length]`) or the lintr `# nolint: rule` filter.
#[derive(Debug, Default)]
pub(crate) struct Suppressions {
    suppressed: HashMap<u32, LineSuppression>,
}

impl Suppressions {
    /// Parse `# nolint` / `# raven:` / `# @lsp-ignore` markers out of `text`.
    pub(crate) fn from_text(text: &str) -> Self {
        let mut suppressed: HashMap<u32, LineSuppression> = HashMap::new();
        let mut in_block = false;
        let mut block_start: Option<u32> = None;
        let mut block_codes = LineSuppression::All;
        let mut file_level: Option<LineSuppression> = None;

        let insert = |map: &mut HashMap<u32, LineSuppression>, line: u32, what: LineSuppression| {
            map.entry(line)
                .and_modify(|e| e.merge(what.clone()))
                .or_insert(what);
        };

        for (idx, line) in text.lines().enumerate() {
            let line_no = idx as u32;
            let marker = find_marker(line);

            // Collapsing these inner `if`s into match guards would force a
            // wildcard arm and lose exhaustiveness over `NolintMarker` variants.
            #[allow(clippy::collapsible_match)]
            match marker {
                Some(NolintMarker::Start(codes)) => {
                    if !in_block {
                        in_block = true;
                        block_start = Some(line_no);
                        block_codes = codes;
                    }
                }
                Some(NolintMarker::End) => {
                    if in_block {
                        let start = block_start.unwrap_or(line_no);
                        for l in start..=line_no {
                            insert(&mut suppressed, l, block_codes.clone());
                        }
                        in_block = false;
                        block_start = None;
                        block_codes = LineSuppression::All;
                    }
                }
                Some(NolintMarker::Line(codes)) => {
                    insert(&mut suppressed, line_no, codes);
                }
                Some(NolintMarker::NextLine(codes)) => {
                    insert(&mut suppressed, line_no + 1, codes);
                }
                Some(NolintMarker::File(codes)) => match &mut file_level {
                    Some(existing) => existing.merge(codes),
                    None => file_level = Some(codes),
                },
                None => {}
            }

            if in_block {
                insert(&mut suppressed, line_no, block_codes.clone());
            }
        }

        // Unterminated `nolint start` — treat as suppressing to EOF, matching
        // lintr's behavior so a missing `end` doesn't silently lose coverage.
        if in_block && let Some(start) = block_start {
            let total_lines = text.lines().count() as u32;
            for l in start..total_lines {
                insert(&mut suppressed, l, block_codes.clone());
            }
        }

        // `# raven: ignore-file` suppresses every line in the file (for the
        // matching codes).
        if let Some(codes) = file_level {
            let total_lines = text.lines().count() as u32;
            for l in 0..total_lines {
                insert(&mut suppressed, l, codes.clone());
            }
        }

        Self { suppressed }
    }

    /// Is the given zero-indexed line suppressed for *any* rule? True when the
    /// line carries any suppression entry, blanket or code-scoped. Test-only
    /// helper; production code uses [`Suppressions::is_suppressed_code`].
    #[cfg(test)]
    pub(crate) fn is_suppressed(&self, line: u32) -> bool {
        self.suppressed.contains_key(&line)
    }

    /// Is the given zero-indexed line suppressed for the lint rule `rule_id`
    /// (snake_case, e.g. `line_length`)? A blanket directive covers every rule;
    /// a code-scoped directive covers only rules whose code is
    /// [`suppresses`](crate::diagnostic_code::suppresses)-covered by one of its
    /// listed codes.
    pub(crate) fn is_suppressed_code(&self, line: u32, rule_id: &str) -> bool {
        self.suppressed
            .get(&line)
            .is_some_and(|s| s.covers(Some(rule_id)))
    }
}

enum NolintMarker {
    Line(LineSuppression),
    Start(LineSuppression),
    End,
    /// `# raven: ignore-next` (alias `# @lsp-ignore-next`) — suppresses the
    /// line *after* the marker.
    NextLine(LineSuppression),
    /// `# raven: ignore-file` — suppresses every line in the file.
    File(LineSuppression),
}

/// Parse a `[code, code2]` bracket selector into a [`LineSuppression`]. `text`
/// must begin at (or before, with leading whitespace) the `[`. Returns
/// `Some(LineSuppression::All)` when there is no bracket, and `None` when a
/// bracket is opened but never closed (a malformed selector — the caller must
/// then reject the whole directive rather than silently blanket-suppress).
fn parse_bracket_codes(text: &str) -> Option<LineSuppression> {
    let text = text.trim_start();
    let Some(rest) = text.strip_prefix('[') else {
        return Some(LineSuppression::All);
    };
    let end = rest.find(']')?;
    Some(codes_or_all(&rest[..end]))
}

/// Parse a lintr `: rule_a, rule_b` filter into a [`LineSuppression`]. `text` is
/// everything after the `nolint` keyword. Returns [`LineSuppression::All`] when
/// there is no `:` filter.
fn parse_colon_codes(text: &str) -> LineSuppression {
    let text = text.trim_start();
    match text.strip_prefix(':') {
        Some(rest) => codes_or_all(rest),
        None => LineSuppression::All,
    }
}

/// Split a comma-separated code body into a [`LineSuppression`], normalizing
/// each code to canonical kebab-case. Empty → [`LineSuppression::All`].
fn codes_or_all(body: &str) -> LineSuppression {
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

/// Scan a line for a `# nolint` marker (with optional `start`/`end` suffix).
///
/// Returns `None` if the marker doesn't appear, or if the `#` is inside a
/// string literal. String detection is intentionally simple — it tracks
/// single and double quotes within the same line. R lacks multi-line strings
/// in the common case, so this is good enough for an unobtrusive linter.
///
/// Falls back to [`find_inline_marker`] for the same-line-in-a-commented-code
/// shape (`# x <- 1 # nolint`).
fn find_marker(line: &str) -> Option<NolintMarker> {
    let (_, outer_body) = first_hash_body(line)?;
    if let Some(marker) = classify(outer_body) {
        return Some(marker);
    }
    find_inline_marker(outer_body)
}

/// Return the byte index of the first `#` outside any string literal and
/// the substring of `line` immediately after it. `None` if no such `#` exists.
fn first_hash_body(line: &str) -> Option<(usize, &str)> {
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
            return Some((i, &line[i + 1..]));
        }
        i += 1;
    }
    None
}

/// Fallback for the `# code-here # nolint` shape: a marker nested inside an
/// outer commented-code line. Issue #242.
///
/// Pipeline:
/// 1. **Substring pre-filter** — return immediately unless the outer body
///    contains `nolint` or `@lsp-ignore` as a literal substring. Short-circuits
///    the vast majority of prose comments before any parsing work.
/// 2. **Inner-`#` scan** — find the first `#` inside the outer body that is
///    not inside a string literal. The scan reuses the same single-/double-
///    quote bookkeeping as [`first_hash_body`].
/// 3. **Marker classify** — only treat the inner `#` as a marker if [`classify`]
///    recognises what follows it (`nolint`, `nolint start`, `nolint end`,
///    `@lsp-ignore`, optionally with a rule filter).
/// 4. **Parse-gate** — require the prefix of the outer body (everything
///    between the outer `#` and the inner `#`) to parse as real R code. This
///    is the same gate `commented_code` uses; reusing it avoids accidentally
///    swallowing suppression-like text in prose comments.
///
/// The scan stops at the first inner `#`: a further-nested inner-inner marker
/// (`# foo # bar # nolint`) is intentionally not honoured.
///
/// `# @lsp-ignore-next` in the inline position is **not** honoured: the
/// fallback exists for same-line suppression on commented-out code, so
/// silently mapping it to "suppress line N+1" would suppress an unrelated
/// neighbour while leaving the user's commented-code line still flagged. A
/// user who really wants next-line semantics should put the marker on its
/// own line.
fn find_inline_marker(body: &str) -> Option<NolintMarker> {
    if !body.contains("nolint") && !body.contains("@lsp-ignore") {
        return None;
    }

    let bytes = body.as_bytes();
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
            // First candidate inner `#`. Classify what follows, reject the
            // `NextLine` variant (see the doc comment above), parse-gate the
            // prefix, then commit — or give up.
            let marker = match classify(&body[i + 1..])? {
                NolintMarker::NextLine(_) => return None,
                m => m,
            };
            let prefix = &body[..i];
            let prefix_code = prefix.trim_start_matches(|c: char| c == '#' || c.is_whitespace());
            if looks_like_code(prefix_code) {
                return Some(marker);
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Test-only re-export of [`first_hash_body`] so the cross-parser parity
/// property test in `cross_file::property_tests` can compare this scanner
/// against `directive::comment_region_outside_strings` directly. Kept in
/// lockstep by that test (see the parity doc comment on
/// `comment_region_outside_strings`).
#[cfg(test)]
pub(crate) fn first_hash_body_for_parity_test(line: &str) -> Option<(usize, &str)> {
    first_hash_body(line)
}

fn classify(after_hash: &str) -> Option<NolintMarker> {
    // Strip an arbitrary run of leading whitespace and additional `#` chars
    // (people sometimes write `## nolint`).
    let trimmed = after_hash
        .trim_start_matches(|c: char| c == '#' || c.is_whitespace())
        .to_ascii_lowercase();

    // `# raven:` is the primary suppression namespace (F2). It covers the
    // lint track here exactly as `# nolint`/`@lsp-ignore` do, and honors a
    // `[code]` selector that narrows suppression to specific lint rules.
    if let Some(marker) = classify_raven(&trimmed) {
        return Some(marker);
    }

    if let Some(rest) = matches_keyword(&trimmed, "nolint") {
        let rest = rest.trim_start();
        return if let Some(after) = rest.strip_prefix("start") {
            // `# nolint start` or `# nolint start: rule_a, rule_b`.
            Some(NolintMarker::Start(parse_colon_codes(after)))
        } else if rest.starts_with("end") {
            Some(NolintMarker::End)
        } else {
            // Either bare `# nolint` or `# nolint: rule_a, rule_b`.
            Some(NolintMarker::Line(parse_colon_codes(rest)))
        };
    }

    if let Some(rest) = matches_keyword(&trimmed, "@lsp-ignore")
        .or_else(|| matches_keyword(&trimmed, "@lsp-expect"))
    {
        // An optional `[code]` selector may directly follow the marker.
        // Match `-next`, `:next`, or whitespace+`next` after the marker so
        // `@lsp-ignore-next`, `@lsp-ignore: next`, and `@lsp-ignore next`
        // all resolve to NextLine. Anything else on the same line is a
        // same-line ignore. `@lsp-expect*` is the asserting flavor (F2 Step 3)
        // and suppresses identically here.
        let same_line_codes = parse_bracket_codes(rest)?;
        let after = rest.trim_start_matches(|c: char| c == ':' || c == '-' || c.is_whitespace());
        return if let Some(after_next) = after.strip_prefix("next") {
            Some(NolintMarker::NextLine(parse_bracket_codes(after_next)?))
        } else {
            Some(NolintMarker::Line(same_line_codes))
        };
    }

    None
}

/// Classify a `# raven: <action>` suppression directive (lint track).
///
/// `trimmed` is the lowercased comment body with leading `#`/whitespace
/// stripped. Recognizes `ignore`, `ignore-next`, `ignore-start`, `ignore-end`,
/// and `ignore-file`, each optionally followed by a `[code]` selector that
/// narrows suppression to specific lint rules. Returns `None` when the body is
/// not a `raven:` directive.
fn classify_raven(trimmed: &str) -> Option<NolintMarker> {
    let rest = matches_keyword(trimmed, "raven")?;
    // Require the namespace separator `:` (optionally surrounded by spaces).
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    // `expect` is the asserting flavor (F2 Step 3); in the lint track it
    // suppresses identically to `ignore`. The `unused-suppression` distinction
    // is handled by the analyzer-track directive enumeration, not here.
    let after_ignore =
        matches_keyword(rest, "ignore").or_else(|| matches_keyword(rest, "expect"))?;
    // `after_ignore` is "", "-next", "-start", "-end", "-file", or any of
    // those followed by a `[code]` selector (and the bare `ignore[code]` form).
    let action = after_ignore.trim_start_matches('-');
    let bracket_at = action.find('[').unwrap_or(action.len());
    let word = action[..bracket_at].trim();
    let codes = parse_bracket_codes(&action[bracket_at..])?;
    // Exact action words only: a typo like `ignore-filed` must NOT prefix-match
    // `file` and escalate to a whole-file suppression, and an unterminated
    // `[...` selector (rejected by `parse_bracket_codes` above) must not become
    // a blanket suppression.
    match word {
        "" => Some(NolintMarker::Line(codes)),
        "next" => Some(NolintMarker::NextLine(codes)),
        "start" => Some(NolintMarker::Start(codes)),
        "end" => Some(NolintMarker::End),
        "file" => Some(NolintMarker::File(codes)),
        _ => None,
    }
}

/// Match `keyword` as a word in `haystack`: the keyword must appear as a
/// prefix and the next character (if any) must be a non-identifier byte. This
/// prevents `nolinter` or `@lsp-ignored` from matching `nolint` /
/// `@lsp-ignore` — mirroring the strictness of the directive parser at
/// `cross_file/directive.rs` for `# @lsp-ignore`.
///
/// `pub(crate)` so other rules that need a conservative "is this comment a
/// suppression/directive marker" test (e.g. the indentation rule's aligned-
/// comment exemption) can reuse the same boundary discipline instead of
/// re-approximating it and drifting out of sync with [`classify`].
pub(crate) fn matches_keyword<'a>(haystack: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = haystack.strip_prefix(keyword)?;
    match rest.as_bytes().first() {
        // EOL, whitespace, colon, `-` (used by `@lsp-ignore-next` and
        // `# nolint:`), or `[` (the `[code]` selector). Anything else means we
        // matched the middle of an unrelated word.
        None => Some(rest),
        Some(b) => {
            let c = *b as char;
            if c.is_whitespace() || c == ':' || c == '-' || c == '[' {
                Some(rest)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_marker_suppresses_only_its_line() {
        let s = Suppressions::from_text("x = 1\ny = 2 # nolint\nz = 3\n");
        assert!(!s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(!s.is_suppressed(2));
    }

    #[test]
    fn block_marker_suppresses_inclusive_range() {
        let s = Suppressions::from_text("a\n# nolint start\nb\nc\n# nolint end\nd\n");
        assert!(!s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(s.is_suppressed(2));
        assert!(s.is_suppressed(3));
        assert!(s.is_suppressed(4));
        assert!(!s.is_suppressed(5));
    }

    #[test]
    fn unterminated_block_extends_to_eof() {
        let s = Suppressions::from_text("a\n# nolint start\nb\nc\n");
        assert!(!s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(s.is_suppressed(2));
        assert!(s.is_suppressed(3));
    }

    #[test]
    fn marker_inside_string_does_not_suppress() {
        // `#` is inside a string literal — must not be parsed as a comment.
        let s = Suppressions::from_text("x <- \"# nolint\"\n");
        assert!(!s.is_suppressed(0));
    }

    #[test]
    fn rule_filter_suffix_is_recognized() {
        let s = Suppressions::from_text("x = 1 # nolint: line_length, no_tab\n");
        assert!(s.is_suppressed(0));
    }

    #[test]
    fn lsp_ignore_suppresses_current_line() {
        let s = Suppressions::from_text("x = 1 # @lsp-ignore\ny = 2\n");
        assert!(s.is_suppressed(0));
        assert!(!s.is_suppressed(1));
    }

    #[test]
    fn lsp_ignore_next_suppresses_following_line() {
        let s = Suppressions::from_text("# @lsp-ignore-next\nx = 1\ny = 2\n");
        assert!(!s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(!s.is_suppressed(2));
    }

    #[test]
    fn typo_lsp_ignored_does_not_suppress() {
        // `@lsp-ignored` must not be parsed as `@lsp-ignore` — otherwise a
        // typo silently swallows the lint instead of producing it.
        let s = Suppressions::from_text("x = 1 # @lsp-ignored\n");
        assert!(!s.is_suppressed(0));
    }

    #[test]
    fn typo_nolinter_does_not_suppress() {
        let s = Suppressions::from_text("x = 1 # nolinter\n");
        assert!(!s.is_suppressed(0));
    }

    #[test]
    fn inline_nolint_in_commented_code_line_suppresses() {
        // The body of the outer comment is `x <- 1 # nolint`. The interior
        // `# nolint` should be treated as a marker because the prefix `x <- 1`
        // parses as real R code (issue #242).
        let s = Suppressions::from_text("# x <- 1 # nolint\n");
        assert!(s.is_suppressed(0));
    }

    #[test]
    fn inline_nolint_with_rule_filter_suppresses() {
        let s = Suppressions::from_text("# x <- 1 # nolint: commented_code\n");
        assert!(s.is_suppressed(0));
    }

    #[test]
    fn inline_nolint_start_end_brackets_block() {
        let src = "# x <- 1 # nolint start\n# y <- 2\n# z <- 3 # nolint end\n";
        let s = Suppressions::from_text(src);
        assert!(s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(s.is_suppressed(2));
    }

    #[test]
    fn inline_lsp_ignore_in_commented_code_line_suppresses() {
        let s = Suppressions::from_text("# x <- 1 # @lsp-ignore\n");
        assert!(s.is_suppressed(0));
    }

    #[test]
    fn inline_marker_inside_string_literal_does_not_suppress() {
        // The interior `# nolint` is inside a string in the commented-out
        // code, so it must NOT be treated as a marker.
        let s = Suppressions::from_text("# x <- \"# nolint\"\n");
        assert!(!s.is_suppressed(0));
    }

    #[test]
    fn inline_marker_inside_prose_comment_does_not_suppress() {
        // The prefix `talking about nolint here` is prose, not code, so the
        // parse-gate should reject the fallback and leave the line unsuppressed.
        let s = Suppressions::from_text("# talking about nolint here # nolint\n");
        assert!(!s.is_suppressed(0));
    }

    #[test]
    fn prose_comment_mentioning_nolint_without_inner_hash_does_not_suppress() {
        let s = Suppressions::from_text("# this comment mentions nolint but no second hash\n");
        assert!(!s.is_suppressed(0));
    }

    // Issue #346: a raw leading U+FEFF (BOM) must not hide a first-line nolint
    // marker. This already holds because `first_hash_body` scans bytes for the
    // first `#` and skips the BOM's bytes, but the marker scan is column-0
    // sensitive in spirit, so guard it against regression.
    #[test]
    fn bom_prefixed_nolint_marker_on_first_line_suppresses() {
        let s = Suppressions::from_text("\u{FEFF}x = 1 # nolint\n");
        assert!(s.is_suppressed(0));
    }

    #[test]
    fn bom_prefixed_nolint_start_on_first_line_opens_block() {
        let s = Suppressions::from_text("\u{FEFF}# nolint start\nx = 1\n# nolint end\n");
        assert!(s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(s.is_suppressed(2));
    }

    #[test]
    fn inline_lsp_ignore_next_is_not_honoured() {
        // `@lsp-ignore-next` semantically points at the *following* line.
        // Honouring it inline would silently suppress a neighbouring line
        // while leaving the user's commented-code line still flagged — worse
        // than not honouring it at all. The inline fallback is for same-line
        // markers only.
        let s = Suppressions::from_text("# x <- 1 # @lsp-ignore-next\ny <- 2\n");
        assert!(!s.is_suppressed(0));
        assert!(!s.is_suppressed(1));
    }

    // ---- F2: `# raven:` primary suppression namespace (lint track) ----

    #[test]
    fn raven_ignore_suppresses_current_line() {
        let s = Suppressions::from_text("x = 1 # raven: ignore\ny = 2\n");
        assert!(s.is_suppressed(0));
        assert!(!s.is_suppressed(1));
    }

    #[test]
    fn raven_ignore_with_code_selector_suppresses_line() {
        let s = Suppressions::from_text("x = 1 # raven: ignore[line-length]\n");
        assert!(s.is_suppressed(0));
    }

    #[test]
    fn raven_ignore_next_suppresses_following_line() {
        let s = Suppressions::from_text("# raven: ignore-next\nx = 1\ny = 2\n");
        assert!(!s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(!s.is_suppressed(2));
    }

    #[test]
    fn raven_ignore_start_end_brackets_block() {
        let src = "a\n# raven: ignore-start\nb\nc\n# raven: ignore-end\nd\n";
        let s = Suppressions::from_text(src);
        assert!(!s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(s.is_suppressed(2));
        assert!(s.is_suppressed(3));
        assert!(s.is_suppressed(4));
        assert!(!s.is_suppressed(5));
    }

    #[test]
    fn raven_ignore_file_suppresses_every_line() {
        let s = Suppressions::from_text("# raven: ignore-file\nx = 1\ny = 2\nz = 3\n");
        assert!(s.is_suppressed(0));
        assert!(s.is_suppressed(1));
        assert!(s.is_suppressed(2));
        assert!(s.is_suppressed(3));
    }

    #[test]
    fn raven_ignore_file_with_code_selector_suppresses_every_line() {
        let s = Suppressions::from_text("# raven: ignore-file[object-name]\nx = 1\n");
        assert!(s.is_suppressed(0));
        assert!(s.is_suppressed(1));
    }

    #[test]
    fn typo_ravenous_does_not_suppress() {
        let s = Suppressions::from_text("x = 1 # ravenous appetite\n");
        assert!(!s.is_suppressed(0));
    }

    /// CodeRabbit: a typo like `# raven: ignore-filed` must NOT prefix-match
    /// `file` and escalate to a whole-file suppression.
    #[test]
    fn typo_ignore_filed_does_not_suppress_file() {
        let s = Suppressions::from_text("# raven: ignore-filed\nx = 1\ny = 2\n");
        assert!(!s.is_suppressed(0));
        assert!(!s.is_suppressed(1));
        assert!(!s.is_suppressed(2));
    }

    /// An unterminated `[...` selector must be rejected, not treated as a
    /// blanket suppression.
    #[test]
    fn unterminated_bracket_selector_does_not_suppress() {
        let s = Suppressions::from_text("x = 1 # raven: ignore[object-name\n");
        assert!(!s.is_suppressed(0));
        let s2 = Suppressions::from_text("x = 1 # @lsp-ignore[object-name\n");
        assert!(!s2.is_suppressed(0));
    }

    /// An unknown action word is rejected rather than silently blanketing.
    #[test]
    fn unknown_raven_action_does_not_suppress() {
        let s = Suppressions::from_text("# raven: ignore-frobnicate\nx = 1\n");
        assert!(!s.is_suppressed(0));
        assert!(!s.is_suppressed(1));
    }

    #[test]
    fn raven_without_namespace_colon_does_not_suppress() {
        // A prose comment that happens to start with "raven" but is not a
        // `raven:` directive must not suppress.
        let s = Suppressions::from_text("x = 1 # raven is a static analyzer\n");
        assert!(!s.is_suppressed(0));
    }

    // ---- F2: per-code (`[code]`) lint suppression ----

    use crate::linting::rule_ids;

    #[test]
    fn code_selector_targets_only_the_named_rule() {
        let s = Suppressions::from_text("x = 1 # raven: ignore[line-length]\n");
        assert!(s.is_suppressed_code(0, rule_ids::LINE_LENGTH));
        assert!(!s.is_suppressed_code(0, rule_ids::OBJECT_NAME));
    }

    #[test]
    fn bare_ignore_blankets_all_rules() {
        let s = Suppressions::from_text("x = 1 # raven: ignore\n");
        assert!(s.is_suppressed_code(0, rule_ids::LINE_LENGTH));
        assert!(s.is_suppressed_code(0, rule_ids::OBJECT_NAME));
    }

    #[test]
    fn code_selector_accepts_snake_case_spelling() {
        // A user who writes the lintr snake_case rule id is honored.
        let s = Suppressions::from_text("x = 1 # raven: ignore[line_length]\n");
        assert!(s.is_suppressed_code(0, rule_ids::LINE_LENGTH));
    }

    #[test]
    fn nolint_rule_filter_is_now_honored_per_rule() {
        // The lintr `# nolint: rule` filter now targets that rule only.
        let s = Suppressions::from_text("x = 1 # nolint: line_length\n");
        assert!(s.is_suppressed_code(0, rule_ids::LINE_LENGTH));
        assert!(!s.is_suppressed_code(0, rule_ids::OBJECT_NAME));
    }

    #[test]
    fn nolint_bare_blankets_all_rules() {
        let s = Suppressions::from_text("x = 1 # nolint\n");
        assert!(s.is_suppressed_code(0, rule_ids::LINE_LENGTH));
        assert!(s.is_suppressed_code(0, rule_ids::OBJECT_NAME));
    }

    #[test]
    fn lsp_ignore_with_code_selector_targets_only_named_rule() {
        let s = Suppressions::from_text("x = 1 # @lsp-ignore[object-name]\n");
        assert!(s.is_suppressed_code(0, rule_ids::OBJECT_NAME));
        assert!(!s.is_suppressed_code(0, rule_ids::LINE_LENGTH));
    }

    #[test]
    fn raven_ignore_next_with_code_selector_targets_following_line() {
        let s = Suppressions::from_text("# raven: ignore-next[line-length]\nx = 1\n");
        assert!(s.is_suppressed_code(1, rule_ids::LINE_LENGTH));
        assert!(!s.is_suppressed_code(1, rule_ids::OBJECT_NAME));
    }

    #[test]
    fn multiple_codes_in_one_selector() {
        let s = Suppressions::from_text("x = 1 # raven: ignore[line-length, object-name]\n");
        assert!(s.is_suppressed_code(0, rule_ids::LINE_LENGTH));
        assert!(s.is_suppressed_code(0, rule_ids::OBJECT_NAME));
        assert!(!s.is_suppressed_code(0, rule_ids::NO_TAB));
    }

    #[test]
    fn block_with_code_selector_targets_only_named_rule() {
        let s = Suppressions::from_text(
            "# raven: ignore-start[line-length]\nx = 1\ny = 2\n# raven: ignore-end\n",
        );
        assert!(s.is_suppressed_code(1, rule_ids::LINE_LENGTH));
        assert!(!s.is_suppressed_code(1, rule_ids::OBJECT_NAME));
    }
}
