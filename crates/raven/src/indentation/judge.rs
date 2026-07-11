//! Judge-backed smart indentation over a repaired virtual buffer.

use tower_lsp::lsp_types::Position;
use tree_sitter::{Node, Point, Tree};

use super::calculator::{IndentationConfig, IndentationStyle};
use crate::linting::{
    IndentKind, InfixContinuationStyle, LineIndentExpectation, accepted_indents_for_line,
};
use crate::parser_pool::with_parser;
use crate::utf16::utf16_column_to_byte_offset;

const SENTINEL: &str = "raven_sentinel_";

#[derive(Debug)]
struct VirtualBuffer {
    text: String,
    probe_line: u32,
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

    if cursor_is_inside_multiline_string(tree, source, position) {
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
    let Some(tree) = with_parser(|parser| parser.parse(&virtual_buffer.text, None)) else {
        log::trace!("judge_backed_indentation: bail: virtual parse failed");
        return None;
    };
    let expected = accepted_indents_for_line(
        &virtual_buffer.text,
        tree.root_node(),
        config.tab_size,
        infix_style,
        virtual_buffer.probe_line,
    );
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
    if kind == ProbeKind::MixedClosers {
        for closer in leading_closers {
            let (_, _, opener) = openers.pop()?;
            if matching_closer(opener) != Some(closer) {
                return None;
            }
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

    let pushed_closers = (kind == ProbeKind::PureClosers).then_some(trimmed);
    let replacement_content = match kind {
        ProbeKind::Blank | ProbeKind::PureClosers => SENTINEL,
        ProbeKind::MixedClosers | ProbeKind::ExistingContent => trimmed,
    };
    let replacement_indent_len = probe_indent.map_or(content_start, |column| column as usize);
    let inserted_lines = closers.len() + usize::from(pushed_closers.is_some());
    let extra_capacity = replacement_indent_len
        .saturating_add(replacement_content.len())
        .saturating_add(inserted_lines.saturating_mul(2));
    let mut text = String::with_capacity(source.len().saturating_add(extra_capacity));
    text.push_str(&source[..line_start]);
    if let Some(column) = probe_indent {
        text.push_str(&" ".repeat(column as usize));
    } else {
        text.push_str(&probe[..content_start]);
    }
    text.push_str(replacement_content);
    for closer in closers {
        text.push('\n');
        text.push(closer);
    }
    if let Some(pushed) = pushed_closers {
        text.push('\n');
        text.push_str(pushed);
    }
    text.push_str(&source[line_end..]);

    Some(VirtualBuffer {
        text,
        probe_line: position.line,
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

fn cursor_is_inside_multiline_string(tree: &Tree, source: &str, position: Position) -> bool {
    let Some(line) = logical_line(source, position.line) else {
        return false;
    };
    let byte_col = utf16_column_to_byte_offset(line, position.character);
    multiline_string_contains(tree, source, position.line as usize, byte_col)
}

fn logical_line(source: &str, line: u32) -> Option<&str> {
    source.lines().nth(line as usize).or_else(|| {
        (source.ends_with('\n') && line as usize == source.lines().count()).then_some("")
    })
}

fn multiline_string_contains(tree: &Tree, source: &str, row: usize, column: usize) -> bool {
    let point = Point::new(row, column);
    let mut node = tree.root_node().descendant_for_point_range(point, point);
    while let Some(current) = node {
        if current.kind() == "string"
            && current.start_position().row < current.end_position().row
            && point_in_node(current, row, column)
        {
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
            if current.kind() == "string"
                && current.start_position().row < current.end_position().row
                && current.end_position() == point
            {
                return true;
            }
            node = current.parent();
        }
    }
    false
}

fn unclosed_delimiters_for_judge(
    tree: &Tree,
    source: &str,
    current_line: u32,
) -> Option<Vec<(u32, u32, char)>> {
    let mut stack = Vec::new();
    for row in 0..current_line as usize {
        let line = logical_line(source, row as u32)?;
        let byte_start = line_start_byte(source, row as u32)?;
        for (column, ch) in line.char_indices() {
            if !matches!(ch, '(' | '[' | '{' | ')' | ']' | '}' | '\"' | '\'' | '`') {
                continue;
            }
            let coverage = tree_coverage(tree, row, column, byte_start + column);
            if coverage == TreeCoverage::Masked {
                continue;
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
            if matches!(current.kind(), "string" | "comment") {
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
            .any(|(_, kinds)| kinds.contains(&kind))
    };
    let candidate = |kind| {
        expected
            .alternatives
            .iter()
            .find_map(|(column, kinds)| kinds.contains(&kind).then_some(*column))
    };

    if has_kind(IndentKind::ChainStart) {
        return match prefs.infix {
            FormPref::Aligned => expected
                .alternatives
                .iter()
                .filter_map(|(column, kinds)| {
                    (kinds.contains(&IndentKind::ChainStart) && *column > expected.primary)
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
    use crate::linting::lint_for_judge_test;

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
            vec![(5, vec![IndentKind::OpenerAligned, IndentKind::ChainStart])]
        );

        let same_line_call = repaired_expectation("long_function_name(x <-\n", 1);
        assert_eq!(same_line_call.primary, 4);
        assert_eq!(
            same_line_call.alternatives,
            vec![
                (21, vec![IndentKind::OpenerAligned]),
                (19, vec![IndentKind::ChainStart]),
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
    fn selection_never_targets_top_level_alternative() {
        let expected = LineIndentExpectation {
            primary: 4,
            alternatives: vec![(0, vec![IndentKind::TopLevel])],
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
