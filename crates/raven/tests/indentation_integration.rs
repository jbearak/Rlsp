//! Integration tests for R smart indentation.
//!
//! These tests verify the full onTypeFormatting request/response cycle,
//! testing with real R code examples from tidyverse style guide.
//!
//! Run with: `cargo test -p raven --test indentation_integration`
//!
//! **Validates: Requirements 3.2, 4.1, 6.1, 7.2, 7.3**

use raven::indentation::{
    IndentationConfig, IndentationStyle, format_indentation, on_type_indentation,
};
use tower_lsp::lsp_types::Position;
use tree_sitter::Parser;

// ============================================================================
// Test Helpers
// ============================================================================

/// Create a tree-sitter parser configured for R.
fn make_r_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .expect("Failed to set R language for tree-sitter");
    parser
}

/// Parse R code and return the tree.
fn parse_r_code(code: &str) -> tree_sitter::Tree {
    let mut parser = make_r_parser();
    parser.parse(code, None).expect("Failed to parse R code")
}

/// Ask the public Tier 2 path for its optional indentation column.
fn on_type_result(code: &str, line: u32, config: &IndentationConfig) -> Option<u32> {
    let tree = parse_r_code(code);
    on_type_indentation(&tree, code, Position::new(line, 0), config)
}

/// Simulate the full onTypeFormatting flow: parse → judge → format.
/// Returns the generated indentation string.
fn simulate_on_type_formatting(code: &str, line: u32, config: IndentationConfig) -> String {
    let target_column =
        on_type_result(code, line, &config).expect("the judge should answer this fixture");
    let edit = format_indentation(line, target_column, config, code);
    edit.new_text
}

/// Get an answered indentation column from the full flow.
fn get_indentation_column(code: &str, line: u32, config: IndentationConfig) -> u32 {
    on_type_result(code, line, &config).expect("the judge should answer this fixture")
}

/// Create RStudio style config with given tab_size.
fn rstudio_config(tab_size: u32) -> IndentationConfig {
    IndentationConfig {
        tab_size,
        insert_spaces: true,
        enabled: true,
        argument_style: IndentationStyle::Aligned,
        infix_continuation_style: IndentationStyle::Aligned,
    }
}

/// Create RStudio-minus style config with given tab_size.
fn rstudio_minus_config(tab_size: u32) -> IndentationConfig {
    IndentationConfig {
        tab_size,
        insert_spaces: true,
        enabled: true,
        argument_style: IndentationStyle::Indented,
        infix_continuation_style: IndentationStyle::Indented,
    }
}

/// Create config with tabs instead of spaces.
fn tabs_config(tab_size: u32, style: IndentationStyle) -> IndentationConfig {
    IndentationConfig {
        tab_size,
        insert_spaces: false,
        enabled: true,
        argument_style: style,
        infix_continuation_style: style,
    }
}

// ============================================================================
// Test 1: Simple Pipe Chain Indentation
// Validates: Requirements 3.2 - Pipe chain continuation indentation
// ============================================================================

#[test]
fn test_simple_pipe_chain_native_pipe() {
    // Native R pipe |> after assignment: align to RHS ("data" at col 10)
    let code = "result <- data |>\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    // "result <- data" - "data" starts at column 10
    assert_eq!(column, 10, "Native pipe should align to RHS of assignment");
}

#[test]
fn test_simple_pipe_chain_magrittr_pipe() {
    // Magrittr pipe %>% after assignment: align to RHS ("data" at col 10)
    let code = "result <- data %>%\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(
        column, 10,
        "Magrittr pipe should align to RHS of assignment"
    );
}

#[test]
fn test_pipe_chain_multiple_lines() {
    // Multi-line pipe chain - all continuation lines should have same indentation
    // "result <- data %>%" — RHS "data" at col 10
    let code = "result <- data %>%\n          filter(x > 0) %>%\n";
    let column = get_indentation_column(code, 2, rstudio_config(2));
    assert_eq!(
        column, 10,
        "All pipe chain continuations should have uniform indentation"
    );
}

#[test]
fn test_pipe_chain_with_tab_size_4() {
    // "result <- data" - "data" at col 10; max(10, 0+4) = 10
    let code = "result <- data %>%\n";
    let column = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(column, 10, "Pipe chain should align to RHS of assignment");
}

#[test]
fn test_pipe_after_assignment_aligns_to_rhs() {
    // x <- merp() |> with tab_size=4: "merp" at col 5, max(5, 0+4) = 5
    let code = "x <- merp() |>\n";
    let column = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(column, 5, "Pipe after assignment should align to RHS");
}

#[test]
fn test_pipe_after_multiline_call_aligns_to_rhs() {
    // x <- merp(x,\n          y) |> with tab_size=4
    // The outermost pipe binary_operator starts at "merp" (col 5)
    let code = "x <- merp(x,\n          y) |>\n";
    let column = get_indentation_column(code, 2, rstudio_config(4));
    assert_eq!(
        column, 5,
        "Pipe after multiline call should align to RHS of assignment"
    );
}

#[test]
fn test_pipe_standalone_no_assignment() {
    // data |> with tab_size=2: chain starts at "data" col 0, max(0, 0+2) = 2
    let code = "data |>\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(column, 2, "Standalone pipe should indent by tab_size");
}

#[test]
fn test_pipe_indented_with_assignment() {
    // "  x <- data |>" with tab_size=2: "data" at col 7, line_indent=2, max(7, 2+2) = 7
    let code = "  x <- data |>\n";
    // Non-conforming reference bail: the offset top-level statement is not
    // rooted at a lint-accepted column.
    assert_eq!(on_type_result(code, 1, &rstudio_config(2)), None);
}

#[test]
fn test_ggplot_plus_operator_chain() {
    // ggplot2 style with + operator
    let code = r#"ggplot(data, aes(x, y)) +
"#;
    let indent = simulate_on_type_formatting(code, 1, rstudio_config(2));
    assert_eq!(indent, "  ", "ggplot + operator should indent continuation");
}

#[test]
fn test_formula_tilde_operator() {
    // Formula with ~ operator: `y` starts at col 9, align continuation there
    let code = "model <- y ~\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(
        column, 9,
        "Formula ~ operator after assignment should align to RHS"
    );
}

// ============================================================================
// Test 2: Function Call with Same-Line Arguments (RStudio Style)
// Validates: Requirements 4.1 - Same-line argument alignment
// ============================================================================

