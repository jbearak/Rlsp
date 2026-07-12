//! Flag lines whose leading whitespace doesn't match the expected indent.
//!
//! Mirrors `lintr::indentation_linter()` with the default tidy hanging style,
//! using its accumulated indent-change model. The full fold semantics live on
//! [`set_expectations`]. The explicit `Indented` pass is verified against a
//! 112-case differential corpus; its infix behavior is strict lintr parity,
//! while Raven's independent aligned-argument and block-form tolerances remain
//! deliberate leniencies.
//!
//! [`InfixContinuationStyle`] is Raven-specific. `Indented` applies the strict
//! block change, `Aligned` pins to the first operand with the owning-statement
//! one-level floor, and `Either` runs both complete folds and unions them.
//! Assignment operators and `assignment_as_infix` suppression are
//! style-independent; see [`operator_change`]. The producer uses this same
//! `Either` expectation machinery but selects from its own settings, so
//! compatible producer/lint pairs agree without either setting steering the
//! other.
//!
//! Like lintr, a run of consecutive lines mis-indented by the same amount
//! produces a single diagnostic (on the run's first line).
//!
//! Lines skipped without checks:
//! * Suppressed lines (`# nolint`, `# nolint start/end`, `# raven: ignore`,
//!   `# raven: ignore-next`; the `# @lsp-ignore` / `# @lsp-ignore-next` forms
//!   are permanent aliases that parse identically).
//! * Blank lines.
//! * Lines whose leading whitespace contains any tab — those belong to the
//!   `no_tab` rule.
//! * Lines that start strictly inside a multi-line string literal.
//! * Standalone comment-only lines vertically aligned with an unbroken run of
//!   same-column comment lines that share the same expected-indent *value*
//!   and that include at least one real trailing comment (code before the
//!   `#`). Directive/suppression marker comments (`# nolint`, `# raven:…`,
//!   `# @lsp-…`) are never anchors and are never exempted this way: a marker
//!   that suppresses its own line (e.g. `# nolint`) is skipped by the
//!   suppression check above before this rule ever runs, and a marker that
//!   doesn't (e.g. `# raven: ignore-next`, `# @lsp-ignore-next`) still gets
//!   the ordinary indentation check. A run cannot bridge lines
//!   whose expected indents differ, and suppressed, string-interior, or
//!   tab-leading lines do not count as anchors or links. (Two lines in
//!   genuinely different scopes that happen to expect the same indent value
//!   can still link into one run — this rule matches on the expected value,
//!   not on scope identity — but that only matters when such lines are also
//!   textually contiguous, which is rare in practice.)
//! * Top-level lines with no enclosing multi-line scope expect indent 0.

use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use tree_sitter::Node;

use crate::linting::LINT_SOURCE;
use crate::linting::config::InfixContinuationStyle;
use crate::linting::nolint::{Suppressions, matches_keyword};
use crate::linting::rule_ids;
use crate::utf16::strip_leading_bom_for_scan;

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    indent_unit: u32,
    infix_style: InfixContinuationStyle,
    severity: DiagnosticSeverity,
    suppressions: &Suppressions,
    out: &mut Vec<Diagnostic>,
) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return;
    }

    let mut string_interior: HashSet<u32> = HashSet::new();
    collect_string_interior_lines(root, &mut string_interior);

    let line_states: Vec<LineState> = lines
        .iter()
        .enumerate()
        .map(|(idx, line_text)| {
            LineState::new(idx as u32, line_text, suppressions, &string_interior)
        })
        .collect();

    let mut comments: HashMap<u32, CommentCol> = HashMap::new();
    collect_comment_cols(root, &lines, &mut comments);

    // One full pass (expectations + aligned-comment exemptions) per style.
    // `Either` runs the `Indented` and `Aligned` passes and accepts a line
    // that ANY pass accepts or exempts — the exemptions must be per-pass
    // because comment-run grouping compares each pass's own `Expected`
    // values, which a merged map cannot reproduce (a run that groups under
    // `Aligned`'s uniform pins breaks against merged multi-value entries).
    let pass_styles: &[InfixContinuationStyle] = match infix_style {
        InfixContinuationStyle::Either => &[
            InfixContinuationStyle::Indented,
            InfixContinuationStyle::Aligned,
        ],
        InfixContinuationStyle::Indented => &[InfixContinuationStyle::Indented],
        InfixContinuationStyle::Aligned => &[InfixContinuationStyle::Aligned],
    };
    let passes: Vec<(HashMap<u32, Expected>, HashSet<u32>)> = pass_styles
        .iter()
        .map(|&style| {
            let expectations = expectations_for_style(root, &lines, indent_unit, style);
            let exemptions =
                collect_aligned_comment_exemptions(&lines, &expectations, &comments, &line_states);
            (expectations, exemptions)
        })
        .collect();

    // Merged per-line accepted values, used only for diagnostic messages and
    // the run-coalescing diff (the primary comes from the first pass, which
    // is the lintr-compatible `Indented` fold when `Either` runs both).
    let merged: HashMap<u32, Expected> = merge_pass_expectations(&passes);

    // lintr suppresses consecutive lints with the same indentation
    // difference — one diagnostic per run of equally mis-indented lines.
    let mut last_bad: Option<(u32, i64)> = None;

    for (idx, line_text) in lines.iter().enumerate() {
        let line_no = idx as u32;
        if line_states[idx].skips_indentation_check() {
            continue;
        }

        let actual = leading_space_count(line_text);
        let standalone_comment = is_standalone_comment_line(line_text);
        let acceptable = passes.iter().any(|(expectations, exemptions)| {
            let expected = expectations
                .get(&line_no)
                .cloned()
                .unwrap_or_else(Expected::top_level);
            expected.is_acceptable(actual) || (exemptions.contains(&line_no) && standalone_comment)
        });
        if acceptable {
            continue;
        }

        let expected = merged
            .get(&line_no)
            .cloned()
            .unwrap_or_else(Expected::top_level);

        let diff = i64::from(expected.primary) - i64::from(actual);
        let consecutive_same_diff = last_bad
            .is_some_and(|(prev_line, prev_diff)| line_no == prev_line + 1 && diff == prev_diff);
        last_bad = Some((line_no, diff));
        if consecutive_same_diff {
            continue;
        }

        out.push(Diagnostic {
            range: Range {
                start: Position::new(line_no, 0),
                end: Position::new(line_no, actual),
            },
            severity: Some(severity),
            source: Some(LINT_SOURCE.to_string()),
            code: Some(NumberOrString::String(rule_ids::INDENTATION.to_string())),
            message: expected.message(actual),
            ..Default::default()
        });
    }
}

/// What an additionally accepted indentation column represents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IndentKind {
    /// Aligned-argument / opener column, including hanging bracket pins.
    OpenerAligned,
    /// Unit-based indentation owned by a parenthesized argument construct.
    ArgumentBlock,
    /// Operator-chain-start column.
    ChainStart,
    /// Unit-based indentation owned by a non-assignment infix continuation.
    InfixBlock,
    /// A style-neutral unit-based block level.
    Block,
    /// Column zero contributed because one style pass does not cover a line.
    ///
    /// This is never a style preference target; it exists only to complete
    /// the accepted set when style passes are merged.
    TopLevel,
}

impl IndentKind {
    const fn bit(self) -> u8 {
        match self {
            IndentKind::OpenerAligned => 1 << 0,
            IndentKind::ArgumentBlock => 1 << 1,
            IndentKind::ChainStart => 1 << 2,
            IndentKind::InfixBlock => 1 << 3,
            IndentKind::Block => 1 << 4,
            IndentKind::TopLevel => 1 << 5,
        }
    }

    const ALL: [IndentKind; 6] = [
        IndentKind::OpenerAligned,
        IndentKind::ArgumentBlock,
        IndentKind::ChainStart,
        IndentKind::InfixBlock,
        IndentKind::Block,
        IndentKind::TopLevel,
    ];
}

/// A set of [`IndentKind`] tags, packed into one byte. Expectations attach a
/// set to every accepted column, and the whole-document diagnostic fold keeps
/// one per line — a `Vec<IndentKind>` there costs a heap allocation per line
/// for at most six flags.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct IndentKindSet(u8);

impl IndentKindSet {
    pub(crate) fn single(kind: IndentKind) -> Self {
        Self(kind.bit())
    }

    pub(crate) fn contains(self, kind: IndentKind) -> bool {
        self.0 & kind.bit() != 0
    }

    fn merge(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

impl std::fmt::Debug for IndentKindSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set()
            .entries(IndentKind::ALL.iter().filter(|kind| self.contains(**kind)))
            .finish()
    }
}

/// Accepted indentation columns for one line, computed by the lint's own
/// expectation engine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct LineIndentExpectation {
    pub(crate) primary: u32,
    /// Every meaning attached to the primary column.
    pub(crate) primary_kinds: IndentKindSet,
    /// Every additionally accepted column and all meanings attached to it.
    pub(crate) alternatives: Vec<(u32, IndentKindSet)>,
}

impl LineIndentExpectation {
    /// True when `actual` is the primary or any accepted alternative column —
    /// the same acceptance the diagnostic pass applies via
    /// `Expected::is_acceptable`.
    pub(crate) fn accepts(&self, actual: u32) -> bool {
        actual == self.primary
            || self
                .alternatives
                .iter()
                .any(|(column, _)| *column == actual)
    }
}

/// Accepted indent columns for `line`, as the indentation lint would compute
/// them, minus suppression/blank/string-interior/tab skips and comment
/// exemptions. The on-type indentation producer probes with a plain
/// identifier line, so none of those exemptions apply. Test-only convenience
/// over [`accepted_indents_for_lines`], which production callers use to fold
/// several lines from one change collection.
#[cfg(test)]
pub(crate) fn accepted_indents_for_line(
    text: &str,
    root: Node<'_>,
    indent_unit: u32,
    infix_style: InfixContinuationStyle,
    line: u32,
) -> LineIndentExpectation {
    accepted_indents_for_lines(text, root, indent_unit, infix_style, &[line])
        .pop()
        .expect("one target line yields one expectation")
}

/// Batched form of the test-only `accepted_indents_for_line`: one expectation
/// per entry of `targets`, in order. The change list is collected and sorted once per
/// style pass and then folded per target line, so this never materializes the
/// whole-document expectation maps the diagnostic pass builds — the on-type
/// judge calls it on every Enter press (#611).
pub(crate) fn accepted_indents_for_lines(
    text: &str,
    root: Node<'_>,
    indent_unit: u32,
    infix_style: InfixContinuationStyle,
    targets: &[u32],
) -> Vec<LineIndentExpectation> {
    let lines: Vec<&str> = text.lines().collect();
    let pass_styles: &[InfixContinuationStyle] = match infix_style {
        InfixContinuationStyle::Either => &[
            InfixContinuationStyle::Indented,
            InfixContinuationStyle::Aligned,
        ],
        InfixContinuationStyle::Indented => &[InfixContinuationStyle::Indented],
        InfixContinuationStyle::Aligned => &[InfixContinuationStyle::Aligned],
    };
    let bounds = targets
        .iter()
        .copied()
        .min()
        .zip(targets.iter().copied().max())
        .map(|(min, max)| (min as usize, max as usize));
    let mut merged: Vec<Option<Expected>> = vec![None; targets.len()];
    for &style in pass_styles {
        let mut changes = Vec::new();
        collect_changes_bounded(root, &lines, style, bounds, &mut changes);
        changes.sort_by_key(|c| c.token_byte);
        let pass = expectations_for_targets(&changes, lines.len(), indent_unit, targets);
        for (slot, expected) in pass.into_iter().enumerate() {
            match &mut merged[slot] {
                None => merged[slot] = Some(expected),
                Some(existing) => {
                    existing.add_alternative(expected.primary, expected.primary_kinds);
                    for (column, kinds) in expected.alternatives {
                        existing.add_alternative(column, kinds);
                    }
                }
            }
        }
    }

    merged
        .into_iter()
        .map(|expected| {
            let expected = expected.expect("every style pass fills every target slot");
            LineIndentExpectation {
                primary: expected.primary,
                primary_kinds: expected.primary_kinds,
                alternatives: expected.alternatives,
            }
        })
        .collect()
}

