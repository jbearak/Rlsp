//! Flag lines whose leading whitespace doesn't match the expected indent.
//!
//! Mirrors `lintr::indentation_linter()` with the default "tidy" hanging-indent
//! style, using lintr's accumulated "indent change" model: indent-inducing
//! tokens (bracket openers, end-of-line infix operators, `else`/`repeat`,
//! unbraced bodies) are collected in document order and each rewrites its
//! covered lines' expectation relative to the *current* accumulated value.
//! The full semantics live on [`set_expectations`]. Verified against a
//! 112-case differential corpus vs real lintr; the deliberate differences
//! are all lenient (Raven additionally accepts the aligned-argument style,
//! the block form where lintr demands hanging/double indents, and the
//! chain-start column the on-type formatter produces — so Raven never flags
//! code those styles produce, while every primary expectation matches
//! lintr's).
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
use crate::linting::nolint::{Suppressions, matches_keyword};
use crate::linting::rule_ids;
use crate::utf16::strip_leading_bom_for_scan;

pub(crate) fn collect(
    text: &str,
    root: Node<'_>,
    indent_unit: u32,
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

    let mut expectations: HashMap<u32, Expected> = HashMap::new();
    set_expectations(root, &lines, indent_unit, &mut expectations);

    let line_states: Vec<LineState> = lines
        .iter()
        .enumerate()
        .map(|(idx, line_text)| {
            LineState::new(idx as u32, line_text, suppressions, &string_interior)
        })
        .collect();

    let mut comments: HashMap<u32, CommentCol> = HashMap::new();
    collect_comment_cols(root, &lines, &mut comments);

    let aligned_comment_exemptions =
        collect_aligned_comment_exemptions(&lines, &expectations, &comments, &line_states);

    // lintr suppresses consecutive lints with the same indentation
    // difference — one diagnostic per run of equally mis-indented lines.
    let mut last_bad: Option<(u32, i64)> = None;

    for (idx, line_text) in lines.iter().enumerate() {
        let line_no = idx as u32;
        if line_states[idx].skips_indentation_check() {
            continue;
        }

        let actual = leading_space_count(line_text);
        let expected = expectations
            .get(&line_no)
            .cloned()
            .unwrap_or_else(Expected::top_level);

        if expected.is_acceptable(actual) {
            continue;
        }

        if aligned_comment_exemptions.contains(&line_no) && is_standalone_comment_line(line_text) {
            continue;
        }

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
    alternatives: Vec<u32>,
}

impl Expected {
    fn single(value: u32) -> Self {
        Self {
            primary: value,
            alternatives: Vec::new(),
        }
    }

    fn top_level() -> Self {
        Self::single(0)
    }

    fn is_acceptable(&self, actual: u32) -> bool {
        actual == self.primary || self.alternatives.contains(&actual)
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

        let mut left = self.alternatives.clone();
        left.sort_unstable();
        left.dedup();
        let mut right = other.alternatives.clone();
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
                .chain(self.alternatives.iter().copied())
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
///   second level, so `x <-\n  a +\n  b` is flat.
/// * `else` / `repeat` at end of line and an unbraced control-flow or
///   function body (`)` at end of line with no `{` following) cover the body
///   lines as `Block`.
///
/// Raven layers documented tolerances on top as alternative accepted values:
/// aligned-with-opener style for argument lists whose opener carries content
/// (`foo(a,\n    b\n)`), the block form where lintr demands hanging (and one
/// unit where lintr demands two), and the chain-start column for operator
/// continuations so the linter never disagrees with the on-type formatter.
/// These only ever *add* accepted values, so Raven flags a subset of what
/// lintr flags on these shapes; primaries match lintr.
fn set_expectations(
    root: Node<'_>,
    lines: &[&str],
    indent_unit: u32,
    out: &mut HashMap<u32, Expected>,
) {
    let mut changes: Vec<Change> = Vec::new();
    collect_changes(root, lines, &mut changes);
    changes.sort_by_key(|c| c.token_byte);

    let line_count = lines.len();
    let mut primary = vec![0u32; line_count];
    let mut alternatives: Vec<Vec<u32>> = vec![Vec::new(); line_count];

    for change in &changes {
        let begin = change.begin as usize;
        let end = (change.end as usize).min(line_count.saturating_sub(1));
        if begin > end {
            continue;
        }
        for line in begin..=end {
            let current = primary[line];
            match change.ty {
                ChangeType::Block => {
                    primary[line] = current + indent_unit;
                    for alt in &mut alternatives[line] {
                        *alt += indent_unit;
                    }
                }
                ChangeType::Double => {
                    primary[line] = current + 2 * indent_unit;
                    for alt in &mut alternatives[line] {
                        *alt += 2 * indent_unit;
                    }
                }
                ChangeType::Hanging(col) => {
                    primary[line] = col;
                    alternatives[line].clear();
                }
            }
            match change.alt {
                AltRule::None => {}
                // Only accept an absolute-column tolerance that sits to the
                // *right* of the primary — the aligned/chain-start styles are
                // deeper than the block/hanging primary. A column at or left
                // of the primary would legalize under-indented continuations
                // (`x <- (\n  a +\n  b\n)` must still flag `b`).
                AltRule::AlsoCol(col) if col > primary[line] => alternatives[line].push(col),
                AltRule::AlsoCol(_) => {}
                AltRule::AlsoBlock => alternatives[line].push(current + indent_unit),
            }
        }
    }

    for line in 0..line_count {
        if primary[line] != 0 || !alternatives[line].is_empty() {
            let mut expected = Expected::single(primary[line]);
            for &alt in &alternatives[line] {
                if alt != expected.primary && !expected.alternatives.contains(&alt) {
                    expected.alternatives.push(alt);
                }
            }
            out.insert(line as u32, expected);
        }
    }
}

/// How an indent change rewrites the covered lines' expectation.
#[derive(Clone, Copy)]
enum ChangeType {
    /// One `indent_unit` beyond the current expectation.
    Block,
    /// Two units (tidyverse double-indent function definitions).
    Double,
    /// An absolute column (content trails the opener; closer trails content).
    Hanging(u32),
}

/// Raven's extra accepted values, layered over the lintr primary.
#[derive(Clone, Copy)]
enum AltRule {
    None,
    /// Accept this absolute column too (aligned-argument style, operator
    /// chain-start alignment).
    AlsoCol(u32),
    /// Accept one `indent_unit` over the pre-change expectation too (the
    /// block form where lintr demands hanging/double).
    AlsoBlock,
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

fn collect_changes(node: Node<'_>, lines: &[&str], out: &mut Vec<Change>) {
    match node.kind() {
        "braced_expression" => bracket_change(node, node, lines, false, out),
        "call" | "subset" | "subset2" => {
            if let Some(args) = node.child_by_field_name("arguments") {
                bracket_change(node, args, lines, false, out);
            }
        }
        "function_definition" => {
            if let Some(params) = node.child_by_field_name("parameters") {
                bracket_change(node, params, lines, true, out);
            }
            unbraced_body_change(node, "body", lines, out);
        }
        "parenthesized_expression" => bracket_change(node, node, lines, false, out),
        "if_statement" | "while_statement" | "for_statement" => {
            bracket_change(node, node, lines, false, out);
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
                    ty: ChangeType::Block,
                    alt: AltRule::None,
                });
            }
        }
        "binary_operator" | "extract_operator" | "namespace_operator" => {
            operator_change(node, lines, out);
        }
        "argument" | "parameter" => named_eq_change(node, out),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_changes(child, lines, out);
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
        (ChangeType::Double, AltRule::AlsoBlock)
    } else if closer_on_own_line {
        let alt = if has_content_after_opener {
            AltRule::AlsoCol(opener_end_col)
        } else {
            AltRule::None
        };
        (ChangeType::Block, alt)
    } else {
        (ChangeType::Hanging(opener_end_col), AltRule::AlsoBlock)
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

/// Emit the continuation change for an infix operator whose right-hand side
/// starts on a later line, subject to the `assignment_as_infix` suppression.
fn operator_change(node: Node<'_>, lines: &[&str], out: &mut Vec<Change>) {
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
    out.push(Change {
        token_byte: op.start_byte(),
        begin: op.end_position().row as u32 + 1,
        end,
        ty: ChangeType::Block,
        // The on-type formatter may align continuations with the chain's
        // start column; accept that too so the linter never disagrees with
        // the formatter's output.
        alt: AltRule::AlsoCol(char_col(
            lines,
            node.start_position().row as u32,
            node.start_position().column,
        )),
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
        ty: ChangeType::Block,
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
        ty: ChangeType::Block,
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
        ty: ChangeType::Block,
        alt: AltRule::None,
    });
}

/// Collect line numbers that start strictly inside a multi-line string. For a
/// string spanning rows `[r1, r2]` with `r2 > r1`, lines `r1 + 1 ..= r2` start
/// inside the string and are skipped by the linter.
fn collect_string_interior_lines(node: Node<'_>, set: &mut HashSet<u32>) {
    if node.kind() == "string" {
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

fn leading_space_count(line: &str) -> u32 {
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

    fn lint(text: &str, indent_unit: u32) -> Vec<Diagnostic> {
        let tree = with_parser(|p| p.parse(text, None)).expect("parse must succeed");
        let suppressions = crate::linting::nolint::Suppressions::from_text(text);
        let mut out = Vec::new();
        collect(
            text,
            tree.root_node(),
            indent_unit,
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
    fn chain_start_col_accepted_as_alternative() {
        // The on-type formatter aligns `bar()` with `foo()` (column 10 —
        // `result <- ` is 10 chars wide) because the chain starts there.
        // The linter must also accept the hanging indent (2). Both must
        // pass without diagnostics.
        let aligned = "result <- foo() +\n          bar()\n";
        let hanging = "result <- foo() +\n  bar()\n";
        assert!(lint(aligned, 2).is_empty(), "aligned style should pass");
        assert!(lint(hanging, 2).is_empty(), "hanging style should pass");
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
    fn mixed_pipe_and_arithmetic_chain_accepts_hanging_and_subchain_aligned() {
        // `f() |> g() + y + z` chains across pipe and arithmetic. The
        // hanging form (2) and the chain-start column (which lines up with
        // `f()` because the binop node spans from there) must both be
        // accepted to avoid disagreeing with the on-type formatter.
        let hanging = "x <- f() |>\n  g() + y +\n  z\n";
        let aligned = "x <- f() |>\n     g() + y +\n     z\n";
        assert!(lint(hanging, 2).is_empty(), "hanging style should pass");
        assert!(
            lint(aligned, 2).is_empty(),
            "subchain-aligned style should pass"
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
}