#[test]
fn test_function_call_same_line_args_rstudio() {
    // RStudio style: align to column after opening paren when content follows
    let code = "func(arg1,\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    // "func(" is 5 chars, so opener_col=4, alignment should be at column 5
    assert_eq!(
        column, 5,
        "RStudio style should align to column after opening paren"
    );
}

#[test]
fn test_function_call_same_line_args_longer_name() {
    // Longer function name
    let code = "longer_function_name(first_arg,\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    // "longer_function_name(" is 21 chars, opener_col=20, alignment at 21
    assert_eq!(
        column, 21,
        "Should align to column after opening paren for longer names"
    );
}

#[test]
fn test_nested_function_call_same_line() {
    // Nested function call with complete code
    // Use a complete expression to avoid parse errors
    let code = "outer(inner(x, y))\n";
    // After the complete expression, we're at base indent
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After complete nested call, should return to base indent"
    );
}

#[test]
fn test_nested_function_call_incomplete() {
    // Test incomplete nested function call
    // This tests repair-and-ask behavior on incomplete code.
    let code = "outer(inner(x,\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    // The judge should answer without panicking; keep a sanity bound.
    assert!(
        column <= 120,
        "Judge indentation should remain within a reasonable bound"
    );
}

// ============================================================================
// Test 3: Function Call with Next-Line Arguments (RStudio Style)
// Validates: Requirements 4.2 - Next-line argument indentation
// ============================================================================

#[test]
fn test_function_call_next_line_args_rstudio() {
    // RStudio style: indent from function line when paren followed by newline
    let code = "func(\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    // Line indent is 0, so should be 0 + tab_size = 2
    assert_eq!(
        column, 2,
        "RStudio style should indent from function line + tab_size"
    );
}

#[test]
fn test_function_call_next_line_args_indented() {
    // Function call on indented line
    let code = "  func(\n";
    // Non-conforming reference bail: the offset top-level call is not rooted
    // at a lint-accepted column.
    assert_eq!(on_type_result(code, 1, &rstudio_config(2)), None);
}

#[test]
fn test_function_call_next_line_tab_size_4() {
    let code = "func(\n";
    let column = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(column, 4, "Should respect tab_size=4 for next-line args");
}

// ============================================================================
// Test 4: Function Call with RStudio-Minus Style
// Validates: Requirements 4.3, 7.3 - RStudio-minus style indentation
// ============================================================================

#[test]
fn test_function_call_rstudio_minus_same_line() {
    // RStudio-minus: always indent from opener line, ignore paren position
    let code = "func(arg1,\n";
    let column = get_indentation_column(code, 1, rstudio_minus_config(2));
    // Line indent is 0, so should be 0 + tab_size = 2 (not aligned to paren)
    assert_eq!(
        column, 2,
        "RStudio-minus should indent from line, not align to paren"
    );
}

#[test]
fn test_function_call_rstudio_minus_next_line() {
    // RStudio-minus with next-line args
    let code = "func(\n";
    let column = get_indentation_column(code, 1, rstudio_minus_config(2));
    assert_eq!(
        column, 2,
        "RStudio-minus should indent from line + tab_size"
    );
}

#[test]
fn test_function_call_rstudio_minus_indented() {
    // RStudio-minus with indented function
    let code = "    func(arg1,\n";
    // Non-conforming reference bail: the offset top-level call is not rooted
    // at a lint-accepted column.
    assert_eq!(on_type_result(code, 1, &rstudio_minus_config(2)), None);
}

#[test]
fn test_rstudio_vs_rstudio_minus_difference() {
    // Same code, different styles should produce different results
    let code = "func(arg1,\n";

    let rstudio_col = get_indentation_column(code, 1, rstudio_config(2));
    let rstudio_minus_col = get_indentation_column(code, 1, rstudio_minus_config(2));

    // RStudio aligns to paren (col 5), RStudio-minus indents from line (col 2)
    assert_eq!(rstudio_col, 5, "RStudio should align to paren");
    assert_eq!(
        rstudio_minus_col, 2,
        "RStudio-minus should indent from line"
    );
    assert_ne!(
        rstudio_col, rstudio_minus_col,
        "Styles should produce different results"
    );
}

// ============================================================================
// Test 5: Brace Block Indentation
// Validates: Requirements 4.4 - Brace block indentation
// ============================================================================

#[test]
fn test_brace_block_basic() {
    let code = "if (TRUE) {\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(column, 2, "Brace block should indent by tab_size");
}

#[test]
fn test_brace_block_indented() {
    let code = "  if (TRUE) {\n";
    // Non-conforming reference bail: the offset top-level block is not rooted
    // at a lint-accepted column.
    assert_eq!(on_type_result(code, 1, &rstudio_config(2)), None);
}

#[test]
fn test_function_body_brace() {
    let code = "my_func <- function(x) {\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(column, 2, "Function body should indent by tab_size");
}

#[test]
fn test_nested_brace_blocks() {
    let code = r#"if (TRUE) {
  if (FALSE) {
"#;
    let column = get_indentation_column(code, 2, rstudio_config(2));
    assert_eq!(
        column, 4,
        "Nested brace should indent from inner brace line"
    );
}

#[test]
fn test_brace_block_tab_size_4() {
    let code = "if (TRUE) {\n";
    let column = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(column, 4, "Brace block should respect tab_size=4");
}

// ============================================================================
// Test 6: Closing Delimiter Alignment
// Validates: Requirements 5.1 - Closing delimiter alignment
// ============================================================================

#[test]
fn test_closing_paren_alignment() {
    // When cursor is at character 0 on a line with only ")", the auto-close
    // heuristic treats this as "inside parens" (the paren was pushed down by Enter).
    // This matches the real onTypeFormatting scenario.
    let code = r#"func(
  arg1,
  arg2
)"#;
    // Non-conforming reference bail: the legacy closer-line fixture is not a
    // lint-conforming post-Enter reference/probe pair.
    assert_eq!(on_type_result(code, 3, &rstudio_config(2)), None);
}

#[test]
fn test_closing_paren_indented_opener() {
    let code = r#"  func(
    arg1
  )"#;
    // Non-conforming reference bail: both the offset opener and legacy
    // closer-line fixture are outside the judge's accepted model.
    assert_eq!(on_type_result(code, 2, &rstudio_config(2)), None);
}

