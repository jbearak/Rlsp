//! Judge-backed smart indentation over a repaired virtual buffer.

use tower_lsp::lsp_types::Position;
use tree_sitter::{Node, Tree};

use super::calculator::{IndentationConfig, IndentationStyle};
use super::context::unclosed_delimiters_heuristic;
use crate::linting::{
    IndentKind, InfixContinuationStyle, LineIndentExpectation, accepted_indents_for_line,
};
use crate::parser_pool::with_parser;
use crate::utf16::utf16_column_to_byte_offset;

const SENTINEL: &str = "raven_sentinel_";

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

    let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();
    if source.is_empty() || source.ends_with('\n') {
        lines.push(String::new());
    }

    let probe_line = position.line as usize;
    let Some(probe) = lines.get(probe_line) else {
        log::trace!(
            "judge_backed_indentation: bail: line {} is out of bounds ({} lines)",
            position.line,
            lines.len()
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

    if cursor_is_inside_multiline_string(tree, source, position)
        || lexically_inside_multiline_string(source, position)
    {
        log::trace!(
            "judge_backed_indentation: bail: cursor on line {} is inside a multiline string",
            position.line
        );
        return None;
    }

    let add_sentinel = needs_sentinel(probe);
    let pushed_closers_only = {
        let trimmed = probe.trim_start();
        !trimmed.is_empty() && trimmed.chars().all(|ch| matches!(ch, ')' | ']' | '}'))
    };
    log::trace!(
        "judge_backed_indentation: probe line={}, kind={}",
        position.line,
        if pushed_closers_only {
            "sentinel-replacing-pushed-closers"
        } else if add_sentinel {
            "sentinel"
        } else {
            "existing-content"
        }
    );
    let pushed_closer_line = if add_sentinel {
        let content_start = probe
            .char_indices()
            .find_map(|(index, ch)| (!ch.is_whitespace()).then_some(index))
            .unwrap_or(probe.len());
        if pushed_closers_only {
            let pushed = lines[probe_line][content_start..].to_owned();
            lines[probe_line].truncate(content_start);
            lines[probe_line].push_str(SENTINEL);
            Some(pushed)
        } else {
            lines[probe_line].insert_str(content_start, SENTINEL);
            None
        }
    } else {
        None
    };

    let Some(openers) = unclosed_delimiters_heuristic(source, position.line) else {
        log::trace!(
            "judge_backed_indentation: bail: delimiter scan could not reach line {}",
            position.line
        );
        return None;
    };
    let closers: Vec<String> = openers
        .iter()
        .rev()
        .map(|(_, _, opener)| {
            matching_closer(*opener)
                .expect("delimiter scan only stores supported openers")
                .to_string()
        })
        .collect();
    log::trace!(
        "judge_backed_indentation: synthesized closers={:?}",
        closers
    );
    let synthesized_count = closers.len();
    lines.splice(probe_line + 1..probe_line + 1, closers);
    if let Some(pushed) = pushed_closer_line {
        lines.insert(probe_line + 1 + synthesized_count, pushed);
    }

    let virtual_text = lines.join("\n");
    let Some(tree) = with_parser(|parser| parser.parse(&virtual_text, None)) else {
        log::trace!("judge_backed_indentation: bail: virtual parse failed");
        return None;
    };
    let expected = accepted_indents_for_line(
        &virtual_text,
        tree.root_node(),
        config.tab_size,
        infix_style,
        position.line,
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

fn needs_sentinel(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return true;
    }

    trimmed
        .split_whitespace()
        .next()
        .is_some_and(|run| run.chars().all(|ch| matches!(ch, ')' | ']' | '}')))
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
    multiline_string_contains(tree.root_node(), position.line as usize, byte_col)
}

fn lexically_inside_multiline_string(source: &str, position: Position) -> bool {
    let mut quote = None;
    let mut quote_start_row = 0usize;
    let mut escaped = false;

    for (row, line) in source.lines().enumerate() {
        if row > position.line as usize {
            break;
        }
        let limit = if row == position.line as usize {
            utf16_column_to_byte_offset(line, position.character)
        } else {
            line.len()
        };
        let Some(prefix) = line.get(..limit) else {
            return false;
        };

        for ch in prefix.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            if quote.is_some() && ch == '\\' {
                escaped = true;
                continue;
            }
            if let Some(delimiter) = quote {
                if ch == delimiter {
                    quote = None;
                }
                continue;
            }
            if ch == '#' {
                break;
            }
            if matches!(ch, '\"' | '\'' | '`') {
                quote = Some(ch);
                quote_start_row = row;
            }
        }
        escaped = false;

        if row == position.line as usize {
            break;
        }
    }

    quote.is_some() && quote_start_row < position.line as usize
}

fn logical_line(source: &str, line: u32) -> Option<&str> {
    source.lines().nth(line as usize).or_else(|| {
        (source.ends_with('\n') && line as usize == source.lines().count()).then_some("")
    })
}

fn multiline_string_contains(node: Node<'_>, row: usize, column: usize) -> bool {
    if node.kind() == "string" && node.start_position().row < node.end_position().row {
        let start = node.start_position();
        let end = node.end_position();
        let after_start = row > start.row || (row == start.row && column >= start.column);
        let before_end = row < end.row || (row == end.row && column < end.column);
        if after_start && before_end {
            return true;
        }
    }

    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| multiline_string_contains(child, row, column))
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
        let mut lines: Vec<String> = source.lines().map(str::to_owned).collect();
        if source.is_empty() || source.ends_with('\n') {
            lines.push(String::new());
        }
        let original = lines[line as usize].trim_start();
        let pushed_closers = (!original.is_empty()
            && original.chars().all(|ch| matches!(ch, ')' | ']' | '}')))
        .then(|| original.to_owned());
        let retained = pushed_closers.as_deref().map_or(original, |_| "");
        lines[line as usize] = format!("{}{}{}", " ".repeat(column as usize), SENTINEL, retained);
        let closers: Vec<String> = unclosed_delimiters_heuristic(source, line)
            .expect("test cursor line must be reachable")
            .into_iter()
            .rev()
            .map(|(_, _, opener)| {
                matching_closer(opener)
                    .expect("scan only records delimiters")
                    .to_string()
            })
            .collect();
        let synthesized_count = closers.len();
        lines.splice(line as usize + 1..line as usize + 1, closers);
        if let Some(pushed) = pushed_closers {
            lines.insert(line as usize + 1 + synthesized_count, pushed);
        }
        lines.join("\n")
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