/// Fold the sorted change list ONCE, applying each change to every covered
/// target's accumulator — the exact per-line body of [`set_expectations`],
/// restricted to the requested lines (the judge asks for two per Enter).
fn expectations_for_targets(
    changes: &[Change],
    line_count: usize,
    indent_unit: u32,
    targets: &[u32],
) -> Vec<Expected> {
    /// One target line's in-progress fold: the same (primary, kinds,
    /// alternatives) triple `set_expectations` keeps per document line.
    type Accum = (u32, IndentKindSet, Vec<(u32, IndentKindSet)>);
    let mut accums: Vec<Accum> = targets
        .iter()
        .map(|_| {
            (
                0u32,
                IndentKindSet::single(IndentKind::TopLevel),
                Vec::new(),
            )
        })
        .collect();
    for change in changes {
        let begin = change.begin as usize;
        let end = (change.end as usize).min(line_count.saturating_sub(1));
        if begin > end {
            continue;
        }
        for (slot, &line) in targets.iter().enumerate() {
            let line = line as usize;
            if line < begin || line > end {
                continue;
            }
            let (primary, kinds, alternatives) = &mut accums[slot];
            apply_change_to_line(change, indent_unit, primary, kinds, alternatives);
        }
    }
    accums
        .into_iter()
        .map(|(primary, kinds, alternatives)| {
            // Mirror the whole-document fold exactly: it only records lines
            // whose primary or alternatives are non-trivial, and an absent
            // line reads back as `top_level()` — so a fold landing on a bare
            // column 0 canonicalizes its kind to `TopLevel`.
            if primary == 0 && alternatives.is_empty() {
                return Expected::top_level();
            }
            let mut expected = Expected::single(primary, kinds);
            for (column, kinds) in alternatives {
                expected.add_alternative(column, kinds);
            }
            expected
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn lint_for_judge_test(
    text: &str,
    indent_unit: u32,
    infix_style: InfixContinuationStyle,
) -> Vec<Diagnostic> {
    let tree = crate::parser_pool::with_parser(|parser| parser.parse(text, None))
        .expect("test input must parse");
    let suppressions = crate::linting::nolint::Suppressions::from_text(text);
    let mut out = Vec::new();
    collect(
        text,
        tree.root_node(),
        indent_unit,
        infix_style,
        DiagnosticSeverity::HINT,
        &suppressions,
        &mut out,
    );
    out
}
#[derive(Clone, Copy)]
struct LineState {
    is_suppressed: bool,
    is_blank: bool,
    is_string_interior: bool,
    has_tab_in_leading: bool,
}

impl LineState {
    /// Snapshot the predicates that make the indentation rule skip a line, so
    /// the diagnostic pass and aligned-comment grouping share the same view.
    fn new(
        line_no: u32,
        line_text: &str,
        suppressions: &Suppressions,
        string_interior: &HashSet<u32>,
    ) -> Self {
        Self {
            is_suppressed: suppressions.is_suppressed_code(line_no, rule_ids::INDENTATION),
            is_blank: line_text.trim().is_empty(),
            is_string_interior: string_interior.contains(&line_no),
            has_tab_in_leading: has_tab_in_leading(line_text),
        }
    }

    /// True when the existing indentation rule would leave this line to
    /// suppression, blank-line handling, string handling, or `no_tab`.
    fn skips_indentation_check(self) -> bool {
        self.is_suppressed || self.is_blank || self.is_string_interior || self.has_tab_in_leading
    }

    /// True when a comment on this line may participate in an aligned-comment
    /// run. Blankness is irrelevant because a parsed comment line is nonblank.
    fn is_comment_run_eligible(self) -> bool {
        !self.is_suppressed && !self.is_string_interior && !self.has_tab_in_leading
    }
}

/// Acceptable indent values for a single line.
///
/// Most lines have a single acceptable indent, but multi-line argument lists
/// whose opener carries content on the same line accept either the aligned
/// column or the hanging indent (lintr's tidy default for argument lists).
#[derive(Clone)]
struct Expected {
    primary: u32,
    primary_kinds: IndentKindSet,
    alternatives: Vec<(u32, IndentKindSet)>,
}

impl Expected {
    fn single(value: u32, kinds: IndentKindSet) -> Self {
        Self {
            primary: value,
            primary_kinds: kinds,
            alternatives: Vec::new(),
        }
    }

    fn top_level() -> Self {
        Self::single(0, IndentKindSet::single(IndentKind::TopLevel))
    }

    fn is_acceptable(&self, actual: u32) -> bool {
        actual == self.primary
            || self
                .alternatives
                .iter()
                .any(|(column, _)| *column == actual)
    }

    fn add_alternative(&mut self, column: u32, kinds: IndentKindSet) {
        if column == self.primary {
            self.primary_kinds.merge(kinds);
            return;
        }
        if let Some((_, existing_kinds)) = self
            .alternatives
            .iter_mut()
            .find(|(existing, _)| *existing == column)
        {
            existing_kinds.merge(kinds);
        } else {
            self.alternatives.push((column, kinds));
        }
    }

    /// Value-equality on the acceptable indents, independent of alternative
    /// order. Aligned comment runs use this as a proxy for "same scope" to
    /// avoid bridging lines with different expected indents; it compares the
    /// expected *value*, not scope identity, so two lines in different scopes
    /// that coincidentally expect the same indent still compare equal (see
    /// the module doc's aligned-comment bullet).
    fn has_same_structure_as(&self, other: &Self) -> bool {
        if self.primary != other.primary {
            return false;
        }

        let mut left: Vec<u32> = self
            .alternatives
            .iter()
            .map(|(column, _)| *column)
            .collect();
        left.sort_unstable();
        left.dedup();
        let mut right: Vec<u32> = other
            .alternatives
            .iter()
            .map(|(column, _)| *column)
            .collect();
        right.sort_unstable();
        right.dedup();
        left == right
    }

    fn message(&self, actual: u32) -> String {
        if self.alternatives.is_empty() {
            format!(
                "Indentation should be {} spaces, not {}.",
                self.primary, actual
            )
        } else {
            let mut options: Vec<u32> = std::iter::once(self.primary)
                .chain(self.alternatives.iter().map(|(column, _)| *column))
                .collect();
            options.sort_unstable();
            options.dedup();
            let listed = options
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" or ");
            format!("Indentation should be {listed} spaces, not {actual}.")
        }
    }
}

#[derive(Clone, Copy)]
struct CommentCol {
    /// Character count of the prefix before the `#` (post-BOM-strip), used to
    /// decide run membership. Character count — not `col_bytes` — is the
    /// right unit here: a standalone comment's column is always a run of
    /// literal ASCII spaces (so char count equals byte count), but a
    /// *trailing* comment's column can differ from its byte offset when
    /// multi-byte UTF-8 characters precede the `#`. Comparing char counts
    /// keeps "same column" matching the visual alignment a user sees, the
    /// same thing `leading_space_count` already measures for indentation.
    align_col: u32,
    is_trailing: bool,
    is_directive_marker: bool,
}

/// Collect every parsed comment's column and whether real code precedes it on
/// the same line. The BOM-safe prefix scan mirrors `commented_code` so a
/// first-line BOM never turns a standalone comment into a false trailing one.
fn collect_comment_cols(node: Node<'_>, lines: &[&str], out: &mut HashMap<u32, CommentCol>) {
    if node.kind() == "comment" {
        let start = node.start_position();
        let line_idx = start.row;
        if let Some(line) = lines.get(line_idx).copied() {
            let col_bytes = start.column;
            if col_bytes <= line.len() {
                let prefix = strip_leading_bom_for_scan(&line[..col_bytes]);
                let is_trailing = !prefix.bytes().all(|b| b == b' ' || b == b'\t');
                let align_col = prefix.chars().count() as u32;
                let is_directive_marker = comment_body_after_hash(line, col_bytes)
                    .is_some_and(is_directive_marker_comment_body);

                out.insert(
                    line_idx as u32,
                    CommentCol {
                        align_col,
                        is_trailing,
                        is_directive_marker,
                    },
                );
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_comment_cols(child, lines, out);
    }
}

/// Extract the trimmed body after the `#` at `col_bytes`, using tree-sitter's
/// byte column. Returns `None` if the column does not point at a comment start.
fn comment_body_after_hash(line: &str, col_bytes: usize) -> Option<&str> {
    line.get(col_bytes..)?.strip_prefix('#').map(str::trim)
}

/// Conservative marker test for comments that must never anchor or be
/// exempted by the aligned-comment rule (they always get the ordinary
/// indentation check). Reuses `nolint::matches_keyword` — the same
/// boundary-aware keyword matcher `nolint.rs::classify` uses — for `nolint`
/// and `raven`, instead of re-approximating its rules, so this stays in
/// lockstep with the real suppression parser rather than drifting out of
/// sync with it (repeated leading `#`/whitespace, case, and
/// whitespace-before-`:` are all normalized identically).
///
/// `@lsp-` is checked as a bare literal prefix rather than via
/// `matches_keyword`, deliberately covering the *entire* `@lsp-` namespace —
/// not just the lint-suppression `@lsp-ignore`/`@lsp-expect` forms, but also
/// the cross-file directive aliases (`@lsp-source`, `@lsp-sourced-by`,
/// `@lsp-cd`, `@lsp-declare-function`, …; see `cross_file::directive`'s
/// `DIRECTIVE_PREFIX`). No word-boundary check is needed here: `@lsp-` is an
/// unambiguous sigil sequence no prose collides with, and `directive.rs`'s
/// own pre-filter (`line.contains("@lsp-")`) is equally unbounded.
///
/// This is intentionally broader than exact suppression classification for
/// the `raven`/`@lsp-` namespaces — e.g. it doesn't verify `ignore`/`expect`
/// (or any other action keyword) follows — but that's safe: treating a line
/// as a marker only ever removes it from anchor/exemption eligibility, never
/// causes a false diagnostic.
fn is_directive_marker_comment_body(body: &str) -> bool {
    let body = body
        .trim_start_matches(|c: char| c == '#' || c.is_whitespace())
        .to_ascii_lowercase();

    if matches_keyword(&body, "nolint").is_some() || body.starts_with("@lsp-") {
        return true;
    }
    match matches_keyword(&body, "raven") {
        Some(rest) => rest.trim_start().starts_with(':'),
        None => false,
    }
}

/// Find standalone comment-only lines that are vertically aligned with an
/// unbroken same-column run containing a real trailing comment in the same
/// expected-indent scope. Suppressed, string-interior, and tab-leading lines
/// break eligibility; directive markers may appear in a run but never anchor
/// it and are never exempted themselves.
fn collect_aligned_comment_exemptions(
    lines: &[&str],
    expectations: &HashMap<u32, Expected>,
    comments: &HashMap<u32, CommentCol>,
    line_states: &[LineState],
) -> HashSet<u32> {
    let mut exempt = HashSet::new();
    let mut idx = 0usize;

    while idx < lines.len() {
        let Some((first_comment, first_expected)) =
            comment_run_member(idx, expectations, comments, line_states)
        else {
            idx += 1;
            continue;
        };

        let run_start = idx;
        let run_col = first_comment.align_col;
        let mut previous_expected = first_expected;
        let mut run_has_anchor = first_comment.is_trailing && !first_comment.is_directive_marker;
        idx += 1;

        while idx < lines.len() {
            let Some((comment, expected)) =
                comment_run_member(idx, expectations, comments, line_states)
            else {
                break;
            };
            if comment.align_col != run_col || !expected.has_same_structure_as(&previous_expected) {
                break;
            }

            run_has_anchor |= comment.is_trailing && !comment.is_directive_marker;
            previous_expected = expected;
            idx += 1;
        }

        if run_has_anchor {
            for (line_idx, line) in lines.iter().enumerate().take(idx).skip(run_start) {
                let line_no = line_idx as u32;
                let Some(comment) = comments.get(&line_no) else {
                    continue;
                };
                if !comment.is_directive_marker && is_standalone_comment_line(line) {
                    exempt.insert(line_no);
                }
            }
        }
    }

    exempt
}

/// Return the comment and expected-indent scope for a line that can join an
/// aligned-comment run; ineligible lines and lines without comments break runs.
fn comment_run_member(
    idx: usize,
    expectations: &HashMap<u32, Expected>,
    comments: &HashMap<u32, CommentCol>,
    line_states: &[LineState],
) -> Option<(CommentCol, Expected)> {
    if !line_states.get(idx)?.is_comment_run_eligible() {
        return None;
    }

    let line_no = idx as u32;
    let comment = *comments.get(&line_no)?;
    let expected = expectations
        .get(&line_no)
        .cloned()
        .unwrap_or_else(Expected::top_level);
    Some((comment, expected))
}

/// Build per-line expected indents with lintr's accumulated "indent change"
/// model. Indent-inducing tokens are collected in document order; each change
/// covers a line range and rewrites those lines' expectation as a function of
/// the *current* expected value — `Block` adds one unit, `Double` two,
/// `Hanging` pins an absolute column. This mirrors
/// `lintr::indentation_linter()`'s algorithm (tidy style), so nested scopes
/// accumulate exactly like lintr instead of anchoring to a line's physical
/// indentation.
///
/// The changes:
/// * A bracket opener (`{`, `(`, `[`, `[[` — from braces, call/subset
///   arguments, parameter lists, parenthesized expressions, and
///   `if`/`while`/`for` statement parens) covers the lines from the opener's
///   next line through the last content line before the closer. A closer that
///   starts its own line is therefore *outside* the range and keeps the
///   surrounding expectation — closer alignment falls out of the model. The
///   change is `Block` when the closer starts its own line and
///   `Hanging(column after the opener)` when the closer trails content
///   (lintr's tidy exclusivity). A function/lambda parameter list whose
///   first parameter sits on a later line than `function` *and* whose closer
///   trails the last parameter is `Double` (tidyverse double-indent
///   definitions). An opener is skipped entirely when a same-line following
///   sibling spans multiple lines and itself contains an end-of-line opener
///   (`foo(bar(`, trailing lambdas `map(x, function(y) {` — only the inner
///   bracket indents, lintr's double-indent avoidance).
/// * An infix operator token at end of line (binary operators including
///   `%…%`, plus `$`/`@`/`::` chains and the `=` of named arguments and
///   formal defaults) covers its right-hand operand's lines as `Block` —
///   unless suppressed by lintr's `assignment_as_infix` default: an operator
///   nested under an assignment whose own operator ends its line (walking up
///   until a call argument or braced block resets the context) adds no
///   second level, so `x <-\n  a +\n  b` is flat. Under
///   `InfixContinuationStyle::Aligned` the non-assignment operators instead
///   pin their continuation to the chain-start column (`Hanging`); see
///   `operator_change`.
/// * `else` / `repeat` at end of line and an unbraced control-flow or
///   function body (`)` at end of line with no `{` following) cover the body
///   lines as `Block`.
///
/// Raven layers argument-layout tolerances on top as alternative accepted
/// values: aligned-with-opener style for lists whose opener carries content
/// (`foo(a,\n    b\n)`), and the block form where lintr demands hanging (or
/// one unit where lintr demands two). Infix behavior itself is strict per
/// pass: `Indented` uses only the block change; `Aligned` replaces it with the
/// floored chain-start pin. `Either` never reaches this function — `collect`
/// runs
/// one full pass (this fold plus the aligned-comment exemptions) per style
/// and accepts a line that any pass accepts, so `Either` is the union of
/// `Indented` and `Aligned` by construction. A per-alternative encoding in a
/// single fold cannot deliver that: downstream bracket tolerances and the
/// comment-run grouping are functions of the fold's own primaries, which
/// differ between the block path and the aligned pins. Genuine
/// under-indentation stays flagged in every style because the chain-start
/// line itself carries no aligned alternative and is checked against its own
/// expectation.
/// Union the per-line accepted sets of the style passes: the primary comes
/// from the first pass (the lintr-compatible `Indented` fold when `Either`
/// runs both), every other pass contributes its primary and alternatives as
/// alternatives. A line absent from a pass's map expects indent 0 there
/// (`Expected::top_level`), so absence contributes 0 to the union — e.g. an
/// a pass that does not cover a line still contributes top level. Used
/// only for diagnostic messages and the run-coalescing diff; acceptance is
/// decided per pass in `collect`.
fn merge_pass_expectations(
    passes: &[(HashMap<u32, Expected>, HashSet<u32>)],
) -> HashMap<u32, Expected> {
    let maps: Vec<&HashMap<u32, Expected>> = passes.iter().map(|(map, _)| map).collect();
    merge_expectation_maps(&maps)
}

fn merge_expectation_maps(maps: &[&HashMap<u32, Expected>]) -> HashMap<u32, Expected> {
    let (first, rest) = maps.split_first().expect("at least one style pass");
    let mut merged = (*first).clone();
    for expectations in rest {
        let mut merge_lines: Vec<u32> = merged
            .keys()
            .copied()
            .chain(expectations.keys().copied())
            .collect();
        merge_lines.sort_unstable();
        merge_lines.dedup();
        for line in merge_lines {
            let accepts = match expectations.get(&line) {
                Some(expected) => std::iter::once((expected.primary, expected.primary_kinds))
                    .chain(expected.alternatives.iter().copied())
                    .collect::<Vec<_>>(),
                None => vec![(0, IndentKindSet::single(IndentKind::TopLevel))],
            };
            let entry = merged.entry(line).or_insert_with(Expected::top_level);
            for (column, kinds) in accepts {
                entry.add_alternative(column, kinds);
            }
        }
    }
    merged
}

fn expectations_for_style(
    root: Node<'_>,
    lines: &[&str],
    indent_unit: u32,
    infix_style: InfixContinuationStyle,
) -> HashMap<u32, Expected> {
    let mut expectations = HashMap::new();
    set_expectations(root, lines, indent_unit, infix_style, &mut expectations);
    expectations
}

fn set_expectations(
    root: Node<'_>,
    lines: &[&str],
    indent_unit: u32,
    infix_style: InfixContinuationStyle,
    out: &mut HashMap<u32, Expected>,
) {
    let mut changes: Vec<Change> = Vec::new();
    collect_changes(root, lines, infix_style, &mut changes);
    changes.sort_by_key(|c| c.token_byte);

    let line_count = lines.len();
    let mut primary = vec![0u32; line_count];
    let mut primary_kinds = vec![IndentKindSet::single(IndentKind::TopLevel); line_count];
    let mut alternatives: Vec<Vec<(u32, IndentKindSet)>> = vec![Vec::new(); line_count];

    for change in &changes {
        let begin = change.begin as usize;
        let end = (change.end as usize).min(line_count.saturating_sub(1));
        if begin > end {
            continue;
        }
        for line in begin..=end {
            apply_change_to_line(
                change,
                indent_unit,
                &mut primary[line],
                &mut primary_kinds[line],
                &mut alternatives[line],
            );
        }
    }

    for line in 0..line_count {
        if primary[line] != 0 || !alternatives[line].is_empty() {
            let mut expected = Expected::single(primary[line], primary_kinds[line]);
            for (column, kinds) in &alternatives[line] {
                expected.add_alternative(*column, *kinds);
            }
            out.insert(line as u32, expected);
        }
    }
}

/// Apply one indent change to one covered line's accumulated expectation —
/// the shared body of the whole-document fold ([`set_expectations`]) and the
/// per-target fold ([`expectations_for_targets`]), so the two cannot drift.
fn apply_change_to_line(
    change: &Change,
    indent_unit: u32,
    primary: &mut u32,
    primary_kinds: &mut IndentKindSet,
    alternatives: &mut Vec<(u32, IndentKindSet)>,
) {
    let current = *primary;
    match change.ty {
        ChangeType::Block(kind) => {
            *primary = current + indent_unit;
            if kind == IndentKind::Block {
                // Style-neutral levels (notably assignment RHS indentation)
                // compose with the axis that owns the surrounding context.
                primary_kinds.merge(IndentKindSet::single(kind));
            } else {
                *primary_kinds = IndentKindSet::single(kind);
            }
            for (column, _) in alternatives.iter_mut() {
                *column += indent_unit;
            }
        }
        ChangeType::Double(kind) => {
            *primary = current + 2 * indent_unit;
            *primary_kinds = IndentKindSet::single(kind);
            for (column, _) in alternatives.iter_mut() {
                *column += 2 * indent_unit;
            }
        }
        ChangeType::Hanging(col, kind) => {
            *primary = col;
            *primary_kinds = IndentKindSet::single(kind);
            alternatives.clear();
        }
        ChangeType::FlooredHanging(col, statement_indent, kind) => {
            *primary = col.max(statement_indent + indent_unit);
            *primary_kinds = IndentKindSet::single(kind);
            alternatives.clear();
        }
    }
    match change.alt {
        AltRule::None => {}
        // Only accept an absolute-column tolerance that sits to the
        // *right* of the primary — the aligned/chain-start styles are
        // deeper than the block/hanging primary. A column at or left
        // of the primary would legalize under-indented continuations
        // (`x <- (\n  a +\n  b\n)` must still flag `b` under the
        // default `Indented` style; `Either` accepts it via the
        // union of the two folds, not via this guard).
        AltRule::AlsoCol(col, kind) if col > *primary => {
            alternatives.push((col, IndentKindSet::single(kind)));
        }
        AltRule::AlsoCol(_, _) => {}
        AltRule::AlsoBlock(kind) => {
            alternatives.push((current + indent_unit, IndentKindSet::single(kind)));
        }
    }
}

/// How an indent change rewrites the covered lines' expectation.
#[derive(Clone, Copy)]
enum ChangeType {
    /// One `indent_unit` beyond the current expectation.
    Block(IndentKind),
    /// Two units (tidyverse double-indent function definitions).
    Double(IndentKind),
    /// An absolute column (content trails the opener and the closer trails
    /// content). Clears accumulated alternatives on the covered lines.
    Hanging(u32, IndentKind),
    /// An absolute chain-start column floored one unit beyond the owning
    /// statement's physical indent. Clears accumulated alternatives.
    FlooredHanging(u32, u32, IndentKind),
}

/// Raven's extra accepted values, layered over the lintr primary.
#[derive(Clone, Copy)]
enum AltRule {
    None,
    /// Accept this absolute column too (aligned-argument style), but only when
    /// it sits strictly deeper than the post-change primary.
    AlsoCol(u32, IndentKind),
    /// Accept one `indent_unit` over the pre-change expectation too (the
    /// block form where lintr demands hanging/double).
    AlsoBlock(IndentKind),
}

/// One indent-inducing token and the line range it governs.
struct Change {
    /// Byte offset of the inducing token — changes apply in document order.
    token_byte: usize,
    /// First covered line (inclusive).
    begin: u32,
    /// Last covered line (inclusive).
    end: u32,
    ty: ChangeType,
    alt: AltRule,
}

fn collect_changes(
    node: Node<'_>,
    lines: &[&str],
    infix_style: InfixContinuationStyle,
    out: &mut Vec<Change>,
) {
    collect_changes_bounded(node, lines, infix_style, None, out);
}

/// [`collect_changes`] with optional row bounds: `Some((min_row, max_row))`
/// prunes the traversal to subtrees that can still emit a change covering one
/// of those rows, so the per-line query (`accepted_indents_for_lines`) does
/// not walk the whole document AST on every Enter press. A subtree starting
/// below `max_row` never can — every change begins at its inducing token's
/// row plus one at the earliest. One ending above `min_row` can only reach
/// down via `operator_change`'s call-function extension (the change runs to
/// the *parent call's* end), so a child ending above `min_row` is skipped
/// unless it is that call's `function` field; everything deeper extends at
/// most to an ancestor within the skipped subtree.
fn collect_changes_bounded(
    node: Node<'_>,
    lines: &[&str],
    infix_style: InfixContinuationStyle,
    bounds: Option<(usize, usize)>,
    out: &mut Vec<Change>,
) {
    if let Some((_, max_row)) = bounds
        && node.start_position().row > max_row
    {
        return;
    }
    match node.kind() {
        "braced_expression" => bracket_change(node, node, lines, false, IndentKind::Block, out),
        "call" | "subset" | "subset2" => {
            if let Some(args) = node.child_by_field_name("arguments") {
                bracket_change(node, args, lines, false, IndentKind::ArgumentBlock, out);
            }
        }
        "function_definition" => {
            if let Some(params) = node.child_by_field_name("parameters") {
                bracket_change(node, params, lines, true, IndentKind::ArgumentBlock, out);
            }
            unbraced_body_change(node, "body", lines, out);
        }
        "parenthesized_expression" => {
            bracket_change(node, node, lines, false, IndentKind::ArgumentBlock, out)
        }
        "if_statement" | "while_statement" | "for_statement" => {
            bracket_change(node, node, lines, false, IndentKind::ArgumentBlock, out);
            let body_field = if node.kind() == "if_statement" {
                "consequence"
            } else {
                "body"
            };
            unbraced_body_change(node, body_field, lines, out);
            else_change(node, out);
        }
        "repeat_statement" => {
            if let (Some(keyword), Some(body)) = (node.child(0), node.child_by_field_name("body"))
                && body.start_position().row > keyword.start_position().row
                && body.kind() != "braced_expression"
            {
                out.push(Change {
                    token_byte: keyword.start_byte(),
                    begin: keyword.start_position().row as u32 + 1,
                    end: body.end_position().row as u32,
                    ty: ChangeType::Block(IndentKind::Block),
                    alt: AltRule::None,
                });
            }
        }
        "binary_operator" | "extract_operator" | "namespace_operator" => {
            operator_change(node, lines, infix_style, out);
        }
        "argument" | "parameter" => named_eq_change(node, out),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some((min_row, _)) = bounds
            && child.end_position().row < min_row
            && !(node.kind() == "call"
                && node
                    .child_by_field_name("function")
                    .is_some_and(|function| function.id() == child.id()))
        {
            continue;
        }
        collect_changes_bounded(child, lines, infix_style, bounds, out);
    }
}

/// Emit the indent change for a bracketed group. `owner` is the syntactic
/// construct (used for the double-indent test); `bracket` is the node holding
/// the `open`/`close` fields.
fn bracket_change(
    owner: Node<'_>,
    bracket: Node<'_>,
    lines: &[&str],
    is_parameters: bool,
    block_kind: IndentKind,
    out: &mut Vec<Change>,
) {
    let Some(open) = bracket.child_by_field_name("open") else {
        return;
    };
    let Some(close) = bracket.child_by_field_name("close") else {
        return;
    };
    let open_row = open.start_position().row as u32;
    let close_row = close.start_position().row as u32;
    if open_row >= close_row {
        return;
    }
    // lintr's double-indent avoidance: when a same-line sibling expression
    // spans multiple lines and itself contains an end-of-line opener
    // (`foo(bar(`, `map(x, function(y) {`), only the inner bracket indents.
    if sibling_content_has_eol_opener(bracket, open, lines) {
        return;
    }

    let Some(prev) = close.prev_sibling() else {
        return;
    };
    if prev.id() == open.id() {
        // Empty group (`(\n)`) — nothing to indent.
        return;
    }
    let begin = open_row + 1;
    let end = prev.end_position().row as u32;
    if begin > end {
        return;
    }

    let closer_on_own_line = {
        let text = line_text(lines, close_row);
        let col = close.start_position().column;
        text.get(..col)
            .is_some_and(|prefix| prefix.chars().all(char::is_whitespace))
    };
    let opener_line_text = line_text(lines, open_row);
    let after_opener = opener_line_text
        .get(open.end_position().column..)
        .unwrap_or("");
    // A trailing `#` comment after the opener doesn't count as content — the
    // aligned-style tolerance needs a real code argument to align to.
    let has_content_after_opener = first_non_whitespace_is_code(after_opener);
    let opener_end_col = char_col(lines, open_row, open.end_position().column);

    // Tidyverse double-indent definitions: `function(` with no parameter on
    // the `function` line and the closer trailing the last parameter.
    let double = is_parameters
        && !has_content_after_opener
        && !closer_on_own_line
        && owner.start_position().row as u32 == open_row;

    // lintr's "suppress" case: an outer call that merely wraps an inner call
    // starting on the same line and running to the closer adds no indent
    // level of its own (`outer_fun(inner_fun(x,\n  arg\n))` — only the inner
    // call indents).
    let prev_content = if prev.kind() == "argument" {
        prev.child_by_field_name("value").unwrap_or(prev)
    } else {
        prev
    };
    if !closer_on_own_line
        && prev_content.kind() == "call"
        && prev_content.start_position().row as u32 == open_row
        && prev_content.end_position().row as u32 == close_row
        && close_row > open_row
    {
        return;
    }

    let (ty, alt) = if double {
        (
            ChangeType::Double(block_kind),
            AltRule::AlsoBlock(block_kind),
        )
    } else if closer_on_own_line {
        let alt = if has_content_after_opener {
            AltRule::AlsoCol(opener_end_col, IndentKind::OpenerAligned)
        } else {
            AltRule::None
        };
        (ChangeType::Block(block_kind), alt)
    } else {
        (
            ChangeType::Hanging(opener_end_col, IndentKind::OpenerAligned),
            AltRule::AlsoBlock(block_kind),
        )
    };

    out.push(Change {
        token_byte: open.start_byte(),
        begin,
        end,
        ty,
        alt,
    });
}

/// True when a sibling content node starting on the opener's row spans
/// multiple rows and contains a bracket opener token at end of line.
fn sibling_content_has_eol_opener(bracket: Node<'_>, open: Node<'_>, lines: &[&str]) -> bool {
    let open_row = open.start_position().row;
    let mut cursor = bracket.walk();
    bracket.children(&mut cursor).any(|child| {
        child.start_byte() > open.start_byte()
            && child.start_position().row == open_row
            && child.end_position().row > open_row
            && subtree_has_eol_opener(child, lines)
    })
}

fn subtree_has_eol_opener(node: Node<'_>, lines: &[&str]) -> bool {
    if matches!(node.kind(), "(" | "[" | "[[" | "{") {
        let row = node.start_position().row;
        let rest = line_text(lines, row as u32)
            .get(node.end_position().column..)
            .unwrap_or("");
        if !first_non_whitespace_is_code(rest) {
            return true;
        }
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| subtree_has_eol_opener(child, lines))
}

/// Physical indent of the expression that owns an operator chain.
///
/// Program and brace children are statements. A call argument or parameter is
/// likewise a statement-like boundary for indentation: a chain beginning on a
/// new argument line gets its floor from that line. Parentheses, unary
/// operators, assignments, and nested binary nodes are transparent, so a
/// chain inside `x <- !(...)` retains `x`'s statement indent rather than
/// treating the first operand's already-indented line as a second floor.
fn owning_statement_indent(node: Node<'_>, lines: &[&str]) -> u32 {
    let mut owner = node;
    while let Some(parent) = owner.parent() {
        if matches!(
            parent.kind(),
            "program" | "braced_expression" | "argument" | "parameter"
        ) {
            break;
        }
        owner = parent;
    }
    leading_space_count(line_text(lines, owner.start_position().row as u32))
}

/// Emit the continuation change for an infix operator whose right-hand side
/// starts on a later line, subject to the `assignment_as_infix` suppression.
///
/// Non-assignment operators honor `InfixContinuationStyle`: `Indented` keeps
/// only the strict lintr-compatible block bump, while `Aligned` pins the
/// continuation to the chain-start column with a one-level statement floor.
/// `Either` never reaches this function — `set_expectations` folds it as the
/// union of the two single-style passes. Assignment operators are exempt and
/// always use the `Indented` shape: their RHS is not a peer operand of the
/// LHS, so "align with the preceding operand" has no meaning there —
/// `Aligned` would otherwise demand `x <-\n  a` put `a` in the assignment
/// target's column. (`suppressed_as_assignment_rhs` cannot cover this: it
/// suppresses operators *nested under* an assignment, not the assignment
/// node itself.)
fn operator_change(
    node: Node<'_>,
    lines: &[&str],
    infix_style: InfixContinuationStyle,
    out: &mut Vec<Change>,
) {
    let Some(op) = node.child_by_field_name("operator") else {
        return;
    };
    let Some(rhs) = node.child_by_field_name("rhs") else {
        return;
    };
    if rhs.start_position().row <= op.end_position().row {
        return;
    }
    if suppressed_as_assignment_rhs(node) {
        return;
    }
    // When the operator's RHS is immediately called (`http_head(url)$\n
    // then(...)`), the continuation level covers the whole call, not just
    // the callee name — lintr extends the range the same way.
    let mut end = rhs.end_position().row as u32;
    if let Some(parent) = node.parent()
        && parent.kind() == "call"
        && parent
            .child_by_field_name("function")
            .is_some_and(|f| f.id() == node.id())
    {
        end = parent.end_position().row as u32;
    }
    // The chain-start column is floored one unit beyond the physical indent
    // of the owning statement. This keeps statement-level continuations one
    // level in while preserving deeper RHS/paren alignment.
    let chain_row = node.start_position().row as u32;
    let chain_col = char_col(lines, chain_row, node.start_position().column);
    let statement_indent = owning_statement_indent(node, lines);
    let is_assignment = matches!(op.kind(), "<-" | "<<-" | "=" | ":=" | "->" | "->>");
    let effective_style = if is_assignment {
        InfixContinuationStyle::Indented
    } else {
        infix_style
    };
    let (ty, alt) = match effective_style {
        InfixContinuationStyle::Indented if is_assignment => {
            (ChangeType::Block(IndentKind::Block), AltRule::None)
        }
        InfixContinuationStyle::Indented => {
            (ChangeType::Block(IndentKind::InfixBlock), AltRule::None)
        }
        InfixContinuationStyle::Aligned => (
            ChangeType::FlooredHanging(chain_col, statement_indent, IndentKind::ChainStart),
            AltRule::None,
        ),
        InfixContinuationStyle::Either => {
            unreachable!("set_expectations folds Either as the union of Indented and Aligned")
        }
    };
    out.push(Change {
        token_byte: op.start_byte(),
        begin: op.end_position().row as u32 + 1,
        end,
        ty,
        alt,
    });
}

/// Emit the continuation change for a named argument / formal default whose
/// `=` ends its line (lintr's EQ_SUB / EQ_FORMALS infix changes).
fn named_eq_change(node: Node<'_>, out: &mut Vec<Change>) {
    let mut cursor = node.walk();
    let Some(eq) = node.children(&mut cursor).find(|child| child.kind() == "=") else {
        return;
    };
    let Some(value) = node
        .child_by_field_name("value")
        .or_else(|| node.child_by_field_name("default"))
    else {
        return;
    };
    if value.start_position().row <= eq.end_position().row {
        return;
    }
    if suppressed_as_assignment_rhs(node) {
        return;
    }
    out.push(Change {
        token_byte: eq.start_byte(),
        begin: eq.end_position().row as u32 + 1,
        end: value.end_position().row as u32,
        ty: ChangeType::Block(IndentKind::Block),
        alt: AltRule::None,
    });
}

/// lintr's `assignment_as_infix = TRUE` suppression: an infix continuation
/// adds no second indent level when it sits on the right-hand side of an
/// assignment whose operator ends its line. Walking up, a call argument or a
/// braced block "restores" the context (stops the search); a parenthesized
/// expression does not.
fn suppressed_as_assignment_rhs(node: Node<'_>) -> bool {
    let mut child = node;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            "binary_operator" => {
                let is_rhs = parent
                    .child_by_field_name("rhs")
                    .is_some_and(|rhs| rhs.id() == child.id());
                if is_rhs
                    && let Some(op) = parent.child_by_field_name("operator")
                    && matches!(op.kind(), "<-" | "<<-" | "=" | ":=")
                    && parent
                        .child_by_field_name("rhs")
                        .is_some_and(|rhs| rhs.start_position().row > op.end_position().row)
                {
                    return true;
                }
                // An assignment operator not at end of line is neither a
                // suppressor nor a restorer — keep walking.
            }
            "argument" | "parameter" => {
                // A named argument whose `=` ends its line suppresses; any
                // other argument is a call boundary that restores.
                let mut cursor = parent.walk();
                let eq = parent.children(&mut cursor).find(|c| c.kind() == "=");
                let value = parent
                    .child_by_field_name("value")
                    .or_else(|| parent.child_by_field_name("default"));
                return match (eq, value) {
                    (Some(eq), Some(value)) if value.id() == child.id() => {
                        value.start_position().row > eq.end_position().row
                    }
                    _ => false,
                };
            }
            "braced_expression" => return false,
            _ => {}
        }
        child = parent;
    }
    false
}

/// Emit the change for an unbraced control-flow or function body: the `)` (or
/// parameter-list `)`) ends its line and the body is not a braced block.
fn unbraced_body_change(node: Node<'_>, body_field: &str, lines: &[&str], out: &mut Vec<Change>) {
    let close = node.child_by_field_name("close").or_else(|| {
        node.child_by_field_name("parameters")
            .and_then(|params| params.child_by_field_name("close"))
    });
    let Some(close) = close else {
        return;
    };
    let Some(body) = node.child_by_field_name(body_field) else {
        return;
    };
    if body.kind() == "braced_expression" {
        return;
    }
    let close_row = close.start_position().row;
    if body.start_position().row <= close_row {
        return;
    }
    let _ = lines;
    out.push(Change {
        token_byte: close.start_byte(),
        begin: close_row as u32 + 1,
        end: body.end_position().row as u32,
        ty: ChangeType::Block(IndentKind::Block),
        alt: AltRule::None,
    });
}

/// Emit the change for an `else` keyword that ends its line.
fn else_change(node: Node<'_>, out: &mut Vec<Change>) {
    if node.kind() != "if_statement" {
        return;
    }
    let Some(alternative) = node.child_by_field_name("alternative") else {
        return;
    };
    let mut cursor = node.walk();
    let Some(else_kw) = node.children(&mut cursor).find(|c| c.kind() == "else") else {
        return;
    };
    if alternative.kind() == "braced_expression"
        || alternative.start_position().row <= else_kw.start_position().row
    {
        return;
    }
    out.push(Change {
        token_byte: else_kw.start_byte(),
        begin: else_kw.start_position().row as u32 + 1,
        end: alternative.end_position().row as u32,
        ty: ChangeType::Block(IndentKind::Block),
        alt: AltRule::None,
    });
}

/// Collect line numbers that start strictly inside a multi-line string or
/// backtick-quoted identifier. For a node spanning rows `[r1, r2]` with
/// `r2 > r1`, lines `r1 + 1 ..= r2` start inside literal content and are
/// skipped by the linter. Plain identifiers cannot span rows, so every
/// multiline `identifier` node is backtick-quoted.
fn collect_string_interior_lines(node: Node<'_>, set: &mut HashSet<u32>) {
    if matches!(node.kind(), "string" | "identifier") {
        let start = node.start_position().row as u32;
        let end = node.end_position().row as u32;
        if end > start {
            for line in (start + 1)..=end {
                set.insert(line);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_string_interior_lines(child, set);
    }
}

/// Character column for a tree-sitter *byte* column on `line`. Expected
/// indents are measured in characters (`leading_space_count`), so absolute
/// columns fed into `Hanging`/`AlsoCol` must be converted — a non-ASCII
/// character before the opener would otherwise shift the expectation.
fn char_col(lines: &[&str], line: u32, byte_col: usize) -> u32 {
    let text = line_text(lines, line);
    match text.get(..byte_col) {
        // A leading BOM (line 0 only) is invisible and must not count —
        // `leading_space_count` never sees it either.
        Some(prefix) => strip_leading_bom_for_scan(prefix).chars().count() as u32,
        None => byte_col as u32,
    }
}

fn line_text<'a>(lines: &'a [&'a str], line: u32) -> &'a str {
    lines.get(line as usize).copied().unwrap_or("")
}

pub(crate) fn leading_space_count(line: &str) -> u32 {
    line.chars().take_while(|c| *c == ' ').count() as u32
}

fn has_tab_in_leading(line: &str) -> bool {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .any(|c| c == '\t')
}

/// True when the line's first non-whitespace character is `#`, with a leading
/// BOM ignored the same way as the comment collector.
fn is_standalone_comment_line(line: &str) -> bool {
    strip_leading_bom_for_scan(line)
        .trim_start()
        .starts_with('#')
}

/// True when the first non-whitespace character in `s` is a code token rather
/// than a `#` that starts a comment. Used to decide whether the opener of a
/// bracketed group carries real content on its line — `foo( # note` should
/// be treated as `foo(`, not as `foo(a`.
fn first_non_whitespace_is_code(s: &str) -> bool {
    match s.chars().find(|c| !c.is_whitespace()) {
        None => false,
        Some('#') => false,
        Some(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser_pool::with_parser;

    /// Lint under explicit strict `Indented` semantics — the lintr-parity
    /// reference used by pre-existing tests, not the user-facing default.
    fn lint(text: &str, indent_unit: u32) -> Vec<Diagnostic> {
        lint_with_style(text, indent_unit, InfixContinuationStyle::Indented)
    }

    fn lint_with_style(
        text: &str,
        indent_unit: u32,
        infix_style: InfixContinuationStyle,
    ) -> Vec<Diagnostic> {
        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        let suppressions = crate::linting::nolint::Suppressions::from_text(text);
        let mut out = Vec::new();
        collect(
            text,
            tree.root_node(),
            indent_unit,
            infix_style,
            DiagnosticSeverity::HINT,
            &suppressions,
            &mut out,
        );
        out
    }

    fn diagnostic_on_line(diags: &[Diagnostic], line: u32) -> Option<&Diagnostic> {
        diags.iter().find(|diag| diag.range.start.line == line)
    }

    #[test]
    fn multiline_backtick_identifier_interior_is_not_indentation() {
        let diagnostics = lint("x <- `a\n  b`\n", 2);
        assert!(
            diagnostic_on_line(&diagnostics, 1).is_none(),
            "backtick identifier content must not be linted as indentation: {diagnostics:?}"
        );
    }

    fn line_expectation(
        text: &str,
        indent_unit: u32,
        style: InfixContinuationStyle,
        line: u32,
    ) -> LineIndentExpectation {
        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        accepted_indents_for_line(text, tree.root_node(), indent_unit, style, line)
    }

    fn alternative_has_kind(
        expected: &LineIndentExpectation,
        column: u32,
        kind: IndentKind,
    ) -> bool {
        expected
            .alternatives
            .iter()
            .any(|(candidate, kinds)| *candidate == column && kinds.contains(kind))
    }

    /// The old unpruned per-line semantics, reconstructed from the
    /// whole-document diagnostic fold: what `accepted_indents_for_lines`
    /// must keep matching after traversal pruning.
    fn whole_document_expectation(
        root: Node<'_>,
        lines: &[&str],
        indent_unit: u32,
        infix_style: InfixContinuationStyle,
        line: u32,
    ) -> LineIndentExpectation {
        let styles: &[InfixContinuationStyle] = match infix_style {
            InfixContinuationStyle::Either => &[
                InfixContinuationStyle::Indented,
                InfixContinuationStyle::Aligned,
            ],
            InfixContinuationStyle::Indented => &[InfixContinuationStyle::Indented],
            InfixContinuationStyle::Aligned => &[InfixContinuationStyle::Aligned],
        };
        let mut merged: Option<Expected> = None;
        for &style in styles {
            let mut map = expectations_for_style(root, lines, indent_unit, style);
            let expected = map.remove(&line).unwrap_or_else(Expected::top_level);
            match &mut merged {
                None => merged = Some(expected),
                Some(existing) => {
                    existing.add_alternative(expected.primary, expected.primary_kinds);
                    for (column, kinds) in expected.alternatives {
                        existing.add_alternative(column, kinds);
                    }
                }
            }
        }
        let merged = merged.expect("at least one style pass");
        LineIndentExpectation {
            primary: merged.primary,
            primary_kinds: merged.primary_kinds,
            alternatives: merged.alternatives,
        }
    }

    #[test]
    fn per_line_query_matches_whole_document_fold_on_every_line() {
        // Structurally rich shapes: nested calls, double-indent definitions,
        // operator chains (including the call-function extension that lets a
        // change reach its parent call's end), unbraced bodies, `else`,
        // `repeat`, walrus subsets, and named arguments.
        let sources = [
            "f <- function(\n    x,\n    y) {\n  a <- x %>%\n    g() %>%\n    h(1,\n      2)\n  \
             if (a)\n    b <- 2\n  else\n    c(3,\n      4)\n}\n",
            "res <- http_head(url)$\n  then(function(x) {\n    x + 1\n  })\n",
            "dt[, y :=\n     z]\nrepeat\n  f(\n    1)\nresult <- a +\n  b\n",
        ];
        for source in sources {
            let tree = with_parser(|p| p.parse(source, None)).expect("test source must parse");
            let lines: Vec<&str> = source.lines().collect();
            for style in [
                InfixContinuationStyle::Indented,
                InfixContinuationStyle::Aligned,
                InfixContinuationStyle::Either,
            ] {
                for line in 0..lines.len() as u32 {
                    let queried =
                        accepted_indents_for_line(source, tree.root_node(), 2, style, line);
                    let reference =
                        whole_document_expectation(tree.root_node(), &lines, 2, style, line);
                    assert_eq!(
                        queried, reference,
                        "pruned per-line query diverged on line {line} ({style:?}) of \
                         {source:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn line_expectation_tags_block_and_opener_aligned_columns() {
        let text = "foo(a,\n  b\n)\n";
        let expected = line_expectation(text, 2, InfixContinuationStyle::Indented, 1);
        assert_eq!(expected.primary, 2);
        assert_eq!(
            expected.alternatives,
            vec![(4, IndentKindSet::single(IndentKind::OpenerAligned))]
        );

        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        let lines: Vec<&str> = text.lines().collect();
        let folded = expectations_for_style(
            tree.root_node(),
            &lines,
            2,
            InfixContinuationStyle::Indented,
        );
        assert_eq!(
            folded[&1].primary_kinds,
            IndentKindSet::single(IndentKind::ArgumentBlock)
        );
    }

    #[test]
    fn line_expectation_tags_hanging_primary_block_alternative() {
        let expected = line_expectation("foo(a,\n  b)\n", 2, InfixContinuationStyle::Indented, 1);
        assert_eq!(expected.primary, 4);
        assert_eq!(
            expected.alternatives,
            vec![(2, IndentKindSet::single(IndentKind::ArgumentBlock))]
        );
    }

    #[test]
    fn strict_indented_has_no_chain_start_tolerance() {
        let strict = line_expectation(
            "result <- foo() +\n  bar()\n",
            2,
            InfixContinuationStyle::Indented,
            1,
        );
        assert_eq!(strict.primary, 2);
        assert!(strict.alternatives.is_empty());
        assert!(strict.primary_kinds.contains(IndentKind::InfixBlock));

        let either = line_expectation(
            "result <- foo() +\n  bar()\n",
            2,
            InfixContinuationStyle::Either,
            1,
        );
        assert!(alternative_has_kind(&either, 10, IndentKind::ChainStart));
    }

    #[test]
    fn line_expectation_assignment_continuation_has_no_alternatives() {
        let expected = line_expectation(
            "result <-\n  f(x)\n",
            2,
            InfixContinuationStyle::Indented,
            1,
        );
        assert_eq!(expected.primary, 2);
        assert!(expected.alternatives.is_empty());
    }

    #[test]
    fn line_expectation_double_indent_has_block_alternative() {
        let expected = line_expectation(
            "f <- function(\n    x) x\n",
            2,
            InfixContinuationStyle::Indented,
            1,
        );
        assert_eq!(expected.primary, 4);
        assert_eq!(
            expected.alternatives,
            vec![(2, IndentKindSet::single(IndentKind::ArgumentBlock))]
        );
    }

    #[test]
    fn line_expectation_uncovered_line_is_top_level_without_alternatives() {
        let expected = line_expectation("x <- 1\n", 2, InfixContinuationStyle::Indented, 0);
        assert_eq!(
            expected,
            LineIndentExpectation {
                primary: 0,
                primary_kinds: IndentKindSet::single(IndentKind::TopLevel),
                alternatives: Vec::new(),
            }
        );
    }

    #[test]
    fn line_expectation_either_unions_values_and_preserves_demoted_primary_kind() {
        let text = "result <- foo() +\n  bar()\n";
        let indented = line_expectation(text, 2, InfixContinuationStyle::Indented, 1);
        let aligned = line_expectation(text, 2, InfixContinuationStyle::Aligned, 1);
        let either = line_expectation(text, 2, InfixContinuationStyle::Either, 1);

        let mut singles = vec![indented.primary, aligned.primary];
        singles.extend(indented.alternatives.iter().map(|(column, _)| *column));
        singles.extend(aligned.alternatives.iter().map(|(column, _)| *column));
        singles.sort_unstable();
        singles.dedup();

        let mut union = vec![either.primary];
        union.extend(either.alternatives.iter().map(|(column, _)| *column));
        union.sort_unstable();
        union.dedup();

        assert_eq!(union, singles);
        assert_eq!(either.primary, indented.primary);
        assert!(alternative_has_kind(&either, 10, IndentKind::ChainStart));
    }

    #[test]
    fn line_expectation_either_merges_equal_floored_and_indented_columns() {
        let expected = line_expectation("data +\n  value\n", 2, InfixContinuationStyle::Either, 1);
        assert_eq!(expected.primary, 2);
        assert!(expected.alternatives.is_empty());
        assert!(expected.primary_kinds.contains(IndentKind::InfixBlock));
        assert!(expected.primary_kinds.contains(IndentKind::ChainStart));
    }

    #[test]
    fn expectation_tags_do_not_change_messages_or_structure() {
        let mut expected = Expected::single(2, IndentKindSet::single(IndentKind::Block));
        expected.add_alternative(4, IndentKindSet::single(IndentKind::OpenerAligned));
        assert_eq!(
            expected.message(1),
            "Indentation should be 2 or 4 spaces, not 1."
        );

        let mut differently_tagged =
            Expected::single(2, IndentKindSet::single(IndentKind::ChainStart));
        differently_tagged.add_alternative(4, IndentKindSet::single(IndentKind::Block));
        assert!(expected.has_same_structure_as(&differently_tagged));
    }

    /// Builds a snippet the way the on-type indenter would: starting from
    /// the first line, each subsequent content line is placed at the column
    /// the real judge-backed on-type pipeline computes for an Enter press at
    /// that point. Compatible same-named lint styles and `Either` must accept
    /// the resulting columns; mismatched strict styles may intentionally flag.
    fn build_with_auto_indent(first: &str, rest: &[&str]) -> String {
        build_with_auto_indent_for_style(first, rest, InfixContinuationStyle::Indented)
    }

    fn build_with_auto_indent_for_style(
        first: &str,
        rest: &[&str],
        infix_style: InfixContinuationStyle,
    ) -> String {
        use crate::indentation::{IndentationConfig, IndentationStyle, on_type_indentation};
        use tower_lsp::lsp_types::Position;

        let emit_style = match infix_style {
            InfixContinuationStyle::Indented => IndentationStyle::Indented,
            InfixContinuationStyle::Aligned | InfixContinuationStyle::Either => {
                IndentationStyle::Aligned
            }
        };
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            enabled: true,
            argument_style: IndentationStyle::Aligned,
            infix_continuation_style: emit_style,
        };
        let mut text = format!("{first}\n");
        for content in rest {
            let line = text.lines().count() as u32;
            let tree = with_parser(|p| p.parse(&text, None)).expect("parse must succeed");
            let col = on_type_indentation(&tree, &text, Position { line, character: 0 }, &config)
                .expect("the producer/judge invariant fixtures must be answerable");
            text.push_str(&" ".repeat(col as usize));
            text.push_str(content);
            text.push('\n');
        }
        text
    }

    #[test]
    fn pass9_earlier_openers_use_judge_column_and_round_trip_clean() {
        for style in ALL_STYLES {
            for (first, assignment) in [
                ("f(", "b <-"),
                ("(", "x <-"),
                ("x[", "a <-"),
                ("f(a,", "b <-"),
            ] {
                let text = build_with_auto_indent_for_style(first, &[assignment, "value"], style);
                let value_line = text.lines().nth(2).expect("value line must exist");
                assert_eq!(
                    value_line, "    value",
                    "earlier opener shape {first:?} must indent the assignment RHS to column 4 under {style:?}"
                );
                assert!(
                    lint_with_style(&text, 2, style).is_empty(),
                    "facade output for earlier opener shape {first:?} must round-trip clean under {style:?}; got {:?}",
                    lint_with_style(&text, 2, style)
                );
            }
        }
    }

    #[test]
    fn issue611_assignment_rhs_one_level_in_every_mode() {
        // The issue's motivating table, rows 1–2: one level after a broken
        // assignment is clean everywhere; column 0 (the pre-fix auto-indent
        // output) is flagged everywhere.
        for style in ALL_STYLES {
            assert!(
                lint_with_style("result <-\n  f(x)\n", 2, style).is_empty(),
                "one-level assignment RHS must be clean under {style:?}"
            );
            assert_eq!(
                lint_with_style("result <-\nf(x)\n", 2, style).len(),
                1,
                "column-0 assignment RHS must be flagged under {style:?}"
            );
        }
    }

    #[test]
    fn issue611_broken_assignment_chain_is_flattened_in_every_mode() {
        // Rows 3–4: a chain under a broken assignment gets no second level.
        for style in ALL_STYLES {
            assert!(
                lint_with_style("result <-\n  data %>%\n  filter(x)\n", 2, style).is_empty(),
                "flattened chain must be clean under {style:?}"
            );
            assert!(
                !lint_with_style("result <-\n  data %>%\n    filter(x)\n", 2, style).is_empty(),
                "double-indented chain must be flagged under {style:?}"
            );
        }
    }

    #[test]
    fn issue611_chained_assignments_flatten_in_every_mode() {
        for style in ALL_STYLES {
            assert!(
                lint_with_style("a <-\n  b <-\n  c\n", 2, style).is_empty(),
                "chained broken assignments must be clean at one level under {style:?}"
            );
            assert!(
                !lint_with_style("a <-\n  b <-\n    c\n", 2, style).is_empty(),
                "chained broken assignments must not stack levels under {style:?}"
            );
        }
    }

    #[test]
    fn issue611_walrus_rhs_one_level_in_every_mode() {
        for style in ALL_STYLES {
            assert!(
                lint_with_style("{\n  x :=\n    v\n}\n", 2, style).is_empty(),
                "`:=` RHS at one level must be clean under {style:?}"
            );
            assert!(
                !lint_with_style("{\n  x :=\n  v\n}\n", 2, style).is_empty(),
                "un-indented `:=` RHS must be flagged under {style:?}"
            );
        }
    }

    #[test]
    fn issue611_paren_wrapped_rhs_chain_stays_flattened() {
        // Parens don't restore the assignment context: a chain inside `(`
        // on a broken assignment's RHS still gets no second level. This is
        // the completed form of the indenter's paren-skipping fallback walk
        // (`text_flattened_under_assignment`), whose output columns (4 for
        // the `(`, 6 for the chain) must be judge-approved.
        let text = "f <- function() {\n  a <-\n    (\n      data %>%\n      g()\n    )\n}\n";
        for style in ALL_STYLES {
            assert!(
                lint_with_style(text, 2, style).is_empty(),
                "paren-wrapped flattened chain must be clean under {style:?}; got {:?}",
                lint_with_style(text, 2, style)
            );
        }
    }

    #[test]
    fn issue611_same_line_condition_chain_keeps_hanging_level() {
        // A chain inside `if (`/`while (` condition parens under a broken
        // assignment: the condition's bracket level survives the
        // assignment suppression — the indenter's formula output (6 for
        // `if`, 9 for `while`) is accepted; the bare line indent (2) is
        // flagged in every mode.
        for style in ALL_STYLES {
            for clean in [
                "a <-\n  if (data %>%\n      g()) 1\n",
                "a <-\n  while (data %>%\n         g()) 1\n",
            ] {
                assert!(
                    lint_with_style(clean, 2, style).is_empty(),
                    "{clean:?} must be clean under {style:?}"
                );
            }
            for flagged in [
                "a <-\n  if (data %>%\n  g()) 1\n",
                "a <-\n  while (data %>%\n  g()) 1\n",
            ] {
                assert!(
                    !lint_with_style(flagged, 2, style).is_empty(),
                    "bare line indent {flagged:?} must be flagged under {style:?}"
                );
            }
        }
    }

    #[test]
    fn issue611_same_line_statement_body_chain_is_flattened() {
        // A chain in a same-row `if` BODY has no bracket level: the
        // flattened column (2) is the only accepted shape.
        for style in ALL_STYLES {
            assert!(
                lint_with_style("a <-\n  if (x) data %>%\n  g()\n", 2, style).is_empty(),
                "flattened if-body chain must be clean under {style:?}"
            );
            assert!(
                !lint_with_style("a <-\n  if (x) data %>%\n    g()\n", 2, style).is_empty(),
                "indented if-body chain must be flagged under {style:?}"
            );
        }
    }

    #[test]
    fn issue611_same_line_paren_chain_keeps_hanging_level() {
        // `a <-` ⏎ `  (data %>%` ⏎: the paren opened on the chain's own
        // line contributes a hanging level the assignment suppression does
        // not remove — the indenter's formula output (4) and the hanging
        // column (3) are accepted; the bare line indent (2, what naive
        // flattening would produce) is flagged in every mode.
        for style in ALL_STYLES {
            for clean in [
                "a <-\n  (data %>%\n    g())\n",
                "a <-\n  (data %>%\n   g())\n",
                "a <-\n  (!data %>%\n    g())\n",
            ] {
                assert!(
                    lint_with_style(clean, 2, style).is_empty(),
                    "{clean:?} must be clean under {style:?}"
                );
            }
            for flagged in [
                "a <-\n  (data %>%\n  g())\n",
                "a <-\n  (!data %>%\n  g())\n",
            ] {
                assert!(
                    !lint_with_style(flagged, 2, style).is_empty(),
                    "bare line indent {flagged:?} must be flagged under {style:?}"
                );
            }
        }
    }

    #[test]
    fn issue611_comment_interrupted_chain_target_at_statement_level() {
        // A comment-only line inside the chain does not move the statement
        // start: the `->` target belongs at the chain's block level (4),
        // matching the judge's comment-transparent chain handling.
        // Under `Aligned` the chain lines themselves are flagged (aligned
        // chains sit at the chain-start column — #610 semantics, unrelated
        // to the target), so assert on the target line only there.
        let text = "f <- function() {\n  data %>%\n    # explanation\n    g() ->\n    target\n}\n";
        for style in [
            InfixContinuationStyle::Indented,
            InfixContinuationStyle::Either,
        ] {
            assert!(
                lint_with_style(text, 2, style).is_empty(),
                "comment-interrupted chain target must be clean under {style:?}; got {:?}",
                lint_with_style(text, 2, style)
            );
        }
        let aligned = lint_with_style(text, 2, InfixContinuationStyle::Aligned);
        assert!(
            diagnostic_on_line(&aligned, 4).is_none(),
            "target line must not be flagged under Aligned; got {aligned:?}"
        );
    }

    #[test]
    fn issue611_right_assignment_target_at_statement_level() {
        // `data %>%` ⏎ `  f() ->` ⏎: the target belongs one level from the
        // statement start (column 2), not two. Under `Aligned` the chain
        // line itself is flagged (aligned-mode chain semantics, #610 —
        // unrelated to the assignment target), so assert on the target line
        // only there.
        let text = "data %>%\n  f() ->\n  target\n";
        for style in [
            InfixContinuationStyle::Indented,
            InfixContinuationStyle::Either,
        ] {
            assert!(
                lint_with_style(text, 2, style).is_empty(),
                "right-assignment target at statement level must be clean under {style:?}"
            );
        }
        let aligned = lint_with_style(text, 2, InfixContinuationStyle::Aligned);
        assert!(
            diagnostic_on_line(&aligned, 2).is_none(),
            "target line must not be flagged under Aligned; got {aligned:?}"
        );
        // Two levels on the target line is flagged where the chain shape
        // itself is accepted (under Aligned the diagnostic lands on the
        // chain line instead, so the target line carries no separate flag).
        for style in [
            InfixContinuationStyle::Indented,
            InfixContinuationStyle::Either,
        ] {
            let deep = lint_with_style("data %>%\n  f() ->\n    target\n", 2, style);
            assert!(
                diagnostic_on_line(&deep, 2).is_some(),
                "double-indented target must be flagged under {style:?}"
            );
        }
    }

    #[test]
    fn issue611_assignment_in_call_args_paren_alignment_accepted() {
        // Inside call arguments the indenter defers to paren alignment
        // (issue #611's `=` decision, applied to every assignment operator);
        // the judge accepts the operator-hanging and block shapes here.
        // Pins why the deferral is safe: this shape has lint-accepted forms
        // that are NOT one-level-from-the-assignment-line.
        for style in ALL_STYLES {
            assert!(
                lint_with_style(
                    "long_function_name(x <-\n                     c)\n",
                    2,
                    style
                )
                .is_empty(),
                "operator-hanging RHS inside call args must be clean under {style:?}"
            );
        }
    }

    #[test]
    fn issue611_auto_indent_output_is_never_flagged() {
        // Producer→judge round trip: build each snippet through the real
        // indentation pipeline, then assert the linter accepts the result.
        for style in ALL_STYLES {
            let built_cases: [(String, &str); 3] = [
                (
                    build_with_auto_indent_for_style("result <-", &["f(x)"], style),
                    "plain assignment",
                ),
                (
                    build_with_auto_indent_for_style(
                        "result <-",
                        &["data %>%", "filter(x)"],
                        style,
                    ),
                    "broken-assignment pipe chain",
                ),
                (
                    format!(
                        "{}}}\n",
                        build_with_auto_indent_for_style(
                            "f <- function() {",
                            &["x <-", "value"],
                            style,
                        )
                    ),
                    "assignment in function body",
                ),
            ];
            for (text, name) in &built_cases {
                assert!(
                    lint_with_style(text, 2, style).is_empty(),
                    "auto-indent output for {name} must be clean under {style:?}; \
                     built {text:?}, got {:?}",
                    lint_with_style(text, 2, style)
                );
            }
        }

        // Right assignment after a chain: clean under Indented/Either; under
        // Aligned only the chain line is flagged (#610 aligned-mode chain
        // semantics), never the auto-indented target line.
        let text = build_with_auto_indent("data %>%", &["f() ->", "target"]);
        assert_eq!(text, "data %>%\n  f() ->\n  target\n");
        for style in [
            InfixContinuationStyle::Indented,
            InfixContinuationStyle::Either,
        ] {
            assert!(
                lint_with_style(&text, 2, style).is_empty(),
                "auto-indent output for right assignment must be clean under {style:?}"
            );
        }
        let aligned = lint_with_style(&text, 2, InfixContinuationStyle::Aligned);
        assert!(
            diagnostic_on_line(&aligned, 2).is_none(),
            "auto-indented target line must not be flagged under Aligned; got {aligned:?}"
        );
    }

    #[test]
    fn function_body_correctly_indented_passes() {
        let text = "f <- function() {\n  x <- 1\n}\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn function_body_underindented_flagged() {
        let text = "f <- function() {\nx <- 1\n}\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 2 spaces"));
    }

    #[test]
    fn function_body_overindented_flagged() {
        let text = "f <- function() {\n    x <- 1\n}\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 2 spaces"));
    }

    #[test]
    fn nested_braces_each_level_one_unit_deeper() {
        let text = "{\n  if (x) {\n    y <- 1\n  }\n}\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn nested_braces_inner_wrong_flagged() {
        let text = "{\n  if (x) {\n  y <- 1\n  }\n}\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.line, 2);
    }

    #[test]
    fn closing_brace_aligned_with_opener() {
        let text = "{\n  x <- 1\n  }\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.line, 2);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn continuation_after_binary_operator() {
        let text = "x <- 1 +\n  2\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn continuation_underindented_flagged() {
        let text = "x <- 1 +\n2\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.line, 1);
    }

    #[test]
    fn pipe_continuation_indented() {
        let text = "x |>\n  f()\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn multi_line_call_hanging_indent_passes() {
        let text = "foo(\n  a,\n  b\n)\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn multi_line_call_closing_paren_aligned() {
        let text = "foo(\n  a,\n  b\n  )\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.line, 3);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn multi_line_call_aligned_with_first_arg_accepted() {
        let text = "foo(a,\n    b)\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn multi_line_call_hanging_when_opener_alone_accepted() {
        let text = "foo(\n  a\n)\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn multi_line_call_misaligned_flagged() {
        let text = "foo(a,\n  b)\n";
        // 2 is the hanging alternative (opener_indent + unit); 4 is aligned.
        // Both are acceptable, so no diagnostic.
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn multi_line_call_wrong_indent_flagged() {
        let text = "foo(a,\n b)\n";
        // 1 is neither aligned (4) nor hanging (2).
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("2 or 4"));
    }

    #[test]
    fn if_else_block_passes() {
        let text = "if (x) {\n  a\n} else {\n  b\n}\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn multi_line_string_skipped() {
        let text = "x <- \"hello\nworld\"\n";
        // Line 1 starts inside the string; should not be flagged.
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn line_with_tab_in_indent_skipped() {
        let text = "f <- function() {\n\tx <- 1\n}\n";
        // Line 1 uses a tab — no_tab handles it; indentation rule stays silent.
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn suppression_nolint_silences_diagnostic() {
        let text = "f <- function() {\nx <- 1 # nolint\n}\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn suppression_lsp_ignore_next_silences_diagnostic() {
        // The marker comment must itself be at the correct indent — the
        // `# @lsp-ignore-next` only suppresses the *following* source line, so
        // a marker placed at column 0 inside a braced block would (correctly)
        // be flagged on its own line.
        let text = "f <- function() {\n  # @lsp-ignore-next\nx <- 1\n}\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn top_level_lines_expect_zero_indent() {
        let text = "  x <- 1\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start.line, 0);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn comment_aligned_with_trailing_comment_column_not_flagged() {
        let text = concat!(
            "source(\"scripts/functions.r\")   # functions to perform the analysis\n",
            "source(\"scripts/folders.r\")     # make sure output directories exist\n",
            "source(\"scripts/data.r\")        # process the data\n",
            "                                # store the data in a list named `ww`\n",
            "ww$sc.oos <- 1                  # for country jackknife validations\n",
        );
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn comment_not_aligned_with_any_trailing_comment_still_flagged() {
        let text = "          # arbitrary standalone note\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 0);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn comment_inside_block_genuinely_misindented_still_flagged() {
        let text = "f <- function() {\n# note\n}\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 2 spaces"));
    }

    #[test]
    fn comment_inside_block_aligned_with_trailing_comment_in_same_block_not_flagged() {
        let gap = " ".repeat(6);
        let aligned = " ".repeat(14);
        let text = format!("f <- function() {{\n  x <- 1{gap}# note\n{aligned}# more\n}}\n");
        assert!(lint(&text, 2).is_empty());
    }

    #[test]
    fn comment_aligned_across_different_scopes_not_exempted() {
        let text = "if (x) { # note\n         # same column, different expected indent\n}\n";
        let diags = lint(text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 2 spaces"));
    }

    #[test]
    fn directive_marker_comment_not_exempted_by_alignment() {
        let anchor_gap = " ".repeat(14);
        let aligned = " ".repeat(20);
        let text = format!("x <- 1{anchor_gap}# note\n{aligned}# @lsp-ignore-next\ny <- 1\n");
        let diags = lint(&text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn anchor_on_suppressed_line_does_not_count() {
        let anchor_gap = " ".repeat(14);
        let aligned = " ".repeat(20);
        let text =
            format!("x <- 1{anchor_gap}# nolint\n{aligned}# aligned only to suppressed line\n");
        let diags = lint(&text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn multi_line_comment_run_below_one_anchor_all_exempted() {
        let anchor_gap = " ".repeat(14);
        let aligned = " ".repeat(20);
        let text = format!(
            "x <- 1{anchor_gap}# note\n{aligned}# first\n{aligned}# second\n{aligned}# third\n"
        );
        assert!(lint(&text, 2).is_empty());
    }

    #[test]
    fn prose_comment_starting_with_nolint_prefix_is_not_a_directive_marker() {
        // "nolinter" is prose, not the `# nolint` directive; it must still be
        // usable as a real trailing-comment anchor.
        let anchor_gap = " ".repeat(14);
        let aligned = " ".repeat(20);
        let text = format!("x <- 1{anchor_gap}# nolinter is not a keyword\n{aligned}# aligned\n");
        assert!(lint(&text, 2).is_empty(), "got {:?}", lint(&text, 2));
    }

    #[test]
    fn multibyte_prefix_before_trailing_comment_aligns_by_character_column() {
        // "résumé" contributes two 2-byte UTF-8 characters before the trailing
        // comment, so the anchor's byte column (15) differs from its character
        // column (13). The standalone comment below is visually aligned at
        // character column 13 (pure ASCII spaces, so its byte and character
        // columns coincide) and must be exempted — matching must use the
        // character column, not the byte column.
        let text = "résumé <- 1  # note\n             # aligned reprise\n";
        assert!(lint(text, 2).is_empty(), "got {:?}", lint(text, 2));
    }

    #[test]
    fn doubled_hash_directive_marker_not_exempted_by_alignment() {
        // `nolint.rs::classify` recognizes `## @lsp-ignore-next` (repeated
        // `#`) as a real directive, so the indentation rule's own marker
        // detection must match that normalization — otherwise a misindented
        // marker line could dodge its own indent check just because it
        // happens to align with a nearby trailing comment.
        let anchor_gap = " ".repeat(14);
        let aligned = " ".repeat(20);
        let text = format!("x <- 1{anchor_gap}# note\n{aligned}## @lsp-ignore-next\ny <- 1\n");
        let diags = lint(&text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn uppercase_directive_marker_not_exempted_by_alignment() {
        // `nolint.rs::classify` lowercases before matching, so `# @LSP-IGNORE-NEXT`
        // is a real directive; the indentation rule's marker detection must
        // fold case the same way, or an uppercase marker could dodge its own
        // indent check by aligning with a nearby trailing comment. Uses the
        // `-next` form (rather than bare `# NOLINT`) because it doesn't
        // suppress its own line, so the diagnostic — or lack of it — is
        // actually observable here (a self-suppressing marker's own line is
        // always skipped by suppression, independent of this rule).
        let anchor_gap = " ".repeat(14);
        let aligned = " ".repeat(20);
        let text = format!("x <- 1{anchor_gap}# note\n{aligned}# @LSP-IGNORE-NEXT\ny <- 1\n");
        let diags = lint(&text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn raven_marker_with_space_before_colon_not_exempted_by_alignment() {
        // `nolint.rs::classify_raven` tolerates whitespace between `raven`
        // and `:` (e.g. `# raven : ignore-next`); the indentation rule's
        // marker detection must recognize that spelling too, or such a
        // misindented marker could dodge its own indent check by aligning
        // with a nearby trailing comment.
        let anchor_gap = " ".repeat(14);
        let aligned = " ".repeat(20);
        let text = format!("x <- 1{anchor_gap}# note\n{aligned}# raven : ignore-next\ny <- 1\n");
        let diags = lint(&text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn structural_lsp_directive_not_exempted_by_alignment() {
        // The `@lsp-` namespace covers far more than lint suppression
        // (`@lsp-ignore`/`@lsp-expect`) — cross-file directive aliases like
        // `@lsp-source` are just as much "directive markers" that must never
        // be exempted by comment-alignment, or a misindented one could dodge
        // its own indent check by coincidentally lining up with a trailing
        // comment.
        let anchor_gap = " ".repeat(14);
        let aligned = " ".repeat(20);
        let text =
            format!("x <- 1{anchor_gap}# note\n{aligned}# @lsp-source \"helpers.R\"\ny <- 1\n");
        let diags = lint(&text, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 0 spaces"));
    }

    #[test]
    fn empty_braced_block_no_diagnostics() {
        let text = "f <- function() {\n}\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn blank_lines_inside_block_not_flagged() {
        let text = "f <- function() {\n\n  x\n}\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn configurable_indent_unit_four() {
        let text = "f <- function() {\n    x <- 1\n}\n";
        assert!(lint(text, 4).is_empty());

        let wrong = "f <- function() {\n  x <- 1\n}\n";
        let diags = lint(wrong, 4);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("should be 4 spaces"));
    }

    #[test]
    fn either_accepts_chain_start_and_strict_indented_columns() {
        let aligned = "result <- foo() +\n          bar()\n";
        let indented = "result <- foo() +\n  bar()\n";
        assert!(lint_with_style(aligned, 2, InfixContinuationStyle::Either).is_empty());
        assert!(lint_with_style(indented, 2, InfixContinuationStyle::Either).is_empty());
        assert!(
            !lint_with_style(aligned, 2, InfixContinuationStyle::Indented).is_empty(),
            "strict indented must reject the former deeper-only tolerance"
        );
    }

    #[test]
    fn comment_after_opener_does_not_unlock_aligned_style() {
        // `foo( # note` opens like `foo(` — only the hanging form (2) is
        // allowed; alignment to column after the opener (5) is not, because
        // there's no code argument on the opener line to align to.
        let hanging_ok = "foo( # note\n  a\n)\n";
        let aligned_bad = "foo( # note\n     a\n)\n";
        assert!(lint(hanging_ok, 2).is_empty());
        let diags = lint(aligned_bad, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
    }

    #[test]
    fn function_definition_parameters_hanging_indent() {
        let text = "f <- function(\n  x,\n  y\n) {\n  x + y\n}\n";
        assert!(lint(text, 2).is_empty());
    }

    #[test]
    fn function_definition_parameters_misindented_flagged() {
        let text = "f <- function(\nx,\n  y\n) {\n  x + y\n}\n";
        let diags = lint(text, 2);
        // Only the `x,` line is mis-indented (0 instead of 2).
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
    }

    #[test]
    fn parenthesized_expression_hanging_indent_passes() {
        // Plain `(expr)` (parenthesized_expression, not a call) routes through
        // the same set_bracketed path as call arguments. Inner content hangs
        // one unit beyond the line of `(`; the inner `a + b` continuation
        // adds another unit on top of that.
        let text = "x <- (\n  a +\n    b\n)\n";
        assert!(lint(text, 2).is_empty(), "got {:?}", lint(text, 2));
    }

    #[test]
    fn parenthesized_expression_inner_misindented_flagged() {
        // `b` continues `a +` and must indent one unit beyond `a`'s line
        // (line indent 2 → expected 4). Writing it at 2 is wrong.
        let text = "x <- (\n  a +\n  b\n)\n";
        let diags = lint(text, 2);
        assert!(
            diags.iter().any(|d| d.range.start.line == 2),
            "expected continuation diagnostic on line 2; got {:?}",
            diags
        );
    }

    #[test]
    fn parenthesized_expression_closing_paren_aligns_with_opener_line() {
        // The closing `)` on its own line aligns with the line of the `(`.
        let aligned = "x <- (\n  a\n)\n";
        let misaligned = "x <- (\n  a\n  )\n";
        assert!(lint(aligned, 2).is_empty());
        let diags = lint(misaligned, 2);
        assert!(
            diags.iter().any(|d| d.range.start.line == 2),
            "expected misalignment diagnostic on line 2; got {:?}",
            diags
        );
    }

    #[test]
    fn parenthesized_binary_closer_aligns_at_top_level() {
        let text = "changed <- !(\n    (is.na(v_prev) & is.na(v_upd)) |\n    (!is.na(v_prev) & !is.na(v_upd) & v_prev == v_upd)\n)\n";
        let diags = lint(text, 4);

        assert!(
            diagnostic_on_line(&diags, 3).is_none(),
            "expected no diagnostic on closing paren line; got {:?}",
            diags
        );
        let operand_diag = diagnostic_on_line(&diags, 2)
            .expect("expected continuation diagnostic on second operand line");
        assert!(
            operand_diag.message.contains("8"),
            "expected message to mention 8 spaces; got {:?}",
            operand_diag
        );
    }

    #[test]
    fn parenthesized_binary_closer_aligns_inside_nested_functions() {
        let text = "outer <- function() {\n    inner <- function() {\n        changed <- !(\n            (is.na(v_prev) & is.na(v_upd)) |\n            (!is.na(v_prev) & !is.na(v_upd) & v_prev == v_upd)\n        )\n    }\n}\n";
        let diags = lint(text, 4);

        assert!(
            diagnostic_on_line(&diags, 5).is_none(),
            "expected no diagnostic on closing paren line; got {:?}",
            diags
        );
        let operand_diag = diagnostic_on_line(&diags, 4)
            .expect("expected continuation diagnostic on second operand line");
        assert!(
            operand_diag.message.contains("16"),
            "expected message to mention 16 spaces; got {:?}",
            operand_diag
        );
    }

    #[test]
    fn either_accepts_mixed_pipe_and_arithmetic_forms() {
        let indented = "x <- f() |>\n  g() + y +\n  z\n";
        let aligned = "x <- f() |>\n     g() + y +\n     z\n";
        for text in [indented, aligned] {
            assert!(
                lint_with_style(text, 2, InfixContinuationStyle::Either).is_empty(),
                "Either must accept {text:?}"
            );
        }
        assert!(
            !lint_with_style(aligned, 2, InfixContinuationStyle::Indented).is_empty(),
            "strict indented must reject chain-start alignment"
        );
    }

    #[test]
    fn function_definition_parameters_closing_paren_aligns_with_function() {
        // The closing `)` of the parameter list sits on its own line and
        // should align with the line of the `function` keyword.
        let aligned = "f <- function(\n  x\n) {\n  x\n}\n";
        let misaligned = "f <- function(\n  x\n  ) {\n  x\n}\n";
        assert!(lint(aligned, 2).is_empty());
        let diags = lint(misaligned, 2);
        assert!(
            diags.iter().any(|d| d.range.start.line == 2),
            "expected diagnostic on line 2 (the misindented `)`); got {:?}",
            diags
        );
    }

    #[test]
    fn multi_line_if_condition_hangs_like_a_bracketed_group() {
        // `if (` / `while (` conditions indent like any other bracketed
        // group (issue #600); real lintr accepts these shapes.
        let if_ok = "if (\n  a &&\n    b\n) {\n  x <- 1\n}\n";
        assert!(lint(if_ok, 2).is_empty(), "got {:?}", lint(if_ok, 2));

        let while_ok = "while (\n  a &&\n    b\n) {\n  y <- 2\n}\n";
        assert!(lint(while_ok, 2).is_empty(), "got {:?}", lint(while_ok, 2));

        // Under-indented condition lines are still flagged.
        let bad = "if (\na\n) {\n  x <- 1\n}\n";
        let diags = lint(bad, 2);
        assert_eq!(diags.len(), 1, "got {:?}", diags);
        assert_eq!(diags[0].range.start.line, 1);
        assert!(diags[0].message.contains("should be 2 spaces"));
    }

    #[test]
    fn multi_line_if_condition_closer_aligns_with_if_line() {
        // The `)` closing a multi-line condition aligns with the `if` line.
        let misaligned = "if (\n  a\n  ) {\n  x <- 1\n}\n";
        let diags = lint(misaligned, 2);
        assert!(
            diags.iter().any(|d| d.range.start.line == 2),
            "expected diagnostic on the misindented `)`; got {:?}",
            diags
        );
    }

    #[test]
    fn hanging_columns_measure_characters_not_bytes() {
        // `é` is 2 bytes, 1 char. The hanging column after `foo(` is the
        // 9th *character*; real lintr accepts this layout (verified).
        let text = "\u{e9} <- foo(a,\n         b)\n";
        assert!(lint(text, 2).is_empty(), "got {:?}", lint(text, 2));
    }

    #[test]
    fn multi_line_for_clause_indents_like_a_bracketed_group() {
        // A `for` clause spanning lines: the nested call's closer at column 0
        // must win over the for-clause's inner-line expectation (descendant
        // writes overwrite ancestors in the pre-order walk).
        let text = "for (i in seq_len(\n  n\n)) {\n  z <- 3\n}\n";
        assert!(lint(text, 2).is_empty(), "got {:?}", lint(text, 2));
    }

    // ----- infixContinuationStyle (#605) -----

    const ALL_STYLES: [InfixContinuationStyle; 3] = [
        InfixContinuationStyle::Indented,
        InfixContinuationStyle::Aligned,
        InfixContinuationStyle::Either,
    ];

    /// The issue's motivating shape (unit 4): peer operands aligned at the
    /// enclosing paren's own indent.
    const MOTIVATING_ALIGNED: &str =
        "changed <- !(\n    first_condition |\n    second_condition\n)\n";
    /// Same shape with the lintr-style extra continuation level.
    const MOTIVATING_INDENTED: &str =
        "changed <- !(\n    first_condition |\n        second_condition\n)\n";

    #[test]
    fn aligned_style_requires_chain_start_column_for_mid_line_chain() {
        // The chain starts at `foo()` (column 10). `Aligned` requires exactly
        // that column and flags the block-indented form `Indented` demands.
        let aligned = "result <- foo() +\n          bar()\n";
        let hanging = "result <- foo() +\n  bar()\n";
        assert!(
            lint_with_style(aligned, 2, InfixContinuationStyle::Aligned).is_empty(),
            "chain-start alignment must pass under Aligned"
        );
        let diags = lint_with_style(hanging, 2, InfixContinuationStyle::Aligned);
        assert_eq!(diags.len(), 1, "got {diags:?}");
        assert_eq!(diags[0].range.start.line, 1);
        assert!(
            diags[0].message.contains("10"),
            "expected message to demand column 10; got {:?}",
            diags[0]
        );
    }

    #[test]
    fn aligned_style_motivating_boolean_shape() {
        assert!(
            lint_with_style(MOTIVATING_ALIGNED, 4, InfixContinuationStyle::Aligned).is_empty(),
            "got {:?}",
            lint_with_style(MOTIVATING_ALIGNED, 4, InfixContinuationStyle::Aligned)
        );
        let diags = lint_with_style(MOTIVATING_INDENTED, 4, InfixContinuationStyle::Aligned);
        let diag = diagnostic_on_line(&diags, 2).expect("block-indented operand must be flagged");
        assert!(
            diag.message.contains('4'),
            "expected message to demand column 4; got {diag:?}"
        );
    }

    #[test]
    fn aligned_style_three_peer_operands_share_one_column() {
        // Left-associative `|` produces two operator nodes that both start at
        // `first`, so every peer operand pins to the same column.
        let text = "changed <- !(\n    first |\n    second |\n    third\n)\n";
        assert!(
            lint_with_style(text, 4, InfixContinuationStyle::Aligned).is_empty(),
            "got {:?}",
            lint_with_style(text, 4, InfixContinuationStyle::Aligned)
        );
    }

    #[test]
    fn aligned_style_floors_statement_level_chains() {
        for (clean, flush, line, expected) in [
            ("x |>\n  y\n", "x |>\ny\n", 1, "2"),
            ("{\n  x |>\n    y\n}\n", "{\n  x |>\n  y\n}\n", 2, "4"),
        ] {
            assert!(
                lint_with_style(clean, 2, InfixContinuationStyle::Aligned).is_empty(),
                "floored aligned form must pass: {clean:?}"
            );
            let diags = lint_with_style(flush, 2, InfixContinuationStyle::Aligned);
            let diag = diagnostic_on_line(&diags, line).expect("flush continuation is flagged");
            assert!(diag.message.contains(expected), "got {diag:?}");
        }
    }

    #[test]
    fn aligned_style_mixed_chain_pins_to_shared_chain_start() {
        // Same shape as the Indented-mode tolerance test above: under
        // Aligned the chain-start column is the only accepted form.
        let aligned = "x <- f() |>\n     g() + y +\n     z\n";
        let hanging = "x <- f() |>\n  g() + y +\n  z\n";
        assert!(
            lint_with_style(aligned, 2, InfixContinuationStyle::Aligned).is_empty(),
            "got {:?}",
            lint_with_style(aligned, 2, InfixContinuationStyle::Aligned)
        );
        assert!(!lint_with_style(hanging, 2, InfixContinuationStyle::Aligned).is_empty());
    }

    #[test]
    fn aligned_style_subchain_starting_on_later_line() {
        // `|>` binds tighter than `+`, so `b |> c` is a subchain whose own
        // chain start is `b`'s written position. With every operand aligned
        // at the paren's indent, both operator pins agree on column 2.
        let text = "x <- (\n  a +\n  b |>\n  c\n)\n";
        assert!(
            lint_with_style(text, 2, InfixContinuationStyle::Aligned).is_empty(),
            "got {:?}",
            lint_with_style(text, 2, InfixContinuationStyle::Aligned)
        );
    }

    #[test]
    fn aligned_style_right_associative_extract_chain() {
        // The statement-level floor applies to the outer and inner extract
        // nodes, so every continued operand shares column 2.
        let text = "obj$\n  field$\n  value\n";
        assert!(
            lint_with_style(text, 2, InfixContinuationStyle::Aligned).is_empty(),
            "got {:?}",
            lint_with_style(text, 2, InfixContinuationStyle::Aligned)
        );
    }

    #[test]
    fn aligned_style_extract_call_extension_composes_with_inner_bracket() {
        // The `$` change is extended over the whole following call; the
        // call's own bracket Block then stacks one unit on top of the pin.
        let ok = "http_head(url)$\n  then(\n    x\n  )\n";
        assert!(
            lint_with_style(ok, 2, InfixContinuationStyle::Aligned).is_empty(),
            "got {:?}",
            lint_with_style(ok, 2, InfixContinuationStyle::Aligned)
        );
        let bad = "http_head(url)$\nthen(\n  x\n)\n";
        assert!(
            !lint_with_style(bad, 2, InfixContinuationStyle::Aligned).is_empty(),
            "a callee left of the statement floor must be flagged under Aligned"
        );
    }

    #[test]
    fn aligned_style_clears_bracket_tolerances_on_shared_lines() {
        // `x <- (a |` opens a hanging group (primary column 6, block form 2
        // accepted as a tolerance). The `|` pin under Aligned clears those
        // and leaves only the chain-start column 6.
        let aligned = "x <- (a |\n      b)\n";
        assert!(
            lint_with_style(aligned, 2, InfixContinuationStyle::Aligned).is_empty(),
            "got {:?}",
            lint_with_style(aligned, 2, InfixContinuationStyle::Aligned)
        );
        let block_form = "x <- (a |\n  b)\n";
        assert!(
            !lint_with_style(block_form, 2, InfixContinuationStyle::Aligned).is_empty(),
            "the bracket's block tolerance must not survive an Aligned pin"
        );
    }

    #[test]
    fn either_style_accepts_both_motivating_forms() {
        for text in [MOTIVATING_ALIGNED, MOTIVATING_INDENTED] {
            assert!(
                lint_with_style(text, 4, InfixContinuationStyle::Either).is_empty(),
                "Either must accept {text:?}; got {:?}",
                lint_with_style(text, 4, InfixContinuationStyle::Either)
            );
        }
    }

    #[test]
    fn either_style_accepts_operand_at_paren_indent() {
        // Flagged under Indented (see
        // `parenthesized_expression_inner_misindented_flagged`); the Aligned
        // pass accepts the chain-start column at the paren's own indent.
        let text = "x <- (\n  a +\n  b\n)\n";
        assert!(
            lint_with_style(text, 2, InfixContinuationStyle::Either).is_empty(),
            "got {:?}",
            lint_with_style(text, 2, InfixContinuationStyle::Either)
        );
    }

    #[test]
    fn under_indented_block_still_flagged_on_chain_start_line() {
        // Both operands sit at column 0 inside a paren expecting 4. The
        // chain-start line carries no aligned alternative, so it stays
        // flagged in every style; the continuation aligned with it inherits
        // its (wrong) column under Aligned/Either — the cascade a Hanging
        // anchor always produces — but the code as a whole is never clean.
        let text = "changed <- !(\nfirst_condition |\nsecond_condition\n)\n";
        for style in ALL_STYLES {
            let diags = lint_with_style(text, 4, style);
            assert!(
                diagnostic_on_line(&diags, 1).is_some(),
                "chain-start line must stay flagged under {style:?}; got {diags:?}"
            );
        }
    }

    #[test]
    fn either_style_applies_floor_when_chain_starts_on_opener_line() {
        // The raw chain start is column 1, but the owning statement starts at
        // column 0, so aligned and Either both require the one-level floor.
        let text = "(a +\n  b\n)\n";
        for style in [
            InfixContinuationStyle::Aligned,
            InfixContinuationStyle::Either,
        ] {
            assert!(
                lint_with_style(text, 2, style).is_empty(),
                "got {:?} under {style:?}",
                lint_with_style(text, 2, style)
            );
        }
    }

    #[test]
    fn either_style_accepts_nested_mixed_precedence_aligned_chain() {
        // Stacked operators (`|>` binds tighter than `+`) with every operand
        // aligned at the paren's indent: clean under Aligned, so the Either
        // union must accept it too.
        let text = "x <- (\n  a +\n  b |>\n  c\n)\n";
        assert!(
            lint_with_style(text, 2, InfixContinuationStyle::Either).is_empty(),
            "got {:?}",
            lint_with_style(text, 2, InfixContinuationStyle::Either)
        );
    }

    #[test]
    fn either_style_honors_aligned_comment_exemptions_per_pass() {
        // The aligned-comment exemption groups runs by comparing each line's
        // `Expected` value. Under Aligned the operand and comment lines all
        // expect exactly {2}, so the standalone comment aligned with the
        // anchor's trailing-comment column is exempt and the snippet is
        // clean. A merged Either map has structurally different per-line
        // values, breaking the run — so exemptions must be computed per
        // pass, with a line accepted when any pass accepts or exempts it.
        let text = "x <- (\n  a +\n  b +   # anchor\n        # standalone\n  c\n)\n";
        for style in [
            InfixContinuationStyle::Aligned,
            InfixContinuationStyle::Either,
        ] {
            assert!(
                lint_with_style(text, 2, style).is_empty(),
                "got {:?} under {style:?}",
                lint_with_style(text, 2, style)
            );
        }
    }

    #[test]
    fn either_style_accepts_bracket_tolerances_inside_aligned_continuations() {
        // A bracket group nested inside an operator continuation: its
        // tolerances (aligned-argument `AlsoCol`, hanging-with-`AlsoBlock`)
        // are functions of the fold's primary, which differs between the
        // aligned pin and the indented block path. Only folding Either as
        // the union of the two passes keeps these clean — a per-alternative
        // encoding flagged both (the review counterexamples).
        let call_in_paren = "x <- (\n  a +\n  fo(k,\n     q\n  )\n)\n";
        let hanging_call = "x <- foo() +\n     bar(a,\n       b)\n";
        for text in [call_in_paren, hanging_call] {
            for style in [
                InfixContinuationStyle::Aligned,
                InfixContinuationStyle::Either,
            ] {
                assert!(
                    lint_with_style(text, 2, style).is_empty(),
                    "{text:?} must be clean under {style:?}; got {:?}",
                    lint_with_style(text, 2, style)
                );
            }
        }
    }

    #[test]
    fn either_style_is_superset_of_both_other_styles() {
        // Everything clean under Indented or Aligned must be clean under
        // Either. Snippets cover the shapes the two strict styles disagree
        // on; acceptance (not diagnostic-line identity) is compared because
        // run-coalescing can move a diagnostic's anchor line. Some snippets
        // also appear in named tests above — the overlap is deliberate: the
        // named tests pin a specific shape's verdict, this sweep pins the
        // cross-mode lattice.
        let snippets = [
            MOTIVATING_ALIGNED,
            MOTIVATING_INDENTED,
            "result <- foo() +\n  bar()\n",
            "result <- foo() +\n          bar()\n",
            "data |>\nf()\n",
            "data |>\n  f()\n",
            "x <- (\n  a +\n  b |>\n  c\n)\n",
            "x <- (\n  a +\n    b |>\n      c\n)\n",
            "(a +\n b\n)\n",
            "obj$\nfield$\nvalue\n",
            "changed <- !(\nfirst_condition |\nsecond_condition\n)\n",
            "x <- (\n  a +\n  fo(k,\n     q\n  )\n)\n",
            "x <- foo() +\n     bar(a,\n       b)\n",
            "x <- (\n  a +\n  b +   # anchor\n        # standalone\n  c\n)\n",
            "x <- f() |>\n     g() + y +\n     z\n",
            "x <- f() |>\n  g() + y +\n  z\n",
        ];
        for text in snippets {
            for style in [
                InfixContinuationStyle::Indented,
                InfixContinuationStyle::Aligned,
            ] {
                if lint_with_style(text, 2, style).is_empty() {
                    assert!(
                        lint_with_style(text, 2, InfixContinuationStyle::Either).is_empty(),
                        "{text:?} is clean under {style:?} but flagged under Either: {:?}",
                        lint_with_style(text, 2, InfixContinuationStyle::Either)
                    );
                }
            }
        }
    }

    #[test]
    fn assignment_continuations_are_style_independent() {
        // Assignment operators are exempt: `Aligned` must not demand the RHS
        // sit in the assignment target's column. The block-indented RHS is
        // the only accepted form in every mode, and the
        // `assignment_as_infix` flattening is untouched.
        for style in ALL_STYLES {
            for text in ["x <-\n  a\n", "x <-\n  a +\n  b\n", "x =\n  a\n"] {
                assert!(
                    lint_with_style(text, 2, style).is_empty(),
                    "{text:?} must be clean under {style:?}; got {:?}",
                    lint_with_style(text, 2, style)
                );
            }
            let flat = "x <-\na\n";
            let diags = lint_with_style(flat, 2, style);
            assert_eq!(
                diags.len(),
                1,
                "un-indented assignment RHS must be flagged under {style:?}; got {diags:?}"
            );
        }
    }

    #[test]
    fn aligned_break_after_assignment_keeps_all_rhs_operands_mutually_aligned() {
        let text = "x <-\n    y +\n    z +\n    w\n";
        for style in ALL_STYLES {
            assert!(
                lint_with_style(text, 4, style).is_empty(),
                "assignment flattening must keep y/z/w at column 4 under {style:?}"
            );
        }
        let lhs_aligned = "x <-\ny +\nz +\nw\n";
        for style in ALL_STYLES {
            assert!(
                !lint_with_style(lhs_aligned, 4, style).is_empty(),
                "the LHS column must not replace the one-level assignment floor"
            );
        }
    }

    #[test]
    fn bracket_and_named_eq_shapes_are_style_independent() {
        // Shapes with no eligible infix continuation must produce identical
        // diagnostics in every mode: pure brackets, named-argument `=`
        // continuations, and a namespace `::` split (invalid R that only
        // error-recovers — the style must not change how it's judged).
        let snippets = [
            "foo(\n  a,\n  b\n)\n",
            "foo(\na\n)\n",
            "f(a =\n  1)\n",
            "val <- pkg::\n  fun\n",
        ];
        for text in snippets {
            let baseline = lint_with_style(text, 2, InfixContinuationStyle::Indented);
            for style in ALL_STYLES {
                assert_eq!(
                    lint_with_style(text, 2, style),
                    baseline,
                    "{text:?} must lint identically under {style:?}"
                );
            }
        }
    }
}