#[test]
fn test_closing_brace_alignment() {
    let code = r#"if (TRUE) {
  x <- 1
}"#;
    let column = get_indentation_column(code, 2, rstudio_config(2));
    // Inside braces → opener_indent + tab_size = 0 + 2
    assert_eq!(
        column, 2,
        "Auto-close heuristic: indent as inside-braces content"
    );
}

#[test]
fn test_closing_bracket_alignment() {
    let code = r#"x[
  1,
  2
]"#;
    // Non-conforming reference bail: the legacy closer-line fixture is not a
    // lint-conforming post-Enter reference/probe pair.
    assert_eq!(on_type_result(code, 3, &rstudio_config(2)), None);
}

// ============================================================================
// Test 7: Complete Expression De-indentation
// Validates: Requirements 5.2 - Complete expression de-indentation
// ============================================================================

#[test]
fn test_complete_expression_deindent() {
    // After a complete expression, return to enclosing block indent
    let code = r#"x <- 1
"#;
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After complete expression, should return to base indent"
    );
}

#[test]
fn test_complete_expression_in_block() {
    let code = r#"if (TRUE) {
  x <- 1
"#;
    // Line 2 is after a complete expression inside a block
    // The enclosing block indent should be maintained
    let column = get_indentation_column(code, 2, rstudio_config(2));
    // Inside the brace block, so should stay at block indent level
    assert_eq!(column, 2, "Inside block, should maintain block indent");
}

// ============================================================================
// Test 8: Configuration with Different tab_size Values
// Validates: Requirements 6.1 - FormattingOptions.tab_size respect
// ============================================================================

#[test]
fn test_tab_size_1() {
    let code = "func(\n";
    let column = get_indentation_column(code, 1, rstudio_config(1));
    assert_eq!(column, 1, "Should respect tab_size=1");
}

#[test]
fn test_tab_size_2() {
    let code = "func(\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(column, 2, "Should respect tab_size=2");
}

#[test]
fn test_tab_size_4() {
    let code = "func(\n";
    let column = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(column, 4, "Should respect tab_size=4");
}

#[test]
fn test_tab_size_8() {
    let code = "func(\n";
    let column = get_indentation_column(code, 1, rstudio_config(8));
    assert_eq!(column, 8, "Should respect tab_size=8");
}

#[test]
fn test_tab_size_affects_pipe_chain() {
    let code = "data %>%\n";

    let col_2 = get_indentation_column(code, 1, rstudio_config(2));
    let col_4 = get_indentation_column(code, 1, rstudio_config(4));

    assert_eq!(col_2, 2, "Pipe chain with tab_size=2");
    assert_eq!(col_4, 4, "Pipe chain with tab_size=4");
}

#[test]
fn test_tab_size_affects_brace_block() {
    let code = "{\n";

    let col_2 = get_indentation_column(code, 1, rstudio_config(2));
    let col_4 = get_indentation_column(code, 1, rstudio_config(4));

    assert_eq!(col_2, 2, "Brace block with tab_size=2");
    assert_eq!(col_4, 4, "Brace block with tab_size=4");
}

// ============================================================================
// Test 9: Configuration with insert_spaces true/false
// Validates: Requirements 6.3, 6.4 - insert_spaces respect
// ============================================================================

#[test]
fn test_insert_spaces_true() {
    let code = "func(\n";
    let indent = simulate_on_type_formatting(code, 1, rstudio_config(4));
    assert_eq!(indent, "    ", "insert_spaces=true should produce spaces");
    assert!(
        indent.chars().all(|c| c == ' '),
        "Should contain only spaces"
    );
}

#[test]
fn test_insert_spaces_false() {
    let code = "func(\n";
    let config = tabs_config(4, IndentationStyle::Aligned);
    // Tabs-mode bail: the judge works in character columns, not tab stops.
    assert_eq!(on_type_result(code, 1, &config), None);
}

#[test]
fn test_insert_spaces_false_with_alignment() {
    // func( has opener at column 4; next-line indent = 4 columns = 1 tab
    let code = "func(\n";
    let config = tabs_config(4, IndentationStyle::Aligned);
    // Tabs-mode bail: the judge works in character columns, not tab stops.
    assert_eq!(on_type_result(code, 1, &config), None);
}

#[test]
fn test_insert_spaces_false_alignment_with_spaces() {
    // Test case where we need tabs + trailing spaces
    // func(arg1, -> opener at col 4, alignment at col 5
    let code = "func(arg1,\n";
    let config = tabs_config(4, IndentationStyle::Aligned);
    // Tabs-mode bail: the judge works in character columns, not tab stops.
    assert_eq!(on_type_result(code, 1, &config), None);
}

#[test]
fn test_insert_spaces_false_multiple_tabs() {
    // Test case needing multiple tabs
    let code = "        func(\n"; // 8 spaces indent
    let config = tabs_config(4, IndentationStyle::Aligned);
    // Tabs-mode bail takes precedence even when the legacy path could express
    // the column as a whole number of tabs.
    assert_eq!(on_type_result(code, 1, &config), None);
}

// ============================================================================
// Real-World Tidyverse Examples
// Validates: Requirements 3.2, 4.1, 7.2, 7.3
// ============================================================================

#[test]
fn test_tidyverse_dplyr_pipe_chain() {
    // Real dplyr example
    // "result <- mtcars" — "mtcars" at col 10
    let code = r#"result <- mtcars %>%
          filter(mpg > 20) %>%
          select(mpg, cyl, hp) %>%
          mutate(efficiency = mpg / hp) %>%
"#;
    // All continuation lines should align to RHS (col 10)
    let col_2 = get_indentation_column(code, 2, rstudio_config(2));
    let col_3 = get_indentation_column(code, 3, rstudio_config(2));
    let col_4 = get_indentation_column(code, 4, rstudio_config(2));

    assert_eq!(col_2, 10, "Line 2 should align to RHS of assignment");
    assert_eq!(col_3, 10, "Line 3 should align to RHS of assignment");
    assert_eq!(col_4, 10, "Line 4 should align to RHS of assignment");
}

