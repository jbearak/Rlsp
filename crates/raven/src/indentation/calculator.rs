//! Indentation calculation for R smart indentation.
//!
//! This module computes the correct indentation amount based on the
//! detected context and user configuration (tab size, style preference).

use super::context::IndentContext;

/// Configuration for indentation calculation.
#[derive(Debug, Clone, PartialEq)]
pub struct IndentationConfig {
    /// Number of spaces per indentation level.
    pub tab_size: u32,
    /// Whether to use spaces (true) or tabs (false) for indentation.
    pub insert_spaces: bool,
    /// The indentation style to use.
    pub style: IndentationStyle,
}

impl Default for IndentationConfig {
    fn default() -> Self {
        Self {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        }
    }
}

/// Indentation style variants.
///
/// These correspond to common R coding conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndentationStyle {
    /// RStudio style: same-line arguments align to opening paren,
    /// next-line arguments indent +tab_size from function line.
    #[default]
    RStudio,
    /// RStudio-minus style: all arguments indent +tab_size from the
    /// opener line (the line containing the opening paren), ignoring the
    /// paren column.
    RStudioMinus,
    /// Off: disable Tier 2 AST-aware indentation entirely.
    /// The onTypeFormatting handler returns None (no edits),
    /// leaving only Tier 1 declarative rules active.
    Off,
}

/// Calculates the target indentation column based on context and configuration.
///
/// # Arguments
///
/// * `context` - The detected syntactic context
/// * `config` - User configuration for tab size and style
/// * `source` - The source code text (used for line indent lookups)
///
/// # Returns
///
/// The target column number for indentation (0-indexed).
pub fn calculate_indentation(
    context: IndentContext,
    config: IndentationConfig,
    source: &str,
) -> u32 {
    match context {
        IndentContext::AfterContinuationOperator {
            chain_start_line,
            chain_start_col,
            operator_type: _,
            flattened_rhs,
        } => {
            let line_indent = get_line_indent(source, chain_start_line, config.tab_size);
            if flattened_rhs {
                // Chain on the RHS of a broken assignment: the assignment
                // already added the level, so continuations stay at the
                // chain-start line's indent (not chain_start_col, which can
                // sit mid-line in mixed chains) — every lint mode flags a
                // second level here (lintr's assignment_as_infix, #611).
                // The suppressor-derived floor keeps malformed input (a
                // chain line itself under-indented) at the level the lint
                // requires instead of mirroring the bad indent; for
                // well-formed input the two are equal.
                std::cmp::max(
                    line_indent,
                    flattened_floor(source, chain_start_line, &config),
                )
            } else {
                // Align to chain start column (RHS of assignment if present)
                // but ensure at least one tab_size indent from the line start.
                std::cmp::max(chain_start_col, line_indent.saturating_add(config.tab_size))
            }
        }
        IndentContext::AfterAssignmentOperator {
            base_line,
            flattened,
        } => {
            // One level from the assignment statement's start line, in every
            // style — an assignment RHS has nothing to align against (#611).
            // A flattened assignment (itself the RHS of an enclosing broken
            // assignment) adds no level of its own, but the enclosing
            // suppressor still floors it (see the flattened chain arm).
            let base = get_line_indent(source, base_line, config.tab_size);
            if flattened {
                std::cmp::max(base, flattened_floor(source, base_line, &config))
            } else {
                base.saturating_add(config.tab_size)
            }
        }
        IndentContext::InsideParens {
            opener_line,
            opener_col,
            has_content_on_opener_line,
        } => {
            match config.style {
                IndentationStyle::RStudio => {
                    if has_content_on_opener_line {
                        // Align to column after opening paren
                        opener_col.saturating_add(1)
                    } else {
                        // Indent from function line
                        get_line_indent(source, opener_line, config.tab_size)
                            .saturating_add(config.tab_size)
                    }
                }
                IndentationStyle::RStudioMinus => {
                    // Always indent from opener line + tab_size
                    get_line_indent(source, opener_line, config.tab_size)
                        .saturating_add(config.tab_size)
                }
                IndentationStyle::Off => {
                    // Off should be handled before reaching calculate_indentation
                    // (the handler returns None early). Fallback to basic indent.
                    get_line_indent(source, opener_line, config.tab_size)
                        .saturating_add(config.tab_size)
                }
            }
        }
        IndentContext::InsideBraces { opener_line, .. } => {
            // Brace block: indent from brace line
            get_line_indent(source, opener_line, config.tab_size).saturating_add(config.tab_size)
        }
        IndentContext::ClosingDelimiter { opener_line, .. } => {
            // Closing delimiter: align to opener line indentation
            get_line_indent(source, opener_line, config.tab_size)
        }
        IndentContext::AfterCompleteExpression {
            enclosing_block_indent,
        } => {
            // Complete expression: return to enclosing block indentation
            enclosing_block_indent
        }
    }
}

/// The minimum column for a flattened construct: one level below the
/// topmost line of the suppressing-assignment run above `line` (see
/// [`super::context::flattening_suppressor_line`]), or 0 when the text walk
/// finds none (then the observed line indent stands unchallenged).
fn flattened_floor(source: &str, line: u32, config: &IndentationConfig) -> u32 {
    super::context::flattening_suppressor_line(source, line)
        .map(|l| get_line_indent(source, l, config.tab_size).saturating_add(config.tab_size))
        .unwrap_or(0)
}

