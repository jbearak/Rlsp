//! Judge-backed smart indentation over a repaired virtual buffer.

use tower_lsp::lsp_types::Position;
use tree_sitter::{InputEdit, Node, Point, Tree};

use super::config::{IndentationConfig, IndentationStyle};
use crate::linting::{
    IndentKind, InfixContinuationStyle, LineIndentExpectation, accepted_indents_for_lines,
    leading_space_count,
};
use crate::parser_pool::with_parser;
use crate::utf16::utf16_column_to_byte_offset;

const SENTINEL: &str = "raven_sentinel_";

#[derive(Debug)]
struct VirtualBuffer {
    text: String,
    probe_line: u32,
    /// The single splice that turned the source into `text`, so the caller
    /// can reparse incrementally from the document's existing tree instead of
    /// paying a full parse on every Enter press.
    edit: InputEdit,
    /// Rows above the probe carrying a real (non-string) tab, surfaced from
    /// the delimiter scan so the caller can bail only when one sits inside
    /// the active indentation context.
    tab_rows: Vec<u32>,
    /// Row of the outermost still-unclosed opener before the probe (before
    /// the probe's own closers consumed any) — the top of the active context
    /// for the tab gate.
    outer_opener_row: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeKind {
    Blank,
    PureClosers,
    MixedClosers,
    ExistingContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FormPref {
    Aligned,
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionPrefs {
    args: FormPref,
    infix: FormPref,
}

impl SelectionPrefs {
    /// Project the existing indentation setting onto the two future preference
    /// axes. This is intentionally the only place that knows about
    /// `IndentationStyle`.
    fn from_config(config: &IndentationConfig) -> Option<Self> {
        match config.style {
            IndentationStyle::RStudio => Some(Self {
                args: FormPref::Aligned,
                infix: FormPref::Aligned,
            }),
            IndentationStyle::RStudioMinus => Some(Self {
                args: FormPref::Block,
                infix: FormPref::Block,
            }),
            IndentationStyle::Off => None,
        }
    }
}

/// Compute the indent column for the cursor line after Enter by asking the
/// indentation lint's own expectation engine on a repaired virtual buffer.
///
/// The two style axes have deliberately separate jobs: the lint setting
/// `raven.linting.infixContinuationStyle` defines the accepted set (which
/// columns are legal), while the indentation style defines the emit preference
/// (which legal column to produce). They live in different settings namespaces
/// and must never be conflated.
///
/// Returns `None` when the repair-and-ask path cannot answer, so the caller
/// emits no edit and the editor's Tier 1/native indentation stands.
/// Beyond unanswerable repairs, the judge deliberately declines in three
/// situations where its character-column model would misfire:
///
/// * The editor inserts tabs, or a real (non-string) tab sits inside the
///   active context (from the outermost unclosed opener or the reference
///   line down to the probe) — the expectation engine counts characters
///   while the editor renders tab stops, so tab-shaped context keeps the
///   legacy visual-column path. Tabs on earlier completed statements are
///   irrelevant and do not disable the judge.
/// * The repaired buffer still has a syntax error intersecting the
///   reference-to-probe row window — the expectation engine would answer
///   top-level (column 0) for a probe it cannot model, deindenting the new
///   line, where the legacy path degrades gracefully. Errors on unrelated
///   earlier statements (which the lint's own fold tolerates too) do not
///   disable the judge.
/// * The nearest checkable line above the probe does not sit at a column the
///   lint accepts — the expectation model accumulates from column 0 and would
///   collapse a user's deliberately offset context (e.g. a top-level `    {`),
///   while the legacy path anchors to physical indentation.
pub fn judge_backed_indentation(
    tree: &Tree,
    source: &str,
    position: Position,
    config: &IndentationConfig,
    infix_style: InfixContinuationStyle,
) -> Option<u32> {
    let Some(prefs) = SelectionPrefs::from_config(config) else {
        log::trace!("judge_backed_indentation: bail: indentation style is Off");
        return None;
    };
    if !config.insert_spaces {
        log::trace!("judge_backed_indentation: bail: editor inserts tabs, not spaces");
        return None;
    }

    let line_index = indexed_lines(source);
    let Some((_, probe)) = line_index.get(position.line as usize).copied() else {
        log::trace!(
            "judge_backed_indentation: bail: line {} is out of bounds ({} lines)",
            position.line,
            line_index.len()
        );
        return None;
    };
    if position.character as usize > probe.encode_utf16().count() {
        log::trace!(
            "judge_backed_indentation: bail: character {} is out of bounds on line {}",
            position.character,
            position.line
        );
        return None;
    }

    if cursor_is_inside_multiline_string_like(tree, &line_index, position) {
        log::trace!(
            "judge_backed_indentation: bail: cursor on line {} is inside a multiline string",
            position.line
        );
        return None;
    }

    let Some(virtual_buffer) = build_virtual_buffer(tree, source, &line_index, position, None)
    else {
        log::trace!("judge_backed_indentation: bail: virtual repair was ambiguous");
        return None;
    };
    let mut edited_tree = tree.clone();
    edited_tree.edit(&virtual_buffer.edit);
    let Some(virtual_tree) =
        with_parser(|parser| parser.parse(&virtual_buffer.text, Some(&edited_tree)))
    else {
        log::trace!("judge_backed_indentation: bail: virtual parse failed");
        return None;
    };

    let reference = reference_row(&virtual_tree, &line_index, position.line);

    // A residual syntax error is disqualifying only when it touches the rows
    // the answer actually reads — the reference-to-probe window. An error on
    // an unrelated earlier statement leaves the probe's covering changes (and
    // the lint's own fold, which never bails on error trees) intact.
    let error_window_start = reference.map_or(position.line, |(row, _)| row) as usize;
    if error_intersects_rows(
        virtual_tree.root_node(),
        error_window_start,
        position.line as usize,
    ) {
        log::trace!(
            "judge_backed_indentation: bail: repaired buffer has syntax errors in the \
             reference-to-probe window"
        );
        return None;
    }

    // A real tab distorts the emitted columns only when it sits inside the
    // active context: the rows from the outermost unclosed opener (whose
    // aligned columns feed the expectation) or the reference line down to the
    // probe. Tabs on earlier completed statements are irrelevant.
    let tab_window_start = virtual_buffer
        .outer_opener_row
        .into_iter()
        .chain(reference.map(|(row, _)| row))
        .min();
    if let Some(start) = tab_window_start
        && virtual_buffer.tab_rows.iter().any(|&row| row >= start)
    {
        log::trace!(
            "judge_backed_indentation: bail: a real tab inside the active context needs the \
             legacy visual-column path"
        );
        return None;
    }

    let targets: Vec<u32> = reference
        .iter()
        .map(|&(row, _)| row)
        .chain([virtual_buffer.probe_line])
        .collect();
    let mut expectations = accepted_indents_for_lines(
        &virtual_buffer.text,
        virtual_tree.root_node(),
        config.tab_size,
        infix_style,
        &targets,
    );
    let expected = expectations.pop()?;
    if let (Some((reference, actual)), Some(reference_expected)) = (reference, expectations.pop())
        && !reference_expected.accepts(actual)
    {
        log::trace!(
            "judge_backed_indentation: bail: reference line {reference} sits at column \
             {actual}, outside its accepted set (primary {})",
            reference_expected.primary
        );
        return None;
    }
    log::trace!(
        "judge_backed_indentation: accepted primary={}, alternatives={:?}",
        expected.primary,
        expected.alternatives
    );

    let selected = select_column(&expected, prefs);
    log::trace!(
        "judge_backed_indentation: selected column={} with prefs={:?}",
        selected,
        prefs
    );
    Some(selected)
}

/// True when the repaired buffer holds a syntax error whose row extent
/// intersects `[start_row, end_row]` — the reference-to-probe window. Such an
/// error means the context the probe's expectation folds over is malformed
/// beyond the sentinel repair, and the engine's answer would be a meaningless
/// top level (e.g. after `x %+`). Errors outside the window are expected
/// mid-typing — a virtual `if (x <- raven_sentinel_)` is still waiting for
/// its consequence below the probe, and a malformed statement several lines
/// up does not feed the probe's covering changes.
fn error_intersects_rows(node: Node<'_>, start_row: usize, end_row: usize) -> bool {
    if !node.has_error()
        || node.start_position().row > end_row
        || node.end_position().row < start_row
    {
        return false;
    }
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| error_intersects_rows(child, start_row, end_row))
}

/// The nearest line above the probe whose physical indent the lint's
/// expectation model actually constrains: skips blank lines, comment-only
/// lines (aligned comment runs may sit at exempted columns), and lines that
/// start inside a multiline string or backtick-quoted identifier. Returns the
/// row together with its measured indent; `None` when no such line exists
/// (the probe is the first content line).
///
/// The lines above the probe are identical in the source and the virtual
/// buffer — the repair splices only the probe line — so measuring the
/// reference line's actual indent on the source is exact. The shared line
/// index makes the reverse walk constant-time per skipped row (masked
/// Rmd/Quarto prose produces long blank runs above a chunk).
fn reference_row(
    virtual_tree: &Tree,
    line_index: &[(usize, &str)],
    probe_line: u32,
) -> Option<(u32, u32)> {
    line_index
        .iter()
        .take(probe_line as usize)
        .enumerate()
        .rev()
        .find_map(|(row, &(_, line))| {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            (!line_starts_inside_multiline_node(virtual_tree, row))
                .then(|| (row as u32, leading_space_count(line)))
        })
}

fn build_virtual_buffer(
    tree: &Tree,
    source: &str,
    line_index: &[(usize, &str)],
    position: Position,
    probe_indent: Option<u32>,
) -> Option<VirtualBuffer> {
    let &(line_start, probe) = line_index.get(position.line as usize)?;
    if position.character as usize > probe.encode_utf16().count() {
        return None;
    }
    let line_end = line_start.checked_add(probe.len())?;
    let content_start = probe
        .char_indices()
        .find_map(|(index, ch)| (!ch.is_whitespace()).then_some(index))
        .unwrap_or(probe.len());
    let trimmed = &probe[content_start..];
    let (kind, leading_closers) = classify_probe(trimmed);
    log::trace!(
        "judge_backed_indentation: probe line={}, kind={kind:?}",
        position.line
    );

    let scan = unclosed_delimiters_for_judge(tree, source, line_index, position.line)?;
    let outer_opener_row = scan.openers.first().map(|&(row, _, _)| row);
    let tab_rows = scan.tab_rows;
    let mut openers = scan.openers;
    // Consume the probe's own leading closers from the opener stack. Mixed
    // content keeps them verbatim inside `trimmed`; pure closers are pushed
    // below the sentinel so each real closer appears exactly once in the
    // repair (re-appending them after synthesized copies would guarantee an
    // unmatched duplicate and an ERROR tree). A closer left over once the
    // stack empties is an unmatched stray (an auto-close artifact): a
    // pure-closer line simply drops it from the repair, while mixed content
    // must bail because its text cannot be edited piecemeal.
    let mut pushed_closers = Vec::new();
    for closer in leading_closers {
        match openers.pop() {
            Some((_, _, opener)) if matching_closer(opener) == Some(closer) => {
                if kind == ProbeKind::PureClosers {
                    pushed_closers.push(closer);
                }
            }
            // Mismatched nesting — the repair is ambiguous.
            Some(_) => return None,
            None if kind == ProbeKind::PureClosers => break,
            None => return None,
        }
    }
    let closers: Vec<char> = openers
        .iter()
        .rev()
        .map(|(_, _, opener)| {
            matching_closer(*opener).expect("judge delimiter scan only stores supported openers")
        })
        .collect();
    log::trace!("judge_backed_indentation: synthesized closers={closers:?}");

    let replacement_content = match kind {
        ProbeKind::Blank | ProbeKind::PureClosers => SENTINEL,
        ProbeKind::MixedClosers | ProbeKind::ExistingContent => trimmed,
    };
    let replacement_indent_len = probe_indent.map_or(content_start, |column| column as usize);
    let pushed_len = pushed_closers.len().saturating_mul(2);
    let inserted_len = replacement_indent_len
        .saturating_add(replacement_content.len())
        .saturating_add(pushed_len)
        .saturating_add(closers.len().saturating_mul(2));
    let mut text = String::with_capacity(source.len().saturating_add(inserted_len));
    text.push_str(&source[..line_start]);
    if let Some(column) = probe_indent {
        text.push_str(&" ".repeat(column as usize));
    } else {
        text.push_str(&probe[..content_start]);
    }
    text.push_str(replacement_content);
    let mut inserted_rows = 0usize;
    let mut last_line_len = replacement_indent_len + replacement_content.len();
    // Pushed-down real closers close the innermost popped openers, so they
    // precede the synthesized closers for the still-open outer ones. Each
    // sits on its own line to retain the lint's closer-on-own-line
    // classification.
    for closer in pushed_closers {
        text.push('\n');
        text.push(closer);
        inserted_rows += 1;
        last_line_len = 1;
    }
    for closer in closers {
        text.push('\n');
        text.push(closer);
        inserted_rows += 1;
        last_line_len = 1;
    }
    text.push_str(&source[line_end..]);

    let edit = InputEdit {
        start_byte: line_start,
        old_end_byte: line_end,
        new_end_byte: line_start + inserted_len,
        start_position: Point::new(position.line as usize, 0),
        old_end_position: Point::new(position.line as usize, probe.len()),
        new_end_position: Point::new(position.line as usize + inserted_rows, last_line_len),
    };

    Some(VirtualBuffer {
        text,
        probe_line: position.line,
        edit,
        tab_rows,
        outer_opener_row,
    })
}

fn classify_probe(trimmed: &str) -> (ProbeKind, Vec<char>) {
    if trimmed.is_empty() {
        return (ProbeKind::Blank, Vec::new());
    }
    let mut closers = Vec::new();
    for ch in trimmed.chars() {
        if matches!(ch, ')' | ']' | '}') {
            closers.push(ch);
        } else if !ch.is_whitespace() {
            return if closers.is_empty() {
                (ProbeKind::ExistingContent, closers)
            } else {
                (ProbeKind::MixedClosers, closers)
            };
        }
    }
    (ProbeKind::PureClosers, closers)
}

fn matching_closer(opener: char) -> Option<char> {
    match opener {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

fn cursor_is_inside_multiline_string_like(
    tree: &Tree,
    line_index: &[(usize, &str)],
    position: Position,
) -> bool {
    let Some((_, line)) = line_index.get(position.line as usize).copied() else {
        return false;
    };
    let byte_col = utf16_column_to_byte_offset(line, position.character);
    multiline_string_like_contains(tree, line_index, position.line as usize, byte_col)
}

/// Strings and backtick-quoted identifiers both carry literal text across
/// lines, so the judge must treat their interiors as opaque — indenting
/// inside one would rewrite the literal (for an identifier, the symbol's
/// name). A plain identifier can never span rows, so any multiline
/// `identifier` node is backtick-quoted.
fn is_multiline_string_like(node: Node<'_>) -> bool {
    matches!(node.kind(), "string" | "identifier")
        && node.start_position().row < node.end_position().row
}

/// True when `row` starts strictly inside a multiline string or
/// backtick-quoted identifier — the same lines the indentation lint skips as
/// string interiors, whose leading whitespace is literal content rather than
/// indentation.
fn line_starts_inside_multiline_node(tree: &Tree, row: usize) -> bool {
    let point = Point::new(row, 0);
    let mut node = tree.root_node().descendant_for_point_range(point, point);
    while let Some(current) = node {
        if is_multiline_string_like(current)
            && current.start_position().row < row
            && point_in_node(current, row, 0)
        {
            return true;
        }
        node = current.parent();
    }
    false
}

fn multiline_string_like_contains(
    tree: &Tree,
    line_index: &[(usize, &str)],
    row: usize,
    column: usize,
) -> bool {
    let point = Point::new(row, column);
    let mut node = tree.root_node().descendant_for_point_range(point, point);
    while let Some(current) = node {
        if is_multiline_string_like(current) && point_in_node(current, row, column) {
            return true;
        }
        node = current.parent();
    }

    // At an empty EOF line, tree-sitter may resolve the exact boundary point
    // to the program rather than to an unterminated string ending there.
    // Probe the preceding byte and accept only a multiline string whose end
    // is exactly the cursor boundary.
    if column == 0
        && row > 0
        && let Some((_, previous)) = line_index.get(row - 1).copied()
        && !previous.is_empty()
    {
        let preceding = Point::new(row - 1, previous.len() - 1);
        let mut node = tree
            .root_node()
            .descendant_for_point_range(preceding, preceding);
        while let Some(current) = node {
            if is_multiline_string_like(current) && current.end_position() == point {
                return true;
            }
            node = current.parent();
        }
    }
    false
}

/// The delimiter scan's findings for the prefix above the probe line.
struct PrefixScan {
    /// Still-unclosed bracket openers, outermost first.
    openers: Vec<(u32, u32, char)>,
    /// Rows carrying a real (non-string) tab, ascending and deduplicated.
    tab_rows: Vec<u32>,
}

/// One pass over the lines above `current_line`, keeping a stack of unclosed
/// bracket openers. Bracket, quote, and tab characters inside strings,
/// comments, and backtick-quoted identifiers are masked via byte intervals
/// collected from the parse tree in a single pruned pre-order pass (no
/// per-character tree lookups). A quote character outside every masked
/// interval is one the tree cannot explain as string-like content — the
/// prefix's string layout is not understood, so the scan aborts (`None`).
/// Real tabs are reported per row rather than aborting; the caller decides
/// whether one sits inside the active indentation context.
fn unclosed_delimiters_for_judge(
    tree: &Tree,
    source: &str,
    line_index: &[(usize, &str)],
    current_line: u32,
) -> Option<PrefixScan> {
    let prefix_end = line_index.get(current_line as usize)?.0;
    let masked = masked_intervals(tree, source, prefix_end);
    let mut mask_idx = 0usize;
    let mut openers = Vec::new();
    let mut tab_rows: Vec<u32> = Vec::new();
    for (row, &(byte_start, line)) in line_index.iter().take(current_line as usize).enumerate() {
        for (column, ch) in line.char_indices() {
            if !matches!(
                ch,
                '(' | '[' | '{' | ')' | ']' | '}' | '\"' | '\'' | '`' | '\t'
            ) {
                continue;
            }
            let byte = byte_start + column;
            while mask_idx < masked.len() && masked[mask_idx].1 <= byte {
                mask_idx += 1;
            }
            if mask_idx < masked.len() && masked[mask_idx].0 <= byte {
                continue;
            }
            match ch {
                '\t' => {
                    if tab_rows.last() != Some(&(row as u32)) {
                        tab_rows.push(row as u32);
                    }
                }
                '\"' | '\'' | '`' => return None,
                '(' | '[' | '{' => openers.push((row as u32, column as u32, ch)),
                ')' if openers.last().is_some_and(|(_, _, opener)| *opener == '(') => {
                    openers.pop();
                }
                ']' if openers.last().is_some_and(|(_, _, opener)| *opener == '[') => {
                    openers.pop();
                }
                '}' if openers.last().is_some_and(|(_, _, opener)| *opener == '{') => {
                    openers.pop();
                }
                _ => {}
            }
        }
    }
    Some(PrefixScan { openers, tab_rows })
}

/// Byte intervals of literal content — strings, comments, and
/// backtick-quoted identifiers — in `[0, prefix_end)`, sorted and
/// non-overlapping (pre-order without descending into masked nodes). One
/// pruned pass over the tree replaces a root-to-leaf `tree_coverage` query
/// per scanned character.
fn masked_intervals(tree: &Tree, source: &str, prefix_end: usize) -> Vec<(usize, usize)> {
    fn walk(node: Node<'_>, source: &str, prefix_end: usize, out: &mut Vec<(usize, usize)>) {
        if node.start_byte() >= prefix_end {
            return;
        }
        let masked = match node.kind() {
            "string" | "comment" => true,
            // A backtick-quoted identifier's content is literal like a
            // string's; plain identifiers cannot contain the scanned
            // characters and stay transparent.
            "identifier" => source.as_bytes().get(node.start_byte()) == Some(&b'`'),
            _ => false,
        };
        if masked {
            out.push((node.start_byte(), node.end_byte()));
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.start_byte() >= prefix_end {
                break;
            }
            walk(child, source, prefix_end, out);
        }
    }
    let mut out = Vec::new();
    walk(tree.root_node(), source, prefix_end, &mut out);
    out
}

fn point_in_node(node: Node<'_>, row: usize, column: usize) -> bool {
    let start = node.start_position();
    let end = node.end_position();
    (row > start.row || (row == start.row && column >= start.column))
        && (row < end.row || (row == end.row && column < end.column))
}

/// Logical lines paired with their source byte offsets. Newline terminators
/// are excluded, CRLF loses both bytes, and a final newline contributes one
/// trailing empty line. Empty source has no logical lines.
fn indexed_lines(source: &str) -> Vec<(usize, &str)> {
    if source.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut start = 0usize;
    for raw in source.split_inclusive('\n') {
        let text = match raw.strip_suffix('\n') {
            Some(text) => text.strip_suffix('\r').unwrap_or(text),
            None => raw,
        };
        lines.push((start, text));
        start += raw.len();
    }
    if source.ends_with('\n') {
        lines.push((source.len(), ""));
    }
    lines
}

fn select_column(expected: &LineIndentExpectation, prefs: SelectionPrefs) -> u32 {
    let has_kind = |kind| {
        expected
            .alternatives
            .iter()
            .any(|(_, kinds)| kinds.contains(kind))
    };
    let candidate = |kind: IndentKind| {
        expected
            .alternatives
            .iter()
            .find_map(|(column, kinds)| kinds.contains(kind).then_some(*column))
    };

    if has_kind(IndentKind::ChainStart) {
        return match prefs.infix {
            FormPref::Aligned => expected
                .alternatives
                .iter()
                .filter_map(|(column, kinds)| {
                    (kinds.contains(IndentKind::ChainStart) && *column > expected.primary)
                        .then_some(*column)
                })
                .max()
                .unwrap_or(expected.primary),
            FormPref::Block => candidate(IndentKind::Block).unwrap_or(expected.primary),
        };
    }

    if has_kind(IndentKind::OpenerAligned) || has_kind(IndentKind::Block) {
        return match prefs.args {
            FormPref::Aligned => candidate(IndentKind::OpenerAligned).unwrap_or(expected.primary),
            FormPref::Block => candidate(IndentKind::Block).unwrap_or(expected.primary),
        };
    }

    expected.primary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linting::{IndentKindSet, accepted_indents_for_line, lint_for_judge_test};

    #[derive(Clone, Copy)]
    struct Case {
        name: &'static str,
        source: &'static str,
        line: u32,
        expected: u32,
    }

    const CASES: &[Case] = &[
        Case {
            name: "top-level",
            source: "x\n",
            line: 1,
            expected: 0,
        },
        Case {
            name: "issue 611 assignment",
            source: "result <-\n",
            line: 1,
            expected: 2,
        },
        Case {
            name: "assignment in function body",
            source: "f <- function() {\n  x <-\n",
            line: 2,
            expected: 4,
        },
        Case {
            name: "assignment in parenthesized expression",
            source: "(\n  x <-\n",
            line: 2,
            expected: 4,
        },
        Case {
            name: "walrus in bracket",
            source: "dt[, y :=\n",
            line: 1,
            expected: 5,
        },
        Case {
            name: "assignment in if condition",
            source: "if (x <-\n",
            line: 1,
            expected: 6,
        },
        Case {
            name: "pushed paren",
            source: "f(\n)",
            line: 1,
            expected: 2,
        },
        Case {
            name: "pushed brace and paren",
            source: "f <- function() {\n})",
            line: 1,
            expected: 2,
        },
        Case {
            name: "mixed brace closer and else body",
            source: "if (x) {\n} else y",
            line: 1,
            expected: 0,
        },
        Case {
            name: "mixed paren closer and infix rhs",
            source: "f(a\n) + x",
            line: 1,
            expected: 0,
        },
        Case {
            name: "nested mixed closer",
            source: "f(\n  g(b\n  ) + 1",
            line: 2,
            expected: 2,
        },
        Case {
            name: "multiline string closes before function opener",
            source: "x <- c(\"a\nb\", function() {\n",
            line: 2,
            expected: 9,
        },
        Case {
            name: "multiline string closes before nested calls",
            source: "x <- paste(\"a\nb\", f(g(\n",
            line: 2,
            expected: 13,
        },
        Case {
            name: "raw string quote does not poison later scan",
            source: "x <- r\"(don\"t)\"\nf(\n",
            line: 2,
            expected: 2,
        },
        Case {
            name: "string closer does not pop real opener",
            source: "f(\"a)b\",\n",
            line: 1,
            expected: 2,
        },
        Case {
            name: "string bracket opener is not synthesized",
            source: "g(\"[\",\n",
            line: 1,
            expected: 2,
        },
        Case {
            name: "string paren opener is not synthesized",
            source: "f(\"(\", \n",
            line: 1,
            expected: 2,
        },
        Case {
            name: "earlier call opener",
            source: "f(\n  b <-\n",
            line: 2,
            expected: 4,
        },
        Case {
            name: "earlier paren opener",
            source: "(\n  x <-\n",
            line: 2,
            expected: 4,
        },
        Case {
            name: "earlier bracket opener",
            source: "x[\n  a <-\n",
            line: 2,
            expected: 4,
        },
        Case {
            name: "earlier opener after first argument",
            source: "f(a,\n  b <-\n",
            line: 2,
            expected: 4,
        },
        Case {
            name: "same-line opener defers to hanging form",
            source: "long_function_name(x <-\n",
            line: 1,
            expected: 19,
        },
        Case {
            name: "backtick name closer does not pop real opener",
            source: "f(`a)b`,\n",
            line: 1,
            expected: 2,
        },
        Case {
            name: "backtick name opener is not synthesized",
            source: "list(\n  `a(b` = 1,\n",
            line: 2,
            expected: 2,
        },
        Case {
            name: "pushed inner closer precedes synthesized outer closer",
            source: "f(g(\n)",
            line: 1,
            expected: 2,
        },
        Case {
            name: "pushed nested closers land on own lines",
            source: "long_function_name(\n  g(\n))",
            line: 2,
            expected: 4,
        },
        Case {
            name: "tab inside string content does not poison the scan",
            source: "x <- c(\"a\n\tb\",\n  f(\n",
            line: 3,
            expected: 9,
        },
    ];

    fn config(style: IndentationStyle) -> IndentationConfig {
        IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style,
        }
    }
    fn judge(
        source: &str,
        position: Position,
        config: &IndentationConfig,
        infix_style: InfixContinuationStyle,
    ) -> Option<u32> {
        let tree =
            with_parser(|parser| parser.parse(source, None)).expect("real test input must parse");
        judge_backed_indentation(&tree, source, position, config, infix_style)
    }

    fn text_with_probe(source: &str, line: u32, column: u32) -> String {
        let tree =
            with_parser(|parser| parser.parse(source, None)).expect("real test input must parse");
        let line_index = indexed_lines(source);
        build_virtual_buffer(
            &tree,
            source,
            &line_index,
            Position::new(line, 0),
            Some(column),
        )
        .expect("test virtual repair must be answerable")
        .text
    }

    fn repaired_expectation(source: &str, line: u32) -> LineIndentExpectation {
        let virtual_text = text_with_probe(source, line, 0);
        let tree = with_parser(|parser| parser.parse(&virtual_text, None))
            .expect("repaired test input must parse");
        accepted_indents_for_line(
            &virtual_text,
            tree.root_node(),
            2,
            InfixContinuationStyle::Indented,
            line,
        )
    }

    #[test]
    fn disputed_shapes_pin_the_frozen_accepted_sets() {
        let walrus = repaired_expectation("dt[, y :=\n", 1);
        assert_eq!(walrus.primary, 4);
        assert_eq!(
            walrus.alternatives,
            vec![(
                5,
                IndentKindSet::of(&[IndentKind::OpenerAligned, IndentKind::ChainStart])
            )]
        );

        let same_line_call = repaired_expectation("long_function_name(x <-\n", 1);
        assert_eq!(same_line_call.primary, 4);
        assert_eq!(
            same_line_call.alternatives,
            vec![
                (21, IndentKindSet::single(IndentKind::OpenerAligned)),
                (19, IndentKindSet::single(IndentKind::ChainStart)),
            ]
        );
    }

    #[test]
    fn string_content_brackets_do_not_change_the_real_opener_stack() {
        let source = "f(\"a)b\",\n";
        let tree =
            with_parser(|parser| parser.parse(source, None)).expect("real test input must parse");
        assert_eq!(
            unclosed_delimiters_for_judge(&tree, source, &indexed_lines(source), 1)
                .map(|scan| scan.openers),
            Some(vec![(0, 1, '(')])
        );
    }

    #[test]
    fn repair_and_ask_shapes_return_lint_accepted_columns() {
        for case in CASES {
            for infix_style in [
                InfixContinuationStyle::Indented,
                InfixContinuationStyle::Aligned,
                InfixContinuationStyle::Either,
            ] {
                let source_before = case.source.to_owned();
                let column = judge(
                    case.source,
                    Position::new(case.line, 0),
                    &config(IndentationStyle::RStudio),
                    infix_style,
                )
                .unwrap_or_else(|| panic!("{} ({infix_style:?}) must be answerable", case.name));

                assert_eq!(
                    column, case.expected,
                    "{} ({infix_style:?}) returned the wrong column",
                    case.name
                );
                assert_eq!(
                    case.source, source_before,
                    "{} leaked its virtual repair into the source",
                    case.name
                );

                let real_text = text_with_probe(case.source, case.line, column);
                let diagnostics = lint_for_judge_test(&real_text, 2, infix_style);
                assert!(
                    diagnostics
                        .iter()
                        .all(|diagnostic| diagnostic.range.start.line != case.line),
                    "{} ({infix_style:?}) emitted a column rejected by the lint: {diagnostics:?}\n{real_text}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn rstudio_minus_prefers_legal_block_forms() {
        let source = "long_function_name(a,\n";
        let column = judge(
            source,
            Position::new(1, 0),
            &config(IndentationStyle::RStudioMinus),
            InfixContinuationStyle::Either,
        );
        assert_eq!(column, Some(2));
    }

    #[test]
    fn bails_for_invalid_positions_and_multiline_strings() {
        let cfg = config(IndentationStyle::RStudio);
        assert_eq!(
            judge(
                "x\n",
                Position::new(3, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            None
        );
        assert_eq!(
            judge(
                "x <- \"open\nstill open\n",
                Position::new(2, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            None
        );
        assert_eq!(
            judge(
                "f(\n] + x",
                Position::new(1, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            None
        );
    }

    #[test]
    fn bails_inside_multiline_backtick_identifiers() {
        let cfg = config(IndentationStyle::RStudio);
        assert_eq!(
            judge(
                "f(`a\nb`)\n",
                Position::new(1, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            None
        );
    }

    #[test]
    fn bails_when_context_does_not_conform_to_the_accepted_set() {
        let cfg = config(IndentationStyle::RStudio);
        for source in ["    {\n", "    result <-\n"] {
            assert_eq!(
                judge(
                    source,
                    Position::new(1, 0),
                    &cfg,
                    InfixContinuationStyle::Indented,
                ),
                None,
                "offset context {source:?} must fall back to the legacy anchor"
            );
        }
        assert_eq!(
            judge(
                "f <- function() {\n      x <- 1\n",
                Position::new(2, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            None,
            "over-indented sibling must fall back to the legacy anchor"
        );
    }

    #[test]
    fn conformity_reference_skips_blank_and_comment_lines() {
        let cfg = config(IndentationStyle::RStudio);
        assert_eq!(
            judge(
                "f(\n\n      # odd comment\n",
                Position::new(3, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            Some(2)
        );
    }

    #[test]
    fn bails_for_tab_contexts_and_tab_editors() {
        let cfg = config(IndentationStyle::RStudio);
        assert_eq!(
            judge(
                "\tf(a,\n",
                Position::new(1, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            None,
            "tab-indented context must use the legacy visual-column path"
        );

        let tabs = IndentationConfig {
            insert_spaces: false,
            ..config(IndentationStyle::RStudio)
        };
        assert_eq!(
            judge(
                "f(a,\n",
                Position::new(1, 0),
                &tabs,
                InfixContinuationStyle::Indented,
            ),
            None,
            "a tabs-mode editor must use the legacy visual-column path"
        );
    }

    #[test]
    fn unrelated_earlier_errors_do_not_disable_the_judge() {
        let cfg = config(IndentationStyle::RStudio);
        // The malformed `x +*` statement sits outside the reference-to-probe
        // window (its reference is the `f(` line) and must not make the
        // judge bail — the lint's own fold tolerates it too.
        assert_eq!(
            judge(
                "x +*\n\ny <- 1\nf(\n",
                Position::new(4, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            Some(2)
        );
    }

    #[test]
    fn tabs_outside_the_active_context_do_not_disable_the_judge() {
        let cfg = config(IndentationStyle::RStudio);
        // The tab sits between tokens of an earlier completed statement,
        // above both the outermost unclosed opener and the reference line,
        // so it cannot distort the emitted columns.
        assert_eq!(
            judge(
                "x <-\t1\nf(\n",
                Position::new(2, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            Some(2)
        );
    }

    #[test]
    fn bails_when_the_repaired_buffer_still_has_errors() {
        let cfg = config(IndentationStyle::RStudio);
        assert_eq!(
            judge(
                "x %+\n",
                Position::new(1, 0),
                &cfg,
                InfixContinuationStyle::Indented,
            ),
            None,
            "an unrepairable operator tail must not deindent to top level"
        );
    }

    #[test]
    fn pure_closer_repairs_parse_cleanly() {
        for (source, line) in [("f(\n)", 1), ("f <- function() {\n})", 1), ("f(g(\n)", 1)] {
            let virtual_text = text_with_probe(source, line, 0);
            let tree = with_parser(|parser| parser.parse(&virtual_text, None))
                .expect("repaired buffer must parse");
            assert!(
                !tree.root_node().has_error(),
                "{source:?} repaired to {virtual_text:?} must parse without errors"
            );
        }
    }

    #[test]
    fn incremental_reparse_matches_a_fresh_parse() {
        for case in CASES {
            let tree = with_parser(|parser| parser.parse(case.source, None))
                .expect("real test input must parse");
            let line_index = indexed_lines(case.source);
            let virtual_buffer = build_virtual_buffer(
                &tree,
                case.source,
                &line_index,
                Position::new(case.line, 0),
                None,
            )
            .unwrap_or_else(|| panic!("{} must be repairable", case.name));
            let mut edited = tree.clone();
            edited.edit(&virtual_buffer.edit);
            let incremental =
                with_parser(|parser| parser.parse(&virtual_buffer.text, Some(&edited)))
                    .expect("incremental parse must succeed");
            let fresh = with_parser(|parser| parser.parse(&virtual_buffer.text, None))
                .expect("fresh parse must succeed");
            assert_eq!(
                incremental.root_node().to_sexp(),
                fresh.root_node().to_sexp(),
                "{}: incremental reparse diverged from a fresh parse",
                case.name
            );
        }
    }

    #[test]
    fn selection_never_targets_top_level_alternative() {
        let expected = LineIndentExpectation {
            primary: 4,
            alternatives: vec![(0, IndentKindSet::single(IndentKind::TopLevel))],
        };
        assert_eq!(
            select_column(
                &expected,
                SelectionPrefs {
                    args: FormPref::Block,
                    infix: FormPref::Block,
                },
            ),
            4
        );
    }
}