#[test]
fn test_tidyverse_ggplot_layers() {
    // Real ggplot2 example
    let code = r#"ggplot(data, aes(x = mpg, y = hp)) +
  geom_point() +
  geom_smooth(method = "lm") +
  theme_minimal() +
"#;
    let col_2 = get_indentation_column(code, 2, rstudio_config(2));
    let col_3 = get_indentation_column(code, 3, rstudio_config(2));
    let col_4 = get_indentation_column(code, 4, rstudio_config(2));

    assert_eq!(col_2, 2, "ggplot layer should have uniform indent");
    assert_eq!(col_3, 2, "ggplot layer should have uniform indent");
    assert_eq!(col_4, 2, "ggplot layer should have uniform indent");
}

#[test]
fn test_tidyverse_function_with_named_args() {
    // Function call with named arguments (RStudio style)
    let code = r#"mutate(data,
       new_col = old_col * 2,
"#;
    // "mutate(" is 7 chars, opener at col 6, alignment at col 7
    let column = get_indentation_column(code, 2, rstudio_config(2));
    assert_eq!(column, 7, "Named args should align to opening paren");
}

#[test]
fn test_tidyverse_nested_function_calls() {
    // Nested function calls common in tidyverse
    let code = r#"summarise(group_by(data, category),
"#;
    // Innermost unclosed paren is from summarise at col 9
    // "summarise(" is 10 chars, opener at col 9, alignment at col 10
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(column, 10, "Should align to innermost function paren");
}

#[test]
fn test_tidyverse_across_function() {
    // across() function common in dplyr - use complete code
    let code = "mutate(data, across(where(is.numeric), mean))\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    // After complete expression, return to base indent
    assert_eq!(
        column, 0,
        "After complete across call, should return to base indent"
    );
}

#[test]
fn test_tidyverse_across_function_incomplete() {
    // Test repair-and-ask behavior for an incomplete across().
    let code = "mutate(across(where(is.numeric),\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    // The judge should answer without panicking; keep a sanity bound.
    assert!(
        column <= 120,
        "Judge indentation should remain within a reasonable bound"
    );
}

// ============================================================================
// Edge Cases and Error Handling
// ============================================================================

#[test]
fn test_multiline_string_interior_emits_no_edit() {
    // Multiline-string interior bail.
    assert_eq!(
        on_type_result("x <- \"open\nstill open\n", 2, &rstudio_config(2),),
        None
    );
}

#[test]
fn test_multiline_backtick_interior_emits_no_edit() {
    // Multiline-string-like interior bail.
    assert_eq!(
        on_type_result("f(\u{60}a\nb\u{60})\n", 1, &rstudio_config(2)),
        None
    );
}

#[test]
fn test_error_window_emits_no_edit() {
    // Error-window bail: the repaired reference-to-probe window still fails
    // to parse.
    assert_eq!(on_type_result("x %+\n", 1, &rstudio_config(2)), None);
    assert_eq!(on_type_result("f(\n] + x", 1, &rstudio_config(2)), None);
}

#[test]
fn test_tab_shaped_active_context_emits_no_edit() {
    // Tabs bail: a real tab in the active context cannot be modeled in
    // character columns.
    assert_eq!(on_type_result("\tf(a,\n", 1, &rstudio_config(2)), None);
}

#[test]
fn test_non_conforming_references_emit_no_edit() {
    // Non-conforming reference bail.
    for source in ["    {\n", "    result <-\n"] {
        assert_eq!(on_type_result(source, 1, &rstudio_config(2)), None);
    }
    assert_eq!(
        on_type_result("f <- function() {\n      x <- 1\n", 2, &rstudio_config(2),),
        None
    );
}

#[test]
fn test_unrelated_errors_and_tabs_do_not_force_a_bail() {
    assert_eq!(
        on_type_result("x +*\n\ny <- 1\nf(\n", 4, &rstudio_config(2)),
        Some(2)
    );
    assert_eq!(
        on_type_result("x <-\t1\nf(\n", 2, &rstudio_config(2)),
        Some(2)
    );
}

#[test]
fn test_empty_file() {
    let code = "";
    // Other bail class: an empty buffer has no probe line to repair.
    assert_eq!(on_type_result(code, 0, &rstudio_config(2)), None);
}

#[test]
fn test_single_line_no_special_context() {
    let code = "x <- 1\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After simple assignment, should return to base indent"
    );
}

#[test]
fn test_comment_after_pipe() {
    // Pipe with trailing comment
    let code = "data %>% # comment\n";
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(column, 2, "Pipe with comment should still indent");
}

#[test]
fn test_whitespace_only_line() {
    let code = "func(\n    \n";
    // Line 2 is whitespace only, but we're inside parens
    let column = get_indentation_column(code, 2, rstudio_config(2));
    // Should still detect InsideParens context
    assert_eq!(
        column, 2,
        "Whitespace line inside parens should maintain indent"
    );
}

// ============================================================================
// TextEdit Range Verification
// Validates: Requirements 6.5 - TextEdit range replacement
// ============================================================================

#[test]
fn test_textedit_replaces_existing_whitespace() {
    let code = "    existing_indent\n";
    let edit = format_indentation(0, 0, rstudio_config(2), code);

    // Range should span from column 0 to 4 (length of existing whitespace)
    assert_eq!(
        edit.range.start.character, 0,
        "Range should start at column 0"
    );
    assert_eq!(
        edit.range.end.character, 4,
        "Range should end at existing whitespace length"
    );
}

#[test]
fn test_textedit_no_existing_whitespace() {
    let code = "no_indent\n";
    let edit = format_indentation(0, 4, rstudio_config(2), code);

    // Range should be empty (0 to 0)
    assert_eq!(edit.range.start.character, 0);
    assert_eq!(edit.range.end.character, 0);
    assert_eq!(edit.new_text, "    ");
}

#[test]
fn test_textedit_multiline_correct_line() {
    let code = "line0\n  line1\n    line2\n";
    let edit = format_indentation(1, 4, rstudio_config(2), code);

    assert_eq!(edit.range.start.line, 1, "Should target line 1");
    assert_eq!(edit.range.end.line, 1, "Should target line 1");
    assert_eq!(edit.range.start.character, 0);
    assert_eq!(edit.range.end.character, 2, "Line 1 has 2 spaces");
    assert_eq!(edit.new_text, "    ");
}

// ============================================================================
// Configuration Default Behavior
// Validates: Requirements 7.4 - Default to RStudio style
// ============================================================================

#[test]
fn test_default_config_is_rstudio() {
    let config = IndentationConfig::default();
    assert_eq!(
        config.argument_style,
        IndentationStyle::Aligned,
        "Default should be RStudio style"
    );
    assert_eq!(config.tab_size, 2, "Default tab_size should be 2");
    assert!(config.insert_spaces, "Default should use spaces");
}

