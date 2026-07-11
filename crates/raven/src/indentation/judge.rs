//! Judge-backed smart indentation over a repaired virtual buffer.

use tower_lsp::lsp_types::Position;
use tree_sitter::{InputEdit, Node, Point, Tree};

use super::calculator::{IndentationConfig, IndentationStyle};
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeKind {
    Blank,
    PureClosers,
    MixedClosers,
    ExistingContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeCoverage {
    Masked,
    Explained,
    Unexplained,
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
/// Returns `None` when the repair-and-ask path cannot answer, so the caller can
/// fall back to the legacy `detect_context` / `calculate_indentation` path.
/// Beyond unanswerable repairs, the judge deliberately declines in three
/// situations where its character-column model would misfire:
///
/// * The editor inserts tabs, or a scanned line's whitespace holds a real
///   (non-string) tab — the expectation engine counts characters while the
///   editor renders tab stops, so tab contexts keep the legacy visual-column
///   path.
/// * The repaired buffer still fails to parse — the expectation engine would
///   answer top-level (column 0) for a probe it cannot model, deindenting the
///   new line, where the legacy path degrades gracefully.
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

    let Some(probe) = logical_line(source, position.line) else {
        log::trace!(
            "judge_backed_indentation: bail: line {} is out of bounds ({} lines)",
            position.line,
            source.lines().count() + usize::from(source.is_empty() || source.ends_with('\n'))
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

    if cursor_is_inside_multiline_string_like(tree, source, position) {
        log::trace!(
            "judge_backed_indentation: bail: cursor on line {} is inside a multiline string",
            position.line
        );
        return None;
    }

    let Some(virtual_buffer) = build_virtual_buffer(tree, source, position, None) else {
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
    if error_at_or_above(virtual_tree.root_node(), position.line as usize) {
        log::trace!("judge_backed_indentation: bail: repaired buffer still has syntax errors");
        return None;
    }

    let reference = reference_row(&virtual_tree, source, position.line);
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

/// True when the repaired buffer holds a syntax error whose extent begins at
/// or above the probe line. Such an error means the context the probe's
/// expectation folds over is malformed beyond the sentinel repair, and the
/// engine's answer would be a meaningless top level (e.g. after `x %+`).
/// Errors that begin strictly below the probe are expected mid-typing — a
/// virtual `if (x <- raven_sentinel_)` is still waiting for its consequence —
/// and cannot influence the probe's covering changes.
fn error_at_or_above(node: Node<'_>, probe_line: usize) -> bool {
    if !node.has_error() || node.start_position().row > probe_line {
        return false;
    }
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| error_at_or_above(child, probe_line))
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
/// reference line's actual indent on the source is exact. The prefix is
/// collected once so the reverse walk never rescans it per skipped row
/// (masked Rmd/Quarto prose produces long blank runs above a chunk).
fn reference_row(virtual_tree: &Tree, source: &str, probe_line: u32) -> Option<(u32, u32)> {
    let lines: Vec<&str> = source.lines().take(probe_line as usize).collect();
    lines
        .iter()
        .copied()
        .enumerate()
        .rev()
        .find_map(|(row, line)| {
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
    position: Position,
    probe_indent: Option<u32>,
) -> Option<VirtualBuffer> {
    let probe = logical_line(source, position.line)?;
    if position.character as usize > probe.encode_utf16().count() {
        return None;
    }
    let line_start = line_start_byte(source, position.line)?;
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

    let mut openers = unclosed_delimiters_for_judge(tree, source, position.line)?;
    // Consume the probe's own leading closers from the opener stack. Mixed
    // content keeps them verbatim inside `trimmed`; pure closers are pushed
    // below the sentinel so each real closer appears exactly once in the
    // repair (re-appending them after synthesized copies would guarantee an
    // unmatched duplicate and an ERROR tree). A closer left over once the
    // stack empties is an unmatched stray (an auto-close artifact): a
    // pure-closer line simply drops it from the repair, while mixed content
    // must bail because its text cannot be edited piecemeal.
    let mut pushed_closers = String::new();
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
    let pushed_len = if pushed_closers.is_empty() {
        0
    } else {
        1 + pushed_closers.len()
    };
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
    if !pushed_closers.is_empty() {
        text.push('\n');
        text.push_str(&pushed_closers);
        inserted_rows += 1;
        last_line_len = pushed_closers.len();
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

fn cursor_is_inside_multiline_string_like(tree: &Tree, source: &str, position: Position) -> bool {
    let Some(line) = logical_line(source, position.line) else {
        return false;
    };
    let byte_col = utf16_column_to_byte_offset(line, position.character);
    multiline_string_like_contains(tree, source, position.line as usize, byte_col)
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

fn logical_line(source: &str, line: u32) -> Option<&str> {
    source.lines().nth(line as usize).or_else(|| {
        (source.ends_with('\n') && line as usize == source.lines().count()).then_some("")
    })
}

fn multiline_string_like_contains(tree: &Tree, source: &str, row: usize, column: usize) -> bool {
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
        && let Some(previous) = logical_line(source, row as u32 - 1)
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

/// One pass over the lines above `current_line`, keeping a stack of unclosed
/// bracket openers. Bracket and quote characters inside strings, comments,
/// and backtick-quoted identifiers are masked via the parse tree; a quote
/// character the tree cannot explain aborts the scan (`None`), and so does a
/// real (non-masked) tab anywhere on a scanned line — the expectation engine
/// measures character columns and the judge emits spaces, so tab-shaped
/// context must use the legacy visual-column fallback.
fn unclosed_delimiters_for_judge(
    tree: &Tree,
    source: &str,
    current_line: u32,
) -> Option<Vec<(u32, u32, char)>> {
    let mut stack = Vec::new();
    let mut rows_seen = 0usize;
    let mut byte_start = 0usize;
    for (row, raw) in source
        .split_inclusive('\n')
        .take(current_line as usize)
        .enumerate()
    {
        rows_seen = row + 1;
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        for (column, ch) in line.char_indices() {
            if !matches!(
                ch,
                '(' | '[' | '{' | ')' | ']' | '}' | '\"' | '\'' | '`' | '\t'
            ) {
                continue;
            }
            let coverage = tree_coverage(tree, row, column, byte_start + column);
            if coverage == TreeCoverage::Masked {
                continue;
            }
            if ch == '\t' {
                return None;
            }
            if matches!(ch, '\"' | '\'' | '`') {
                if coverage == TreeCoverage::Unexplained {
                    return None;
                }
                continue;
            }
            match ch {
                '(' | '[' | '{' => stack.push((row as u32, column as u32, ch)),
                ')' if stack.last().is_some_and(|(_, _, opener)| *opener == '(') => {
                    stack.pop();
                }
                ']' if stack.last().is_some_and(|(_, _, opener)| *opener == '[') => {
                    stack.pop();
                }
                '}' if stack.last().is_some_and(|(_, _, opener)| *opener == '{') => {
                    stack.pop();
                }
                _ => {}
            }
        }
        byte_start += raw.len();
    }
    if rows_seen < current_line as usize {
        return None;
    }
    Some(stack)
}

fn tree_coverage(tree: &Tree, row: usize, column: usize, byte: usize) -> TreeCoverage {
    let point = Point::new(row, column);
    let Some(deepest) = tree.root_node().descendant_for_point_range(point, point) else {
        return TreeCoverage::Unexplained;
    };
    let mut deepest_covering = None;
    let mut node = Some(deepest);
    while let Some(current) = node {
        if current.start_byte() <= byte && byte < current.end_byte() {
            deepest_covering.get_or_insert(current);
            // An `identifier` covering a bracket, quote, or tab byte can only
            // be backtick-quoted — its content is literal, like a string's.
            if matches!(current.kind(), "string" | "comment" | "identifier") {
                return TreeCoverage::Masked;
            }
        }
        node = current.parent();
    }
    match deepest_covering {
        Some(node) if node.child_count() == 0 && !node.is_error() && !node.is_missing() => {
            TreeCoverage::Explained
        }
        _ => TreeCoverage::Unexplained,
    }
}

fn point_in_node(node: Node<'_>, row: usize, column: usize) -> bool {
    let start = node.start_position();
    let end = node.end_position();
    (row > start.row || (row == start.row && column >= start.column))
        && (row < end.row || (row == end.row && column < end.column))
}

fn line_start_byte(source: &str, line: u32) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    let mut seen = 0u32;
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == line {
                return Some(index + 1);
            }
        }
    }
    None
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
        build_virtual_buffer(&tree, source, Position::new(line, 0), Some(column))
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
            unclosed_delimiters_for_judge(&tree, source, 1),
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
            let virtual_buffer =
                build_virtual_buffer(&tree, case.source, Position::new(case.line, 0), None)
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