/// Gets the indentation (leading whitespace column) of a specific line.
///
/// # Arguments
///
/// * `source` - The source code text
/// * `line` - The line number (0-indexed)
/// * `tab_size` - The number of columns a tab character represents
///
/// # Returns
///
/// The column of the first non-whitespace character on the line.
pub fn get_line_indent(source: &str, line: u32, tab_size: u32) -> u32 {
    source
        .lines()
        .nth(line as usize)
        .map(|l| {
            l.chars()
                .take_while(|c| c.is_whitespace())
                .map(|c| if c == '\t' { tab_size } else { 1 })
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_indentation_config_default() {
        let config = IndentationConfig::default();
        assert_eq!(config.tab_size, 2);
        assert!(config.insert_spaces);
        assert_eq!(config.style, IndentationStyle::RStudio);
    }

    #[test]
    fn test_indentation_style_default() {
        let style = IndentationStyle::default();
        assert_eq!(style, IndentationStyle::RStudio);
    }

    #[test]
    fn test_get_line_indent() {
        let source = "no indent\n  two spaces\n    four spaces";
        assert_eq!(get_line_indent(source, 0, 1), 0);
        assert_eq!(get_line_indent(source, 1, 1), 2);
        assert_eq!(get_line_indent(source, 2, 1), 4);
    }

    #[test]
    fn test_get_line_indent_empty_line() {
        let source = "first\n\nthird";
        assert_eq!(get_line_indent(source, 1, 1), 0);
    }

    #[test]
    fn test_get_line_indent_out_of_bounds() {
        let source = "only one line";
        assert_eq!(get_line_indent(source, 10, 1), 0);
    }

    #[test]
    fn test_get_line_indent_whitespace_only_line() {
        // Lines with only whitespace should return the whitespace count
        let source = "first\n    \nthird";
        assert_eq!(get_line_indent(source, 1, 1), 4);
    }

    #[test]
    fn test_get_line_indent_with_tabs() {
        // Tab characters counted with tab_size=1 (each tab = 1 column)
        let source = "\tfirst\n\t\tsecond\n\t  mixed";
        assert_eq!(get_line_indent(source, 0, 1), 1); // 1 tab
        assert_eq!(get_line_indent(source, 1, 1), 2); // 2 tabs
        assert_eq!(get_line_indent(source, 2, 1), 3); // 1 tab + 2 spaces
    }

    #[test]
    fn test_get_line_indent_with_tabs_tab_size_4() {
        // Tab characters expand to tab_size columns each
        let source = "\tx\n\t\tx\n\t  x\n";
        assert_eq!(get_line_indent(source, 0, 4), 4); // 1 tab * 4
        assert_eq!(get_line_indent(source, 1, 4), 8); // 2 tabs * 4
        assert_eq!(get_line_indent(source, 2, 4), 6); // 1 tab * 4 + 2 spaces
    }

    #[test]
    fn test_get_line_indent_mixed_whitespace() {
        // Mixed tabs and spaces with tab_size=1
        let source = "  \t  code";
        assert_eq!(get_line_indent(source, 0, 1), 5); // 2 spaces + 1 tab(=1) + 2 spaces
    }

    #[test]
    fn test_get_line_indent_mixed_whitespace_tab_size_4() {
        // Mixed tabs and spaces with tab_size=4
        let source = "  \t  code";
        assert_eq!(get_line_indent(source, 0, 4), 8); // 2 spaces + 1 tab(=4) + 2 spaces
    }

    #[test]
    fn test_get_line_indent_empty_source() {
        // Empty source string
        let source = "";
        assert_eq!(get_line_indent(source, 0, 1), 0);
    }

    #[test]
    fn test_get_line_indent_single_newline() {
        // Source with just a newline
        let source = "\n";
        assert_eq!(get_line_indent(source, 0, 1), 0);
        assert_eq!(get_line_indent(source, 1, 1), 0);
    }

    // ========================================================================
    // Pipe Chain Continuation Tests (Task 5.1)
    // ========================================================================

    #[test]
    fn test_pipe_chain_continuation_basic() {
        use super::super::context::OperatorType;

        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // Chain starts at column 0, so continuation should be at column 2
        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 0,
            operator_type: OperatorType::Pipe,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 2);
    }

    #[test]
    fn test_pipe_chain_continuation_with_offset() {
        use super::super::context::OperatorType;

        let config = IndentationConfig {
            tab_size: 4,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // Pipe chain start at col 4 (RHS of assignment), line 0 has no indent.
        // Formula: max(4, 0+4) = 4
        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 4,
            operator_type: OperatorType::MagrittrPipe,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 4);
    }

    #[test]
    fn test_non_pipe_chain_continuation_with_offset() {
        use super::super::context::OperatorType;

        let config = IndentationConfig {
            tab_size: 4,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // Plus chain start at col 4, no line indent: max(4, 0+4) = 4
        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 4,
            operator_type: OperatorType::Plus,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 4);
    }

    #[test]
    fn test_pipe_chain_continuation_all_operators_at_col_zero() {
        use super::super::context::OperatorType;

        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // When chain starts at column 0 with no line indent, all operators
        // produce the same result (tab_size = 2):
        // - Pipe/MagrittrPipe: max(0, 0+2) = 2
        // - Others: 0 + 2 = 2
        let operators = [
            OperatorType::Pipe,
            OperatorType::MagrittrPipe,
            OperatorType::Plus,
            OperatorType::Tilde,
            OperatorType::CustomInfix,
        ];

        for op in operators {
            let context = IndentContext::AfterContinuationOperator {
                chain_start_line: 0,
                chain_start_col: 0,
                operator_type: op,
                flattened_rhs: false,
            };

            let indent = calculate_indentation(context.clone(), config.clone(), "");
            assert_eq!(
                indent, 2,
                "Operator {:?} at col 0 should produce indent 2",
                op
            );
        }
    }

    #[test]
    fn test_all_operators_same_indent_with_offset() {
        use super::super::context::OperatorType;

        // All operators at col 5 (RHS of assignment): max(5, 0+2) = 5
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        for op in [
            OperatorType::Pipe,
            OperatorType::MagrittrPipe,
            OperatorType::Plus,
            OperatorType::Tilde,
            OperatorType::CustomInfix,
        ] {
            let ctx = IndentContext::AfterContinuationOperator {
                chain_start_line: 0,
                chain_start_col: 5,
                operator_type: op,
                flattened_rhs: false,
            };
            assert_eq!(
                calculate_indentation(ctx, config.clone(), ""),
                5,
                "Operator {:?} at col 5 should produce indent 5",
                op
            );
        }
    }

    #[test]
    fn test_flattened_chain_uses_chain_line_indent() {
        use super::super::context::OperatorType;

        // Chain on the RHS of a broken assignment (#611): the one-level
        // floor is dropped and the continuation sits at the chain-start
        // line's indent — even when the sub-chain starts mid-line
        // (chain_start_col deeper than the line indent).
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };
        let source = "result <-\n  a + data %>%\n";
        let ctx = IndentContext::AfterContinuationOperator {
            chain_start_line: 1,
            chain_start_col: 6,
            operator_type: OperatorType::MagrittrPipe,
            flattened_rhs: true,
        };
        assert_eq!(calculate_indentation(ctx, config, source), 2);
    }

    #[test]
    fn test_after_assignment_operator_one_level_from_base_line() {
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };
        let source = "f <- function() {\n  x <-\n";
        let ctx = IndentContext::AfterAssignmentOperator {
            base_line: 1,
            flattened: false,
        };
        assert_eq!(calculate_indentation(ctx, config, source), 4);
    }

    #[test]
    fn test_after_assignment_operator_flattened_adds_no_level() {
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };
        let source = "a <-\n  b <-\n";
        let ctx = IndentContext::AfterAssignmentOperator {
            base_line: 1,
            flattened: true,
        };
        assert_eq!(calculate_indentation(ctx, config, source), 2);
    }

    #[test]
    fn test_after_assignment_operator_style_independent() {
        // The assignment context ignores the paren-alignment style choice.
        let source = "result <-\n";
        for style in [IndentationStyle::RStudio, IndentationStyle::RStudioMinus] {
            let config = IndentationConfig {
                tab_size: 2,
                insert_spaces: true,
                style,
            };
            let ctx = IndentContext::AfterAssignmentOperator {
                base_line: 0,
                flattened: false,
            };
            assert_eq!(
                calculate_indentation(ctx, config, source),
                2,
                "style {style:?}"
            );
        }
    }

    #[test]
    fn test_pipe_chain_continuation_uniform_across_lines() {
        use super::super::context::OperatorType;

        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // Simulate multiple continuation lines in a chain
        // All should get the same indentation regardless of which line they're on
        let chain_start_col = 0;
        let expected_indent = chain_start_col + config.tab_size;

        for line in 1..5 {
            let context = IndentContext::AfterContinuationOperator {
                chain_start_line: 0,
                chain_start_col,
                operator_type: OperatorType::Pipe,
                flattened_rhs: false,
            };

            let indent = calculate_indentation(context, config.clone(), "");
            assert_eq!(
                indent, expected_indent,
                "Line {} should have same indent as other continuation lines",
                line
            );
        }
    }

    #[test]
    fn test_pipe_chain_continuation_various_tab_sizes() {
        use super::super::context::OperatorType;

        let chain_start_col = 0;

        for tab_size in [1, 2, 4, 8] {
            let config = IndentationConfig {
                tab_size,
                insert_spaces: true,
                style: IndentationStyle::RStudio,
            };

            let context = IndentContext::AfterContinuationOperator {
                chain_start_line: 0,
                chain_start_col,
                operator_type: OperatorType::Pipe,
                flattened_rhs: false,
            };

            let indent = calculate_indentation(context, config, "");
            assert_eq!(
                indent,
                chain_start_col + tab_size,
                "Tab size {} should produce indent {}",
                tab_size,
                chain_start_col + tab_size
            );
        }
    }

    #[test]
    fn test_pipe_chain_continuation_style_independent() {
        use super::super::context::OperatorType;

        // Pipe chain indentation should be the same regardless of style setting
        // (style only affects function argument alignment)
        // chain_start_col=4, line_indent=0, tab_size=2 → max(4, 0+2) = 4
        let chain_start_col = 4;
        let tab_size = 2;
        let expected_indent = 4; // max(4, 0+2) = 4

        for style in [IndentationStyle::RStudio, IndentationStyle::RStudioMinus] {
            let config = IndentationConfig {
                tab_size,
                insert_spaces: true,
                style,
            };

            let context = IndentContext::AfterContinuationOperator {
                chain_start_line: 0,
                chain_start_col,
                operator_type: OperatorType::Pipe,
                flattened_rhs: false,
            };

            let indent = calculate_indentation(context, config, "");
            assert_eq!(
                indent, expected_indent,
                "Style {:?} should not affect pipe chain indentation",
                style
            );
        }
    }

    // ========================================================================
    // Function Argument Alignment Tests (Task 5.3)
    // ========================================================================

    #[test]
    fn test_inside_parens_rstudio_same_line_content() {
        // RStudio style: when there's content after the opening paren,
        // align to the column after the opening paren (opener_col + 1)
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // func(arg1,  <- opener at column 4, has content
        //      ^-- should align to column 5
        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 4,
            has_content_on_opener_line: true,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 5); // opener_col + 1
    }

    #[test]
    fn test_inside_parens_rstudio_next_line() {
        // RStudio style: when opening paren is followed by newline,
        // indent from function line + tab_size
        let source = "func(\n";
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // func(  <- opener at column 4, no content after
        //   ^-- should indent from line indent (0) + tab_size (2) = 2
        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 4,
            has_content_on_opener_line: false,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // get_line_indent(0) + tab_size = 0 + 2
    }

    #[test]
    fn test_inside_parens_rstudio_next_line_with_indent() {
        // RStudio style with indented function line
        let source = "  func(\n";
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // "  func(" <- line has 2 spaces indent, opener at column 6
        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 6,
            has_content_on_opener_line: false,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 4); // get_line_indent(0) + tab_size = 2 + 2
    }

    #[test]
    fn test_inside_parens_rstudio_minus_always_indent() {
        // RStudio-minus style: always indent from opener line + tab_size,
        // regardless of whether there's content after the paren
        let source = "func(arg1,\n";
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudioMinus,
        };

        // Even with content on opener line, RStudio-minus ignores it
        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 4,
            has_content_on_opener_line: true,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // get_line_indent(0) + tab_size = 0 + 2
    }

    #[test]
    fn test_inside_parens_rstudio_minus_no_content() {
        // RStudio-minus style with no content after paren
        let source = "func(\n";
        let config = IndentationConfig {
            tab_size: 4,
            insert_spaces: true,
            style: IndentationStyle::RStudioMinus,
        };

        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 4,
            has_content_on_opener_line: false,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 4); // get_line_indent(0) + tab_size = 0 + 4
    }

    #[test]
    fn test_inside_parens_various_tab_sizes() {
        let source = "func(\n";

        for tab_size in [1, 2, 4, 8] {
            let config = IndentationConfig {
                tab_size,
                insert_spaces: true,
                style: IndentationStyle::RStudio,
            };

            let context = IndentContext::InsideParens {
                opener_line: 0,
                opener_col: 4,
                has_content_on_opener_line: false,
            };

            let indent = calculate_indentation(context, config, source);
            assert_eq!(
                indent, tab_size,
                "Tab size {} should produce indent {}",
                tab_size, tab_size
            );
        }
    }

    // ========================================================================
    // Brace Block Indentation Tests (Task 5.3)
    // ========================================================================

    #[test]
    fn test_inside_braces_basic() {
        // Brace block: indent from brace line + tab_size
        let source = "if (TRUE) {\n";
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::InsideBraces {
            opener_line: 0,
            opener_col: 10, // This is the brace column, but we use line indent
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // get_line_indent(0) + tab_size = 0 + 2
    }

    #[test]
    fn test_inside_braces_with_indent() {
        // Brace block with indented opener line
        let source = "  if (TRUE) {\n";
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::InsideBraces {
            opener_line: 0,
            opener_col: 12,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 4); // get_line_indent(0) + tab_size = 2 + 2
    }

    #[test]
    fn test_inside_braces_various_tab_sizes() {
        let source = "{\n";

        for tab_size in [1, 2, 4, 8] {
            let config = IndentationConfig {
                tab_size,
                insert_spaces: true,
                style: IndentationStyle::RStudio,
            };

            let context = IndentContext::InsideBraces {
                opener_line: 0,
                opener_col: 0,
            };

            let indent = calculate_indentation(context, config, source);
            assert_eq!(
                indent, tab_size,
                "Tab size {} should produce indent {}",
                tab_size, tab_size
            );
        }
    }

    #[test]
    fn test_inside_braces_style_independent() {
        // Brace indentation should be the same regardless of style setting
        let source = "{\n";
        let tab_size = 2;

        for style in [IndentationStyle::RStudio, IndentationStyle::RStudioMinus] {
            let config = IndentationConfig {
                tab_size,
                insert_spaces: true,
                style,
            };

            let context = IndentContext::InsideBraces {
                opener_line: 0,
                opener_col: 0,
            };

            let indent = calculate_indentation(context, config, source);
            assert_eq!(
                indent, tab_size,
                "Style {:?} should not affect brace indentation",
                style
            );
        }
    }

    // ========================================================================
    // De-indentation Tests (Task 5.5)
    // ========================================================================

    #[test]
    fn test_closing_delimiter_basic() {
        // Closing delimiter should align to opener line indentation
        let source = "func(\n  arg1\n)";
        let config = IndentationConfig::default();

        let context = IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 4,
            delimiter: ')',
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 0); // opener line has no indentation
    }

    #[test]
    fn test_closing_delimiter_with_indented_opener() {
        // Closing delimiter should align to opener line indentation
        let source = "  func(\n    arg1\n  )";
        let config = IndentationConfig::default();

        let context = IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 6,
            delimiter: ')',
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // opener line has 2 spaces indentation
    }

    #[test]
    fn test_closing_delimiter_brace() {
        // Closing brace should align to opener line indentation
        let source = "    if (TRUE) {\n      x <- 1\n    }";
        let config = IndentationConfig::default();

        let context = IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 14,
            delimiter: '}',
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 4); // opener line has 4 spaces indentation
    }

    #[test]
    fn test_closing_delimiter_bracket() {
        // Closing bracket should align to opener line indentation
        let source = "  x[\n    1\n  ]";
        let config = IndentationConfig::default();

        let context = IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 3,
            delimiter: ']',
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // opener line has 2 spaces indentation
    }

    #[test]
    fn test_closing_delimiter_style_independent() {
        // Closing delimiter alignment should be style-independent
        let source = "  func(\n    arg1\n  )";

        for style in [IndentationStyle::RStudio, IndentationStyle::RStudioMinus] {
            let config = IndentationConfig {
                tab_size: 2,
                insert_spaces: true,
                style,
            };

            let context = IndentContext::ClosingDelimiter {
                opener_line: 0,
                opener_col: 6,
                delimiter: ')',
            };

            let indent = calculate_indentation(context, config, source);
            assert_eq!(
                indent, 2,
                "Style {:?} should not affect closing delimiter alignment",
                style
            );
        }
    }

    #[test]
    fn test_after_complete_expression_basic() {
        // After complete expression, return to enclosing block indentation
        let config = IndentationConfig::default();

        let context = IndentContext::AfterCompleteExpression {
            enclosing_block_indent: 0,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 0);
    }

    #[test]
    fn test_after_complete_expression_with_indent() {
        // After complete expression in indented block
        let config = IndentationConfig::default();

        let context = IndentContext::AfterCompleteExpression {
            enclosing_block_indent: 4,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 4);
    }

    #[test]
    fn test_after_complete_expression_various_indents() {
        let config = IndentationConfig::default();

        for enclosing_indent in [0, 2, 4, 6, 8, 10] {
            let context = IndentContext::AfterCompleteExpression {
                enclosing_block_indent: enclosing_indent,
            };

            let indent = calculate_indentation(context, config.clone(), "");
            assert_eq!(
                indent, enclosing_indent,
                "Enclosing indent {} should be returned as-is",
                enclosing_indent
            );
        }
    }

    #[test]
    fn test_after_complete_expression_style_independent() {
        // Complete expression de-indentation should be style-independent
        for style in [IndentationStyle::RStudio, IndentationStyle::RStudioMinus] {
            let config = IndentationConfig {
                tab_size: 2,
                insert_spaces: true,
                style,
            };

            let context = IndentContext::AfterCompleteExpression {
                enclosing_block_indent: 4,
            };

            let indent = calculate_indentation(context, config, "");
            assert_eq!(
                indent, 4,
                "Style {:?} should not affect complete expression de-indentation",
                style
            );
        }
    }

    // ========================================================================
    // Property-Based Tests (Task 5.2)
    // ========================================================================

    // ========================================================================
    // Edge Case Tests (Task 5.8)
    // ========================================================================

    // --- Column 0 Edge Cases ---

    #[test]
    fn test_edge_case_chain_start_at_column_0() {
        use super::super::context::OperatorType;

        // Chain start at column 0 should produce indentation of just tab_size
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 0,
            operator_type: OperatorType::Pipe,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 2); // 0 + 2 = 2
    }

    #[test]
    fn test_edge_case_opener_at_column_0() {
        // Opener at column 0 with content should align to column 1
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 0,
            has_content_on_opener_line: true,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 1); // 0 + 1 = 1
    }

    #[test]
    fn test_edge_case_opener_at_column_0_no_content() {
        // Opener at column 0 with no content should use line indent + tab_size
        let source = "(\n";
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 0,
            has_content_on_opener_line: false,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // line indent 0 + tab_size 2 = 2
    }

    #[test]
    fn test_edge_case_brace_at_column_0() {
        // Brace at column 0 should indent by tab_size
        let source = "{\n";
        let config = IndentationConfig {
            tab_size: 4,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::InsideBraces {
            opener_line: 0,
            opener_col: 0,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 4); // line indent 0 + tab_size 4 = 4
    }

    #[test]
    fn test_edge_case_enclosing_block_indent_0() {
        // Enclosing block indent of 0 should return 0
        let config = IndentationConfig::default();

        let context = IndentContext::AfterCompleteExpression {
            enclosing_block_indent: 0,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 0);
    }

    #[test]
    fn test_edge_case_closing_delimiter_opener_at_column_0() {
        // Closing delimiter with opener at column 0 should return 0
        let source = "(\n  arg\n)";
        let config = IndentationConfig::default();

        let context = IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 0,
            delimiter: ')',
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 0); // opener line has no indentation
    }

    // --- Very Large Indentation Edge Cases ---

    #[test]
    fn test_edge_case_chain_start_at_column_100_pipe() {
        use super::super::context::OperatorType;

        // Pipe chain start at column 100 with no line indent: max(100, 0+2) = 100
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 100,
            operator_type: OperatorType::Pipe,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 100); // max(100, 0+2) = 100
    }

    #[test]
    fn test_edge_case_chain_start_at_column_100_plus() {
        use super::super::context::OperatorType;

        // Plus chain start at column 100, no line indent: max(100, 0+2) = 100
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 100,
            operator_type: OperatorType::Plus,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 100); // max(100, 0+2) = 100
    }

    #[test]
    fn test_edge_case_opener_at_column_100() {
        // Opener at column 100+ with content should align to column 101
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 100,
            has_content_on_opener_line: true,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 101); // 100 + 1 = 101
    }

    #[test]
    fn test_edge_case_large_tab_size() {
        use super::super::context::OperatorType;

        // Large tab_size (8) should work correctly
        let config = IndentationConfig {
            tab_size: 8,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 0,
            operator_type: OperatorType::Pipe,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 8); // 0 + 8 = 8
    }

    #[test]
    fn test_edge_case_large_enclosing_block_indent() {
        // Large enclosing block indent should be returned as-is
        let config = IndentationConfig::default();

        let context = IndentContext::AfterCompleteExpression {
            enclosing_block_indent: 200,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 200);
    }

    #[test]
    fn test_edge_case_very_large_line_indent() {
        // Very large line indentation should work correctly
        let source = format!("{}func(\n", " ".repeat(150));
        let config = IndentationConfig {
            tab_size: 4,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 154, // 150 spaces + "func"
            has_content_on_opener_line: false,
        };

        let indent = calculate_indentation(context, config, &source);
        assert_eq!(indent, 154); // line indent 150 + tab_size 4 = 154
    }

    #[test]
    fn test_edge_case_closing_delimiter_large_indent() {
        // Closing delimiter with large opener line indent
        let source = format!("{}func(\n", " ".repeat(100));
        let config = IndentationConfig::default();

        let context = IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 104,
            delimiter: ')',
        };

        let indent = calculate_indentation(context, config, &source);
        assert_eq!(indent, 100); // opener line has 100 spaces indentation
    }

    // --- Invalid Positions / Missing Openers Edge Cases ---

    #[test]
    fn test_edge_case_out_of_bounds_line_number() {
        // get_line_indent should handle out-of-bounds line numbers gracefully
        let source = "only one line";
        assert_eq!(get_line_indent(source, 0, 1), 0);
        assert_eq!(get_line_indent(source, 1, 1), 0); // out of bounds
        assert_eq!(get_line_indent(source, 100, 1), 0); // way out of bounds
        assert_eq!(get_line_indent(source, u32::MAX, 1), 0); // maximum value
    }

    #[test]
    fn test_edge_case_empty_source_string() {
        // Empty source string should be handled gracefully
        let source = "";
        assert_eq!(get_line_indent(source, 0, 1), 0);

        // Calculation with empty source should still work
        let config = IndentationConfig::default();

        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 4,
            has_content_on_opener_line: false,
        };

        // get_line_indent returns 0 for empty source, so indent = 0 + tab_size
        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // 0 + 2 = 2
    }

    #[test]
    fn test_edge_case_opener_line_out_of_bounds() {
        // When opener_line is out of bounds, get_line_indent returns 0
        let source = "single line";
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        // Opener line 10 doesn't exist in single-line source
        let context = IndentContext::InsideParens {
            opener_line: 10,
            opener_col: 4,
            has_content_on_opener_line: false,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // get_line_indent returns 0, so 0 + 2 = 2
    }

    #[test]
    fn test_edge_case_closing_delimiter_opener_line_out_of_bounds() {
        // Closing delimiter with out-of-bounds opener line
        let source = "single line";
        let config = IndentationConfig::default();

        let context = IndentContext::ClosingDelimiter {
            opener_line: 100, // doesn't exist
            opener_col: 4,
            delimiter: ')',
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 0); // get_line_indent returns 0 for out-of-bounds
    }

    #[test]
    fn test_edge_case_brace_opener_line_out_of_bounds() {
        // Brace with out-of-bounds opener line
        let source = "single line";
        let config = IndentationConfig {
            tab_size: 4,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::InsideBraces {
            opener_line: 50, // doesn't exist
            opener_col: 10,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 4); // get_line_indent returns 0, so 0 + 4 = 4
    }

    // --- Configuration Defaults Edge Cases ---

    #[test]
    fn test_edge_case_default_config_pipe_chain() {
        use super::super::context::OperatorType;

        // Default config should work correctly with pipe chains
        let config = IndentationConfig::default();
        assert_eq!(config.tab_size, 2);
        assert!(config.insert_spaces);
        assert_eq!(config.style, IndentationStyle::RStudio);

        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 0,
            operator_type: OperatorType::Pipe,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 2); // default tab_size is 2
    }

    #[test]
    fn test_edge_case_default_config_inside_parens() {
        // Default config with InsideParens context
        let source = "func(\n";
        let config = IndentationConfig::default();

        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 4,
            has_content_on_opener_line: false,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // default tab_size is 2, line indent is 0
    }

    #[test]
    fn test_edge_case_default_config_inside_braces() {
        // Default config with InsideBraces context
        let source = "{\n";
        let config = IndentationConfig::default();

        let context = IndentContext::InsideBraces {
            opener_line: 0,
            opener_col: 0,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // default tab_size is 2
    }

    #[test]
    fn test_edge_case_default_config_closing_delimiter() {
        // Default config with ClosingDelimiter context
        let source = "  func(\n    arg\n  )";
        let config = IndentationConfig::default();

        let context = IndentContext::ClosingDelimiter {
            opener_line: 0,
            opener_col: 6,
            delimiter: ')',
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // opener line has 2 spaces indentation
    }

    #[test]
    fn test_edge_case_default_config_complete_expression() {
        // Default config with AfterCompleteExpression context
        let config = IndentationConfig::default();

        let context = IndentContext::AfterCompleteExpression {
            enclosing_block_indent: 4,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 4); // enclosing block indent is returned as-is
    }

    #[test]
    fn test_edge_case_tab_size_1() {
        use super::super::context::OperatorType;

        // Minimum tab_size of 1 should work correctly
        let config = IndentationConfig {
            tab_size: 1,
            insert_spaces: true,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 0,
            operator_type: OperatorType::Pipe,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 1); // 0 + 1 = 1
    }

    #[test]
    fn test_edge_case_insert_spaces_false() {
        use super::super::context::OperatorType;

        // insert_spaces=false should not affect calculation (only formatting)
        let config = IndentationConfig {
            tab_size: 4,
            insert_spaces: false,
            style: IndentationStyle::RStudio,
        };

        let context = IndentContext::AfterContinuationOperator {
            chain_start_line: 0,
            chain_start_col: 0,
            operator_type: OperatorType::Pipe,
            flattened_rhs: false,
        };

        let indent = calculate_indentation(context, config, "");
        assert_eq!(indent, 4); // calculation is the same regardless of insert_spaces
    }

    #[test]
    fn test_edge_case_rstudio_minus_default_behavior() {
        // RStudio-minus style should always use line indent + tab_size
        let source = "func(arg1,\n";
        let config = IndentationConfig {
            tab_size: 2,
            insert_spaces: true,
            style: IndentationStyle::RStudioMinus,
        };

        // Even with content on opener line, RStudio-minus ignores it
        let context = IndentContext::InsideParens {
            opener_line: 0,
            opener_col: 4,
            has_content_on_opener_line: true,
        };

        let indent = calculate_indentation(context, config, source);
        assert_eq!(indent, 2); // line indent 0 + tab_size 2 = 2, ignores opener_col
    }

    // ========================================================================
    // Property-Based Tests (Task 5.2)
    // ========================================================================

    mod property_tests {
        use super::*;
        use proptest::prelude::*;

        // Feature: r-smart-indentation, Property 2a: Pipe Chain Indentation Calculation
        // For pipe/magrittr operators, indentation = max(chain_start_col, line_indent + tab_size).
        // With source="" and chain_start_line=0, line_indent=0, so indent = max(C, T).
        // **Validates: Requirements 3.2**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_pipe_indentation_calculation(
                chain_start_col in 0u32..100,
                tab_size in 1u32..9,
                is_magrittr in proptest::bool::ANY,
                style_idx in 0usize..2,
            ) {
                use super::super::super::context::OperatorType;

                let operator_type = if is_magrittr {
                    OperatorType::MagrittrPipe
                } else {
                    OperatorType::Pipe
                };

                let style = match style_idx {
                    0 => IndentationStyle::RStudio,
                    _ => IndentationStyle::RStudioMinus,
                };

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style,
                };

                let context = IndentContext::AfterContinuationOperator {
                    chain_start_line: 0,
                    chain_start_col,
                    operator_type,
                    flattened_rhs: false,
                };

                let indent = calculate_indentation(context, config, "");

                // Property: indent = max(chain_start_col, 0 + tab_size)
                let expected = std::cmp::max(chain_start_col, tab_size);
                prop_assert_eq!(
                    indent,
                    expected,
                    "For pipe chain_start_col={}, tab_size={}, expected indent={}, got={}",
                    chain_start_col,
                    tab_size,
                    expected,
                    indent
                );
            }
        }

        // Feature: r-smart-indentation, Property 2b: Non-Pipe Chain Indentation Calculation
        // For non-pipe operators (+, ~, %word%), indentation = max(chain_start_col, line_indent + tab_size).
        // With source="" and chain_start_line=0, line_indent=0, so indent = max(C, T).
        // **Validates: Requirements 3.2**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_non_pipe_indentation_calculation(
                chain_start_col in 0u32..100,
                tab_size in 1u32..9,
                operator_type_idx in 0usize..3,
                style_idx in 0usize..2,
            ) {
                use super::super::super::context::OperatorType;

                let operator_type = match operator_type_idx {
                    0 => OperatorType::Plus,
                    1 => OperatorType::Tilde,
                    _ => OperatorType::CustomInfix,
                };

                let style = match style_idx {
                    0 => IndentationStyle::RStudio,
                    _ => IndentationStyle::RStudioMinus,
                };

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style,
                };

                let context = IndentContext::AfterContinuationOperator {
                    chain_start_line: 0,
                    chain_start_col,
                    operator_type,
                    flattened_rhs: false,
                };

                let indent = calculate_indentation(context, config, "");

                // Property: indent = max(chain_start_col, 0 + tab_size)
                let expected = std::cmp::max(chain_start_col, tab_size);
                prop_assert_eq!(
                    indent,
                    expected,
                    "For non-pipe chain_start_col={}, tab_size={}, expected indent={}, got={}",
                    chain_start_col,
                    tab_size,
                    expected,
                    indent
                );
            }
        }

        // Feature: r-smart-indentation, Property 3: Uniform Continuation Indentation
        // *For any* chain with multiple continuation lines of the same operator class
        // (pipe vs non-pipe), all continuation lines should receive identical indentation.
        // **Validates: Requirements 3.3**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_uniform_pipe_continuation_indentation(
                chain_start_col in 0u32..100,
                chain_start_line in 0u32..1000,
                tab_size in 1u32..9,
                num_continuation_lines in 2usize..20,
                use_magrittr in prop::collection::vec(proptest::bool::ANY, 2..20),
            ) {
                use super::super::super::context::OperatorType;

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style: IndentationStyle::RStudio,
                };

                let mut indentations = Vec::new();

                for (i, &is_mag) in use_magrittr.iter().take(num_continuation_lines).enumerate() {
                    let operator_type = if is_mag {
                        OperatorType::MagrittrPipe
                    } else {
                        OperatorType::Pipe
                    };
                    let context = IndentContext::AfterContinuationOperator {
                        chain_start_line,
                        chain_start_col,
                        operator_type,
                        flattened_rhs: false,
                    };

                    let indent = calculate_indentation(context, config.clone(), "");
                    indentations.push((i, indent));
                }

                // With source="" and chain_start_line pointing to empty: line_indent=0
                let expected_indent = std::cmp::max(chain_start_col, tab_size);
                for (line_idx, indent) in &indentations {
                    prop_assert_eq!(
                        *indent,
                        expected_indent,
                        "Pipe continuation line {} should have indent {}, got {}",
                        line_idx,
                        expected_indent,
                        indent
                    );
                }
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_uniform_non_pipe_continuation_indentation(
                chain_start_col in 0u32..100,
                chain_start_line in 0u32..1000,
                tab_size in 1u32..9,
                num_continuation_lines in 2usize..20,
                operator_types in prop::collection::vec(0usize..3, 2..20),
            ) {
                use super::super::super::context::OperatorType;

                let map_operator = |idx: usize| match idx % 3 {
                    0 => OperatorType::Plus,
                    1 => OperatorType::Tilde,
                    _ => OperatorType::CustomInfix,
                };

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style: IndentationStyle::RStudio,
                };

                let mut indentations = Vec::new();

                for (i, &op_idx) in operator_types.iter().take(num_continuation_lines).enumerate() {
                    let context = IndentContext::AfterContinuationOperator {
                        chain_start_line,
                        chain_start_col,
                        operator_type: map_operator(op_idx),
                        flattened_rhs: false,
                    };

                    let indent = calculate_indentation(context, config.clone(), "");
                    indentations.push((i, indent));
                }

                let expected_indent = std::cmp::max(chain_start_col, tab_size);
                for (line_idx, indent) in &indentations {
                    prop_assert_eq!(
                        *indent,
                        expected_indent,
                        "Non-pipe continuation line {} should have indent {}, got {}",
                        line_idx,
                        expected_indent,
                        indent
                    );
                }
            }
        }

        // Feature: r-smart-indentation, Property 4: Same-Line Argument Alignment (RStudio Style)
        // *For any* function call with RStudio style configured, where the opening parenthesis
        // is followed by content on the same line, the computed indentation for continuation
        // arguments should equal the column immediately after the opening parenthesis (opener_col + 1).
        // **Validates: Requirements 4.1**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_same_line_argument_alignment_rstudio(
                opener_col in 0u32..100,
                tab_size in 1u32..9,
            ) {
                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style: IndentationStyle::RStudio,
                };

                // RStudio style with content on opener line: align to opener_col + 1
                let context = IndentContext::InsideParens {
                    opener_line: 0,
                    opener_col,
                    has_content_on_opener_line: true,
                };

                let indent = calculate_indentation(context, config, "");

                // Property: indentation should equal opener_col + 1
                prop_assert_eq!(
                    indent,
                    opener_col + 1,
                    "For opener_col={}, expected indent={}, got={}",
                    opener_col,
                    opener_col + 1,
                    indent
                );
            }
        }

        // Feature: r-smart-indentation, Property 5: Next-Line Argument Indentation
        // *For any* function call where the opening parenthesis is followed by a newline,
        // the computed indentation for the first argument should equal the indentation of
        // the line containing the opening parenthesis plus tab_size.
        // **Validates: Requirements 4.2**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_next_line_argument_indentation(
                line_indent in 0u32..50,
                opener_col in 0u32..100,
                tab_size in 1u32..9,
            ) {
                // Generate source with the specified line indentation
                let source = format!("{}func(\n", " ".repeat(line_indent as usize));

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style: IndentationStyle::RStudio,
                };

                // RStudio style with no content on opener line: indent from line + tab_size
                let context = IndentContext::InsideParens {
                    opener_line: 0,
                    opener_col,
                    has_content_on_opener_line: false,
                };

                let indent = calculate_indentation(context, config, &source);

                // Property: indentation should equal line_indent + tab_size
                prop_assert_eq!(
                    indent,
                    line_indent + tab_size,
                    "For line_indent={}, tab_size={}, expected indent={}, got={}",
                    line_indent,
                    tab_size,
                    line_indent + tab_size,
                    indent
                );
            }
        }

        // Feature: r-smart-indentation, Property 6: RStudio-Minus Style Indentation
        // *For any* function call with RStudio-minus style configured, the computed indentation
        // for continuation arguments should equal the indentation of the previous line plus tab_size,
        // regardless of whether the opening parenthesis is followed by content or a newline.
        // **Validates: Requirements 4.3**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_rstudio_minus_style_indentation(
                line_indent in 0u32..50,
                opener_col in 0u32..100,
                tab_size in 1u32..9,
                has_content in proptest::bool::ANY,
            ) {
                // Generate source with the specified line indentation
                let source = format!("{}func(\n", " ".repeat(line_indent as usize));

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style: IndentationStyle::RStudioMinus,
                };

                // RStudio-minus style: always indent from opener line + tab_size
                // regardless of has_content_on_opener_line
                let context = IndentContext::InsideParens {
                    opener_line: 0,
                    opener_col,
                    has_content_on_opener_line: has_content,
                };

                let indent = calculate_indentation(context, config, &source);

                // Property: indentation should equal line_indent + tab_size
                // regardless of whether there's content on the opener line
                prop_assert_eq!(
                    indent,
                    line_indent + tab_size,
                    "For line_indent={}, tab_size={}, has_content={}, expected indent={}, got={}",
                    line_indent,
                    tab_size,
                    has_content,
                    line_indent + tab_size,
                    indent
                );
            }
        }

        // Feature: r-smart-indentation, Property 7: Brace Block Indentation
        // *For any* code block with an opening brace `{`, the computed indentation for lines
        // inside the block should equal the indentation of the line containing the opening
        // brace plus tab_size.
        // **Validates: Requirements 4.4**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_brace_block_indentation(
                line_indent in 0u32..50,
                opener_col in 0u32..100,
                tab_size in 1u32..9,
                style_idx in 0usize..2,
            ) {
                // Generate source with the specified line indentation
                let source = format!("{}if (TRUE) {{\n", " ".repeat(line_indent as usize));

                // Map index to style - brace indentation should be style-independent
                let style = match style_idx {
                    0 => IndentationStyle::RStudio,
                    _ => IndentationStyle::RStudioMinus,
                };

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style,
                };

                // Inside braces: indent from brace line + tab_size
                let context = IndentContext::InsideBraces {
                    opener_line: 0,
                    opener_col,
                };

                let indent = calculate_indentation(context, config, &source);

                // Property: indentation should equal line_indent + tab_size
                // regardless of style setting
                prop_assert_eq!(
                    indent,
                    line_indent + tab_size,
                    "For line_indent={}, tab_size={}, style={:?}, expected indent={}, got={}",
                    line_indent,
                    tab_size,
                    style,
                    line_indent + tab_size,
                    indent
                );
            }
        }

        // Feature: r-smart-indentation, Property 8: Closing Delimiter Alignment
        // *For any* closing delimiter (`)`, `]`, or `}`) that appears on its own line,
        // the computed indentation should equal the indentation of the line containing
        // the matching opening delimiter.
        // **Validates: Requirements 5.1**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_closing_delimiter_alignment(
                line_indent in 0u32..50,
                opener_col in 0u32..100,
                tab_size in 1u32..9,
                delimiter_idx in 0usize..3,
                style_idx in 0usize..2,
            ) {
                // Generate source with the specified line indentation
                let source = format!("{}func(\n", " ".repeat(line_indent as usize));

                // Map index to delimiter type
                let delimiter = match delimiter_idx {
                    0 => ')',
                    1 => ']',
                    _ => '}',
                };

                // Map index to style - closing delimiter alignment should be style-independent
                let style = match style_idx {
                    0 => IndentationStyle::RStudio,
                    _ => IndentationStyle::RStudioMinus,
                };

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style,
                };

                // Closing delimiter: align to opener line indentation
                let context = IndentContext::ClosingDelimiter {
                    opener_line: 0,
                    opener_col,
                    delimiter,
                };

                let indent = calculate_indentation(context, config, &source);

                // Property: indentation should equal the opener line's indentation
                // regardless of delimiter type, opener_col, tab_size, or style
                prop_assert_eq!(
                    indent,
                    line_indent,
                    "For line_indent={}, delimiter='{}', style={:?}, expected indent={}, got={}",
                    line_indent,
                    delimiter,
                    style,
                    line_indent,
                    indent
                );
            }
        }

        // Feature: r-smart-indentation, Property 9: Complete Expression De-indentation
        // *For any* complete expression (no trailing continuation operator, no unclosed
        // delimiters), the computed indentation for the following line should equal the
        // indentation of the enclosing block.
        // **Validates: Requirements 5.2**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_complete_expression_deindentation(
                enclosing_block_indent in 0u32..100,
                tab_size in 1u32..9,
                style_idx in 0usize..2,
            ) {
                // Map index to style - complete expression de-indentation should be style-independent
                let style = match style_idx {
                    0 => IndentationStyle::RStudio,
                    _ => IndentationStyle::RStudioMinus,
                };

                let config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style,
                };

                // After complete expression: return to enclosing block indentation
                let context = IndentContext::AfterCompleteExpression {
                    enclosing_block_indent,
                };

                let indent = calculate_indentation(context, config, "");

                // Property: indentation should equal the enclosing block indentation
                // regardless of tab_size or style setting
                prop_assert_eq!(
                    indent,
                    enclosing_block_indent,
                    "For enclosing_block_indent={}, style={:?}, expected indent={}, got={}",
                    enclosing_block_indent,
                    style,
                    enclosing_block_indent,
                    indent
                );
            }
        }

        // Feature: r-smart-indentation, Property 13: Style Configuration Behavior
        // *For any* function call, when raven.indentation.style is set to "rstudio",
        // same-line arguments should align to the opening paren (opener_col + 1) and
        // next-line arguments should indent from the function line; when set to
        // "rstudio-minus", all arguments should indent from the previous line.
        // **Validates: Requirements 7.2, 7.3**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn property_style_configuration_behavior(
                line_indent in 0u32..50,
                opener_col in 0u32..100,
                tab_size in 1u32..9,
                has_content in proptest::bool::ANY,
            ) {
                // Generate source with the specified line indentation
                let source = format!("{}func(\n", " ".repeat(line_indent as usize));

                // Test RStudio style behavior
                let rstudio_config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style: IndentationStyle::RStudio,
                };

                let context_rstudio = IndentContext::InsideParens {
                    opener_line: 0,
                    opener_col,
                    has_content_on_opener_line: has_content,
                };

                let rstudio_indent = calculate_indentation(context_rstudio, rstudio_config, &source);

                // RStudio style:
                // - Same-line content: align to opener_col + 1
                // - Next-line (no content): indent from function line (line_indent + tab_size)
                let expected_rstudio = if has_content {
                    opener_col + 1
                } else {
                    line_indent + tab_size
                };

                prop_assert_eq!(
                    rstudio_indent,
                    expected_rstudio,
                    "RStudio style: line_indent={}, opener_col={}, has_content={}, tab_size={}, expected={}, got={}",
                    line_indent,
                    opener_col,
                    has_content,
                    tab_size,
                    expected_rstudio,
                    rstudio_indent
                );

                // Test RStudio-minus style behavior
                let rstudio_minus_config = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style: IndentationStyle::RStudioMinus,
                };

                let context_rstudio_minus = IndentContext::InsideParens {
                    opener_line: 0,
                    opener_col,
                    has_content_on_opener_line: has_content,
                };

                let rstudio_minus_indent = calculate_indentation(context_rstudio_minus, rstudio_minus_config, &source);

                // RStudio-minus style:
                // - Always indent from opener line + tab_size, regardless of same-line content
                let expected_rstudio_minus = line_indent + tab_size;

                prop_assert_eq!(
                    rstudio_minus_indent,
                    expected_rstudio_minus,
                    "RStudio-minus style: line_indent={}, opener_col={}, has_content={}, tab_size={}, expected={}, got={}",
                    line_indent,
                    opener_col,
                    has_content,
                    tab_size,
                    expected_rstudio_minus,
                    rstudio_minus_indent
                );

                // Additional property: RStudio-minus should be independent of has_content
                // (already verified above, but let's make it explicit)
                // The indentation should be the same whether has_content is true or false
                let context_with_content = IndentContext::InsideParens {
                    opener_line: 0,
                    opener_col,
                    has_content_on_opener_line: true,
                };
                let context_without_content = IndentContext::InsideParens {
                    opener_line: 0,
                    opener_col,
                    has_content_on_opener_line: false,
                };

                let rstudio_minus_config_clone = IndentationConfig {
                    tab_size,
                    insert_spaces: true,
                    style: IndentationStyle::RStudioMinus,
                };

                let indent_with = calculate_indentation(context_with_content, rstudio_minus_config_clone.clone(), &source);
                let indent_without = calculate_indentation(context_without_content, rstudio_minus_config_clone, &source);

                prop_assert_eq!(
                    indent_with,
                    indent_without,
                    "RStudio-minus should produce same indent regardless of has_content: with={}, without={}",
                    indent_with,
                    indent_without
                );
            }
        }
    }
}