#[test]
fn test_default_style_same_line_alignment() {
    // Verify default config produces RStudio-style alignment
    let code = "func(arg1,\n";
    let column = get_indentation_column(code, 1, IndentationConfig::default());
    // RStudio style aligns to paren: "func(" = 5 chars, opener at 4, align at 5
    assert_eq!(
        column, 5,
        "Default config should use RStudio-style alignment"
    );
}

// ============================================================================
// Full Request/Response Cycle Simulation
// ============================================================================

#[test]
fn test_full_cycle_pipe_chain() {
    // Simulate complete onTypeFormatting flow
    // Use complete code that parses without errors
    let code = "result <- data %>%\n  filter(x > 0)";
    let config = rstudio_config(2);

    // Ask the judge for indentation at line 1 (after the pipe).
    // "result <- data" — "data" at col 10, max(10, 0+2) = 10
    let target_column =
        on_type_result(code, 1, &config).expect("the judge should answer this fixture");
    assert_eq!(target_column, 10, "Should align pipe to RHS of assignment");

    // Step 4: Format TextEdit
    let edit = format_indentation(1, target_column, config, code);
    assert_eq!(
        edit.new_text, "          ",
        "Should generate correct whitespace"
    );
    assert_eq!(edit.range.start.line, 1);
    assert_eq!(edit.range.end.line, 1);
}

#[test]
fn test_full_cycle_function_args_rstudio() {
    let code = "mutate(data,\n";
    let config = rstudio_config(2);

    let target_column =
        on_type_result(code, 1, &config).expect("the judge should answer this fixture");
    // "mutate(" is 7 chars, opener at 6, align at 7
    assert_eq!(target_column, 7);

    let edit = format_indentation(1, target_column, config, code);
    assert_eq!(edit.new_text, "       ", "Should generate 7 spaces");
}

#[test]
fn test_full_cycle_function_args_rstudio_minus() {
    let code = "mutate(data,\n";
    let config = rstudio_minus_config(2);

    let target_column =
        on_type_result(code, 1, &config).expect("the judge should answer this fixture");
    // RStudio-minus: line indent (0) + tab_size (2) = 2
    assert_eq!(target_column, 2);

    let edit = format_indentation(1, target_column, config, code);
    assert_eq!(edit.new_text, "  ", "Should generate 2 spaces");
}

#[test]
fn test_full_cycle_closing_delimiter() {
    let code = r#"func(
  arg1
)"#;
    let config = rstudio_config(2);

    // Non-conforming reference bail: this legacy closer-line fixture is not
    // the post-Enter blank/closer state the judge can validate.
    assert_eq!(on_type_result(code, 2, &config), None);
}

#[test]
fn test_full_cycle_brace_block() {
    let code = "if (TRUE) {\n";
    let config = rstudio_config(4);

    let target_column =
        on_type_result(code, 1, &config).expect("the judge should answer this fixture");
    assert_eq!(target_column, 4, "Brace block should indent by tab_size");

    let edit = format_indentation(1, target_column, config, code);
    assert_eq!(edit.new_text, "    ", "Should generate 4 spaces");
}

// ============================================================================
// Style Comparison Tests
// Validates: Requirements 7.2, 7.3 - Style configuration behavior
// ============================================================================

#[test]
fn test_style_comparison_same_line_args() {
    let code = "function_call(first_arg,\n";

    // RStudio: align to paren
    let rstudio_col = get_indentation_column(code, 1, rstudio_config(2));
    // "function_call(" is 14 chars, opener at 13, align at 14
    assert_eq!(rstudio_col, 14, "RStudio should align to paren");

    // RStudio-minus: indent from line
    let minus_col = get_indentation_column(code, 1, rstudio_minus_config(2));
    assert_eq!(minus_col, 2, "RStudio-minus should indent from line");
}

#[test]
fn test_style_comparison_next_line_args() {
    let code = "function_call(\n";

    // Both styles should behave the same for next-line args
    let rstudio_col = get_indentation_column(code, 1, rstudio_config(2));
    let minus_col = get_indentation_column(code, 1, rstudio_minus_config(2));

    assert_eq!(rstudio_col, 2, "RStudio next-line should indent from line");
    assert_eq!(
        minus_col, 2,
        "RStudio-minus next-line should indent from line"
    );
    assert_eq!(
        rstudio_col, minus_col,
        "Both styles same for next-line args"
    );
}

#[test]
fn test_style_does_not_affect_pipe_chains() {
    let code = "data %>%\n";

    let rstudio_col = get_indentation_column(code, 1, rstudio_config(2));
    let minus_col = get_indentation_column(code, 1, rstudio_minus_config(2));

    assert_eq!(
        rstudio_col, minus_col,
        "Style should not affect pipe chain indentation"
    );
}

#[test]
fn test_style_does_not_affect_brace_blocks() {
    let code = "{\n";

    let rstudio_col = get_indentation_column(code, 1, rstudio_config(2));
    let minus_col = get_indentation_column(code, 1, rstudio_minus_config(2));

    assert_eq!(
        rstudio_col, minus_col,
        "Style should not affect brace block indentation"
    );
}

#[test]
fn test_style_does_not_affect_closing_delimiters() {
    let code = r#"func(
  arg
)"#;

    // Non-conforming reference bail: neither style emits an edit for this
    // legacy closer-line fixture.
    assert_eq!(on_type_result(code, 2, &rstudio_config(2)), None);
    assert_eq!(on_type_result(code, 2, &rstudio_minus_config(2)), None);
}

// ============================================================================
// Complex Real-World R Code Scenarios
// Validates: Requirements 3.3, 3.4, 10.1, 10.2, 10.3
// ============================================================================

// ----------------------------------------------------------------------------
// Complex Pipe Chains with Nested Function Calls
// Validates: Requirements 3.3 (uniform continuation), 10.2 (function in pipe)
// ----------------------------------------------------------------------------

#[test]
fn test_pipe_chain_with_nested_function_calls() {
    // Complex dplyr chain with nested functions inside pipe steps
    // "result <- data" — "data" at col 10
    let code = r#"result <- data %>%
          filter(category %in% c("A", "B", "C")) %>%
          mutate(score = ifelse(value > mean(value), "high", "low")) %>%
          group_by(category, score) %>%
"#;
    // All continuation lines should align to RHS (col 10)
    let col_2 = get_indentation_column(code, 2, rstudio_config(2));
    let col_3 = get_indentation_column(code, 3, rstudio_config(2));
    let col_4 = get_indentation_column(code, 4, rstudio_config(2));

    assert_eq!(col_2, 10, "Nested function in pipe should align to RHS");
    assert_eq!(col_3, 10, "Nested ifelse in pipe should align to RHS");
    assert_eq!(col_4, 10, "Multiple args in pipe should align to RHS");
}

#[test]
fn test_pipe_chain_with_deeply_nested_functions() {
    // Pipe with deeply nested function calls (3+ levels)
    // "result <- data" — "data" at col 10
    let code = r#"result <- data %>%
          mutate(x = outer(middle(inner(value)))) %>%
"#;
    let column = get_indentation_column(code, 2, rstudio_config(2));
    assert_eq!(
        column, 10,
        "Deeply nested functions in pipe should align to RHS"
    );
}

#[test]
fn test_pipe_chain_with_anonymous_function() {
    // Pipe with anonymous function (common in purrr)
    let code = r#"result <- data %>%
  map(function(x) {
    x * 2
  }) %>%
  filter(y > 0)
"#;
    // Line 5 is after the complete pipe chain
    let column = get_indentation_column(code, 5, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After complete pipe chain should return to base indent"
    );
}

// ----------------------------------------------------------------------------
// Pipe Inside Function Argument
// Validates: Requirements 3.4 (nested pipe context), 10.1 (pipe in function)
// ----------------------------------------------------------------------------

#[test]
fn test_pipe_inside_function_argument() {
    // Pipe chain as a function argument - common in tidyverse
    let code = r#"result <- mutate(data, new_col = x %>%
"#;
    // Inside the pipe chain that's inside the function argument
    let column = get_indentation_column(code, 1, rstudio_config(2));
    // The pipe chain starts at "x %>%", so continuation should be relative to that
    // This tests that pipe context takes priority over function argument context
    assert!(
        column >= 2,
        "Pipe inside function arg should indent for continuation"
    );
}

#[test]
fn test_pipe_inside_function_argument_complete() {
    // Complete pipe chain inside function argument
    // "  new_col = x %>%" on line 2 — "x" at col 12 (RHS of `=`)
    let code = r#"result <- mutate(
  data,
  new_col = x %>%
            filter(y > 0) %>%
            summarise(mean(z))
)
"#;
    // Line 4 is a pipe continuation inside the function argument
    let col_4 = get_indentation_column(code, 4, rstudio_config(2));
    // Should align to RHS of assignment (col 12)
    assert_eq!(
        col_4, 12,
        "Pipe continuation inside function arg should align to RHS"
    );
}

#[test]
fn test_nested_pipe_in_across() {
    // Pipe inside across() - common pattern in dplyr
    let code = r#"result <- data %>%
  mutate(across(where(is.numeric), ~ .x %>%
"#;
    let column = get_indentation_column(code, 2, rstudio_config(2));
    // Inside a nested pipe context
    assert!(
        column >= 2,
        "Pipe inside across should indent for continuation"
    );
}

// ----------------------------------------------------------------------------
// Function Inside Pipe
// Validates: Requirements 10.2 (function in pipe)
// ----------------------------------------------------------------------------

#[test]
fn test_function_call_inside_pipe_same_line_args() {
    // Function with same-line args inside pipe chain - complete expression
    let code = r#"result <- data %>%
  mutate(col = func(arg1, arg2))
"#;
    // Line 2 is after the complete expression
    let column = get_indentation_column(code, 2, rstudio_config(2));
    // After complete expression, return to base indent
    assert_eq!(
        column, 0,
        "After complete function in pipe should return to base indent"
    );
}

#[test]
fn test_function_call_inside_pipe_next_line_args() {
    // Function with next-line args inside pipe chain - complete expression
    let code = r#"result <- data %>%
  mutate(col = func(
    arg1,
    arg2
  ))
"#;
    // Line 5 is after the complete expression
    let column = get_indentation_column(code, 5, rstudio_config(2));
    // After complete expression, return to base indent
    assert_eq!(
        column, 0,
        "After complete function in pipe should return to base indent"
    );
}

#[test]
fn test_function_call_inside_pipe_rstudio_minus() {
    // Function inside pipe with RStudio-minus style - complete expression
    let code = r#"result <- data %>%
  mutate(col = func(arg1, arg2))
"#;
    let column = get_indentation_column(code, 2, rstudio_minus_config(2));
    // After complete expression, return to base indent
    assert_eq!(
        column, 0,
        "After complete function in pipe should return to base indent (RStudio-minus)"
    );
}

// ----------------------------------------------------------------------------
// Mixed Operators (Pipe + Plus in Same Chain)
// Validates: Requirements 3.3 (uniform continuation)
// ----------------------------------------------------------------------------

#[test]
fn test_mixed_pipe_and_plus_operators() {
    // ggplot2 pattern: pipe into ggplot, then + for layers
    let code = r#"result <- data %>%
  ggplot(aes(x, y)) +
  geom_point() +
"#;
    // The judge aligns this assignment-owned chain to the RHS at column 10.
    let col_2 = get_indentation_column(code, 2, rstudio_config(2));
    let col_3 = get_indentation_column(code, 3, rstudio_config(2));

    assert_eq!(col_2, 10, "ggplot after pipe should align to the RHS");
    assert_eq!(col_3, 10, "geom_point after + should align to the RHS");
}

#[test]
fn test_ggplot_full_chain_with_pipe() {
    // Full ggplot chain starting with pipe
    let code = r#"mtcars %>%
  filter(mpg > 20) %>%
  ggplot(aes(x = wt, y = mpg)) +
  geom_point(aes(color = factor(cyl))) +
  geom_smooth(method = "lm") +
  theme_minimal() +
  labs(title = "MPG vs Weight") +
"#;
    // All lines should have uniform indentation
    for line in 2..7 {
        let column = get_indentation_column(code, line, rstudio_config(2));
        assert_eq!(column, 2, "Line {} should have uniform chain indent", line);
    }
}

#[test]
fn test_formula_with_pipe() {
    // Formula operator mixed with pipe
    let code = r#"model <- data %>%
  lm(y ~ x1 + x2 +
"#;
    // Inside the formula, the + is a continuation
    let column = get_indentation_column(code, 2, rstudio_config(2));
    // This is inside the lm() call's arguments
    assert!(
        column >= 2,
        "Formula continuation inside pipe should indent"
    );
}

// ----------------------------------------------------------------------------
// Deeply Nested Structures (3+ Levels)
// Validates: Requirements 10.3 (multiple nesting levels)
// ----------------------------------------------------------------------------

#[test]
fn test_three_level_nesting_pipe_func_pipe() {
    // Pipe -> Function -> Pipe (3 levels)
    let code = r#"result <- data %>%
  mutate(
    new_col = other_data %>%
      filter(x > 0) %>%
"#;
    // Line 4 is inside a pipe chain that's inside a function that's inside a pipe
    let column = get_indentation_column(code, 4, rstudio_config(2));
    // The expectation engine preserves the hanging argument/assignment
    // contributions around the inner chain.
    assert_eq!(
        column, 14,
        "Inner pipe chain should keep the lint-accepted hanging indent"
    );
}

#[test]
fn test_three_level_nesting_func_pipe_func() {
    // Function -> Pipe -> Function (3 levels)
    let code = r#"outer_func(
  data %>%
    inner_func(arg1,
"#;
    // Line 3 is inside inner_func's arguments
    let column = get_indentation_column(code, 3, rstudio_config(2));
    // RStudio style: align to inner_func's paren
    // "    inner_func(" - paren at column 14
    assert_eq!(column, 15, "Innermost function should align to its paren");
}

#[test]
fn test_deeply_nested_braces_and_parens() {
    // Multiple levels of braces and parens
    let code = r#"if (condition) {
  result <- function(x) {
    inner_call(
      arg1,
"#;
    // Line 4 is inside inner_call's arguments
    let column = get_indentation_column(code, 4, rstudio_config(2));
    // "    inner_call(" - paren at column 14
    assert_eq!(
        column, 6,
        "Deeply nested function args should indent correctly"
    );
}

#[test]
fn test_nested_anonymous_functions() {
    // Nested anonymous functions (common in functional R)
    let code = r#"result <- lapply(data, function(x) {
  sapply(x, function(y) {
    y * 2
  })
})
"#;
    // Line 2 is inside the inner function body (function(y) { ... })
    // The inner { is on line 1 which has indent 2, so body indent = 2 + tab_size = 4
    let col_2 = get_indentation_column(code, 2, rstudio_config(2));
    assert_eq!(
        col_2, 4,
        "Inner function body should indent from inner brace line"
    );
}

// ----------------------------------------------------------------------------
// Real-World Tidyverse Patterns
// Validates: Requirements 3.3, 3.4, 10.1, 10.2, 10.3
// ----------------------------------------------------------------------------

#[test]
fn test_tidyverse_data_wrangling_pipeline() {
    // Realistic data wrangling pipeline
    // "clean_data <- raw_data" — "raw_data" at col 14
    let code = r#"clean_data <- raw_data %>%
              janitor::clean_names() %>%
              filter(!is.na(important_column)) %>%
              mutate(
                date = lubridate::ymd(date_string),
                category = factor(category, levels = c("A", "B", "C"))
              ) %>%
              select(id, date, category, value)
"#;
    // Check various continuation lines — align to RHS col 14
    let col_2 = get_indentation_column(code, 2, rstudio_config(2));
    let col_3 = get_indentation_column(code, 3, rstudio_config(2));
    // Line 8 is after the complete expression
    let col_8 = get_indentation_column(code, 8, rstudio_config(2));

    assert_eq!(col_2, 14, "janitor call should align to RHS");
    assert_eq!(col_3, 14, "filter call should align to RHS");
    assert_eq!(
        col_8, 0,
        "After complete pipeline should return to base indent"
    );
}

#[test]
fn test_tidyverse_summarise_with_across() {
    // Common summarise + across pattern
    let code = r#"summary <- data %>%
  group_by(category) %>%
  summarise(
    across(where(is.numeric), list(
      mean = ~mean(.x, na.rm = TRUE),
      sd = ~sd(.x, na.rm = TRUE)
    )),
    n = n()
  )
"#;
    // Line 9 is after the complete expression
    let column = get_indentation_column(code, 9, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After complete summarise should return to base indent"
    );
}

#[test]
fn test_tidyverse_pivot_operations() {
    // pivot_longer/pivot_wider patterns
    let code = r#"tidy_data <- wide_data %>%
  pivot_longer(
    cols = starts_with("value_"),
    names_to = "variable",
    values_to = "measurement"
  ) %>%
  separate(variable, into = c("type", "year"), sep = "_")
"#;
    // Line 7 is after the complete expression
    let col_7 = get_indentation_column(code, 7, rstudio_config(2));

    assert_eq!(
        col_7, 0,
        "After complete pipeline should return to base indent"
    );
}

#[test]
fn test_tidyverse_join_operations() {
    // Multiple join operations
    let code = r#"combined <- data1 %>%
  left_join(data2, by = "id") %>%
  inner_join(
    data3 %>%
      filter(active == TRUE) %>%
      select(id, extra_col),
    by = "id"
  )
"#;
    // Line 8 is after the complete expression
    let column = get_indentation_column(code, 8, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After complete join pipeline should return to base indent"
    );
}

#[test]
fn test_tidyverse_nested_mutate_case_when() {
    // Nested case_when inside mutate
    let code = r#"result <- data %>%
  mutate(
    status = case_when(
      score >= 90 ~ "A",
      score >= 80 ~ "B",
      score >= 70 ~ "C",
      TRUE ~ "F"
    )
  )
"#;
    // Line 9 is after the complete expression
    let column = get_indentation_column(code, 9, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After complete nested case_when should return to base indent"
    );
}

#[test]
fn test_purrr_map_with_pipe() {
    // purrr map functions with pipes
    let code = r#"results <- data %>%
  split(.$group) %>%
  map(~ .x %>%
    filter(value > 0) %>%
    summarise(mean = mean(value))
  )
"#;
    // Line 6 is after the complete expression
    let column = get_indentation_column(code, 6, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After complete map with nested pipe should return to base indent"
    );
}

#[test]
fn test_ggplot_with_facets_and_themes() {
    // Complex ggplot with facets and theme customization
    let code = r#"plot <- data %>%
  ggplot(aes(x = x_var, y = y_var, color = group)) +
  geom_point(alpha = 0.7) +
  geom_smooth(method = "lm", se = FALSE) +
  facet_wrap(~ category, scales = "free") +
  scale_color_brewer(palette = "Set1") +
  theme_minimal() +
  theme(
    legend.position = "bottom",
    axis.text.x = element_text(angle = 45, hjust = 1)
  )
"#;
    // Lines 2-7 align to the assignment RHS, `data`, at column 8.
    for line in 2..8 {
        let column = get_indentation_column(code, line, rstudio_config(2));
        assert_eq!(
            column, 8,
            "ggplot line {} should have uniform chain indent",
            line
        );
    }
    // Line 11 is after the complete expression
    let col_11 = get_indentation_column(code, 11, rstudio_config(2));
    assert_eq!(
        col_11, 0,
        "After complete ggplot should return to base indent"
    );
}

#[test]
fn test_shiny_reactive_chain() {
    // Shiny reactive expression pattern
    let code = r#"filtered_data <- reactive({
  req(input$dataset)
  data() %>%
    filter(category == input$category) %>%
    arrange(desc(value)) %>%
"#;
    // Lines inside the reactive block with pipe chain
    // Non-conforming reference bail: the fixture's line 3 is already at a
    // column outside the expectation set for this incomplete chain.
    let col_4 = on_type_result(code, 4, &rstudio_config(2));
    let col_5 = get_indentation_column(code, 5, rstudio_config(2));

    assert_eq!(col_4, None);
    assert_eq!(
        col_5, 4,
        "Continuation inside reactive should maintain indent"
    );
}

// ----------------------------------------------------------------------------
// Edge Cases for Complex Nesting
// ----------------------------------------------------------------------------

#[test]
fn test_pipe_after_closing_brace() {
    // Pipe continuation after a closing brace
    let code = r#"result <- data %>%
  {
    . %>%
      filter(x > 0)
  } %>%
  select(y)
"#;
    // Line 6 is after the complete expression
    let column = get_indentation_column(code, 6, rstudio_config(2));
    assert_eq!(
        column, 0,
        "After complete pipe chain should return to base indent"
    );
}

#[test]
fn test_multiple_pipes_same_line() {
    // Multiple pipe operations on same line (less common but valid)
    // "result <- data %>% filter(x > 0) %>%" — RHS "data" at col 10
    let code = r#"result <- data %>% filter(x > 0) %>%
"#;
    let column = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(
        column, 10,
        "Continuation after multiple pipes should align to RHS"
    );
}

#[test]
fn test_pipe_with_comment_between_lines() {
    // Pipe chain with comments interspersed
    let code = r#"result <- data %>%
  # Filter out negative values
  filter(x > 0) %>%
  # Calculate summary statistics
  summarise(mean = mean(x))
"#;
    // Line 3 is the filter call (after comment) - but it's a complete line with pipe
    // Line 5 is the summarise call (after comment) - complete expression
    let col_5 = get_indentation_column(code, 5, rstudio_config(2));

    // After complete expression should return to base indent
    assert_eq!(
        col_5, 0,
        "After complete expression should return to base indent"
    );
}

#[test]
fn test_native_and_magrittr_pipe_mixed() {
    // Mixing native |> and magrittr %>% pipes (unusual but valid)
    // "result <- data |>" — RHS "data" at col 10
    let code = r#"result <- data |>
          filter(x > 0) %>%
          select(y) |>
"#;
    let col_2 = get_indentation_column(code, 2, rstudio_config(2));
    let col_3 = get_indentation_column(code, 3, rstudio_config(2));

    assert_eq!(col_2, 10, "Magrittr pipe after native should align to RHS");
    assert_eq!(col_3, 10, "Native pipe after magrittr should align to RHS");
}

#[test]
fn test_nested_parens_in_pipe_chain() {
    // When typing inside nested parens within a pipe chain,
    // align with the first argument of the inner function call
    let code = "x <- f() |>\n     z(x = f(1,\n";
    let column = get_indentation_column(code, 2, rstudio_config(4));
    // f( is at col 12 on line 1, with content "1," after it
    // RStudio style: align to col 13 (opener_col + 1)
    assert_eq!(column, 13);
}

#[test]
fn test_nested_parens_in_pipe_chain_with_autoclose() {
    // VS Code auto-closing pairs push )) to the next line when Enter is pressed
    // The document looks like:
    //   x <- f() |>
    //        z(x = f(1,
    //   ))
    // We want line 2 to get col 13 (align with "1"), not col 0
    let code = "x <- f() |>\n     z(x = f(1,\n))\n";
    let column = get_indentation_column(code, 2, rstudio_config(4));
    assert_eq!(
        column, 13,
        "Should align inside f() to where '1' is, even with auto-closed parens"
    );
}

#[test]
fn test_nested_call_autoclose_no_pipe() {
    // Non-pipe: x <- f(x = f(1,)) with )) pushed down by Enter
    // Currently returns 13, but should this be 0 or something else?
    let code = "x <- f(x = f(1,\n))\n";
    let col = get_indentation_column(code, 1, rstudio_config(2));
    assert_eq!(col, 9, "Should use the lint-accepted nested-call column");
}

#[test]
fn test_nested_call_simple() {
    // x <- f(a(a,
    // |))
    // Should indent to align with the first arg of inner a(
    let code = "x <- f(a(a,\n";
    let col = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(col, 11, "Should use the lint-accepted nested-call column");
}

#[test]
fn test_nested_call_with_named_arg() {
    // x <- f(a, x = f(a,
    // |))
    // Should indent to align with the first arg of inner f(
    let code = "x <- f(a, x = f(a,\n";
    let col = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(col, 11, "Should use the lint-accepted nested-call column");
}

#[test]
fn test_nested_call_simple_with_autoclose() {
    // x <- f(a(a,
    // ))
    // When )) is present, should still indent as if inside the inner call
    let code = "x <- f(a(a,\n))\n";
    let col = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(
        col, 11,
        "Should use the lint-accepted nested-call column with auto-close"
    );
}

#[test]
fn test_nested_call_with_named_arg_and_autoclose() {
    // x <- f(a, x = f(a,
    // ))
    // When )) is present, should still indent as if inside the inner call
    let code = "x <- f(a, x = f(a,\n))\n";
    let col = get_indentation_column(code, 1, rstudio_config(4));
    assert_eq!(
        col, 11,
        "Should use the lint-accepted nested-call column with auto-close"
    );
}
