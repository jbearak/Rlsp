//! TextEdit generation for R smart indentation.
//!
//! This module generates LSP TextEdit objects that replace existing
//! indentation with the computed target indentation.

use tower_lsp::lsp_types::{Position, Range, TextEdit};

use super::config::IndentationConfig;

/// Generates a TextEdit that replaces existing indentation with the target indentation.
///
/// The TextEdit range spans from column 0 to the end of existing whitespace,
/// ensuring that the LSP response completely replaces VS Code's declarative
/// indentation rather than adding to it.
///
/// # Arguments
///
/// * `line` - The line number to format (0-indexed)
/// * `target_column` - The target indentation column
/// * `config` - Configuration for tab/space generation
/// * `source` - The source code text
///
/// # Returns
///
/// A TextEdit that replaces the line's leading whitespace with the target indentation.
pub fn format_indentation(
    line: u32,
    target_column: u32,
    config: IndentationConfig,
    source: &str,
) -> TextEdit {
    // Calculate existing whitespace length on target line
    let existing_ws_len = source
        .lines()
        .nth(line as usize)
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .unwrap_or(0);

    // Generate new whitespace string
    let new_indent = generate_whitespace(target_column, &config);

    // Create TextEdit that replaces existing whitespace
    TextEdit {
        range: Range {
            start: Position { line, character: 0 },
            end: Position {
                line,
                character: existing_ws_len as u32,
            },
        },
        new_text: new_indent,
    }
}

/// Generates a whitespace string for the target column.
///
/// When `insert_spaces` is true, generates only spaces.
/// When `insert_spaces` is false, generates tabs with trailing spaces
/// for alignment if needed.
///
/// # Arguments
///
/// * `target_column` - The target indentation column
/// * `config` - Configuration for tab size and space/tab preference
///
/// # Returns
///
/// A string containing the appropriate whitespace characters.
fn generate_whitespace(target_column: u32, config: &IndentationConfig) -> String {
    if config.insert_spaces {
        " ".repeat(target_column as usize)
    } else {
        // Use tabs, with trailing spaces for alignment if needed
        let tab_size = config.tab_size.max(1); // Avoid division by zero
        let tabs = target_column / tab_size;
        let spaces = target_column % tab_size;
        let mut result = "\t".repeat(tabs as usize);
        result.push_str(&" ".repeat(spaces as usize));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indentation::IndentationStyle;
    use proptest::prelude::*;

    fn make_config(tab_size: u32, insert_spaces: bool) -> IndentationConfig {
        IndentationConfig {
            tab_size,
            insert_spaces,
            style: IndentationStyle::RStudio,
        }
    }

    #[test]
    fn test_generate_whitespace_spaces() {
        let config = make_config(2, true);
        assert_eq!(generate_whitespace(0, &config), "");
        assert_eq!(generate_whitespace(2, &config), "  ");
        assert_eq!(generate_whitespace(4, &config), "    ");
    }

    #[test]
    fn test_generate_whitespace_tabs() {
        let config = make_config(2, false);
        assert_eq!(generate_whitespace(0, &config), "");
        assert_eq!(generate_whitespace(2, &config), "\t");
        assert_eq!(generate_whitespace(4, &config), "\t\t");
    }

    #[test]
    fn test_generate_whitespace_tabs_with_trailing_spaces() {
        let config = make_config(4, false);
        assert_eq!(generate_whitespace(5, &config), "\t ");
        assert_eq!(generate_whitespace(6, &config), "\t  ");
        assert_eq!(generate_whitespace(7, &config), "\t   ");
    }

    #[test]
    fn test_format_indentation_replaces_existing() {
        let source = "    existing_indent";
        let config = make_config(2, true);

        let edit = format_indentation(0, 2, config, source);

        // Range should span from column 0 to 4 (length of existing whitespace)
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 4);

        // New text should be 2 spaces
        assert_eq!(edit.new_text, "  ");
    }

    #[test]
    fn test_format_indentation_no_existing_whitespace() {
        let source = "no_indent";
        let config = make_config(2, true);

        let edit = format_indentation(0, 4, config, source);

        // Range should be empty (0 to 0)
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 0);

        // New text should be 4 spaces
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_format_indentation_multiline() {
        let source = "line0\n  line1\n    line2";
        let config = make_config(2, true);

        let edit = format_indentation(1, 4, config, source);

        assert_eq!(edit.range.start.line, 1);
        assert_eq!(edit.range.end.line, 1);
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 2);
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_generate_whitespace_zero_column() {
        // Edge case: target_column of 0 should produce empty string
        let spaces_config = make_config(4, true);
        assert_eq!(generate_whitespace(0, &spaces_config), "");

        let tabs_config = make_config(4, false);
        assert_eq!(generate_whitespace(0, &tabs_config), "");
    }

    #[test]
    fn test_generate_whitespace_tab_size_zero_protection() {
        // Edge case: tab_size of 0 should be treated as 1 to avoid division by zero
        let config = make_config(0, false);
        // With tab_size clamped to 1: 4 / 1 = 4 tabs, 4 % 1 = 0 spaces
        assert_eq!(generate_whitespace(4, &config), "\t\t\t\t");
    }

    #[test]
    fn test_generate_whitespace_very_large_tab_size() {
        // Edge case: very large tab_size should be handled gracefully
        // Note: In practice, backend.rs clamps tab_size to 1-8, but the formatter
        // should still handle larger values without panicking
        let config = make_config(100, false);
        // With tab_size 100: 50 / 100 = 0 tabs, 50 % 100 = 50 spaces
        assert_eq!(generate_whitespace(50, &config), " ".repeat(50));

        // With tab_size 100: 100 / 100 = 1 tab, 100 % 100 = 0 spaces
        assert_eq!(generate_whitespace(100, &config), "\t");

        // With tab_size 100: 150 / 100 = 1 tab, 150 % 100 = 50 spaces
        assert_eq!(
            generate_whitespace(150, &config),
            format!("\t{}", " ".repeat(50))
        );
    }

    #[test]
    fn test_generate_whitespace_max_tab_size() {
        // Edge case: maximum reasonable tab_size (8, as clamped by backend)
        let config = make_config(8, false);
        // 16 / 8 = 2 tabs, 16 % 8 = 0 spaces
        assert_eq!(generate_whitespace(16, &config), "\t\t");
        // 20 / 8 = 2 tabs, 20 % 8 = 4 spaces
        assert_eq!(generate_whitespace(20, &config), "\t\t    ");
    }

    #[test]
    fn test_generate_whitespace_spaces_mode_ignores_tab_size() {
        // In spaces mode, tab_size doesn't affect the output (only column count matters)
        let config_small = make_config(1, true);
        let config_large = make_config(100, true);

        // Both should produce the same output for the same target column
        assert_eq!(generate_whitespace(10, &config_small), "          ");
        assert_eq!(generate_whitespace(10, &config_large), "          ");
    }

    #[test]
    fn test_format_indentation_empty_line() {
        // Edge case: empty line should have 0 existing whitespace
        let source = "line0\n\nline2";
        let config = make_config(2, true);

        let edit = format_indentation(1, 4, config, source);

        assert_eq!(edit.range.start.line, 1);
        assert_eq!(edit.range.end.line, 1);
        // Empty line has 0 whitespace
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 0);
        // New text should be 4 spaces
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_format_indentation_line_out_of_bounds() {
        // Edge case: line number beyond source should default to 0 existing whitespace
        let source = "line0\nline1";
        let config = make_config(2, true);

        // Line 5 doesn't exist in source
        let edit = format_indentation(5, 4, config, source);

        assert_eq!(edit.range.start.line, 5);
        assert_eq!(edit.range.end.line, 5);
        // Non-existent line defaults to 0 whitespace
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 0);
        // New text should still be generated correctly
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_format_indentation_whitespace_only_line() {
        // Edge case: line with only whitespace
        let source = "line0\n    \nline2";
        let config = make_config(2, true);

        let edit = format_indentation(1, 2, config, source);

        assert_eq!(edit.range.start.line, 1);
        assert_eq!(edit.range.end.line, 1);
        // Line has 4 whitespace characters
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 4);
        // New text should be 2 spaces
        assert_eq!(edit.new_text, "  ");
    }

    #[test]
    fn test_format_indentation_tab_whitespace() {
        // Edge case: existing whitespace includes tabs
        let source = "\t\tcode";
        let config = make_config(4, true);

        let edit = format_indentation(0, 4, config, source);

        assert_eq!(edit.range.start.character, 0);
        // 2 tab characters
        assert_eq!(edit.range.end.character, 2);
        // New text should be 4 spaces
        assert_eq!(edit.new_text, "    ");
    }

    // ========================================================================
    // Error Handling Unit Tests (Task 9.7)
    // Validates: Requirements 6.1, 8.3
    // ========================================================================

    #[test]
    fn test_format_indentation_default_insert_spaces() {
        // Test default behavior when using default config
        // Default should use insert_spaces = true
        let source = "  code";
        let config = IndentationConfig::default();

        assert!(
            config.insert_spaces,
            "Default config should have insert_spaces = true"
        );

        let edit = format_indentation(0, 4, config, source);

        // Should produce spaces, not tabs
        assert_eq!(edit.new_text, "    ");
        assert!(
            edit.new_text.chars().all(|c| c == ' '),
            "Default should produce only spaces"
        );
    }

    #[test]
    fn test_format_indentation_very_large_target_column() {
        // Edge case: very large target column
        let source = "code";
        let config = make_config(4, true);

        let edit = format_indentation(0, 1000, config, source);

        // Should produce 1000 spaces without panicking
        assert_eq!(edit.new_text.len(), 1000);
        assert!(edit.new_text.chars().all(|c| c == ' '));
    }

    #[test]
    fn test_format_indentation_zero_target_column() {
        // Edge case: target column of 0 (no indentation)
        let source = "    code";
        let config = make_config(4, true);

        let edit = format_indentation(0, 0, config, source);

        // Should produce empty string
        assert_eq!(edit.new_text, "");
        // Range should still cover existing whitespace
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 4);
    }

    #[test]
    fn test_format_indentation_line_with_only_tabs() {
        // Edge case: line with only tab characters as whitespace
        let source = "\t\t\tcode";
        let config = make_config(4, true);

        let edit = format_indentation(0, 8, config, source);

        // Range should cover 3 tab characters
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 3);
        // New text should be 8 spaces
        assert_eq!(edit.new_text, "        ");
    }

    #[test]
    fn test_format_indentation_mixed_tabs_and_spaces() {
        // Edge case: line with mixed tabs and spaces
        let source = "\t  \t code";
        let config = make_config(4, true);

        let edit = format_indentation(0, 4, config, source);

        // Range should cover all whitespace characters (tab, space, space, tab, space)
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 5);
        // New text should be 4 spaces
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_format_indentation_unicode_content() {
        // Edge case: line with unicode content after whitespace
        let source = "  变量 <- 1";
        let config = make_config(4, true);

        let edit = format_indentation(0, 4, config, source);

        // Range should cover 2 space characters
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 2);
        // New text should be 4 spaces
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_format_indentation_newline_in_source() {
        // Edge case: source with multiple lines, format middle line
        let source = "line0\n  line1\nline2";
        let config = make_config(4, true);

        let edit = format_indentation(1, 4, config, source);

        // Should format line 1
        assert_eq!(edit.range.start.line, 1);
        assert_eq!(edit.range.end.line, 1);
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 2);
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_format_indentation_last_line_no_newline() {
        // Edge case: last line without trailing newline
        let source = "line0\nline1";
        let config = make_config(4, true);

        let edit = format_indentation(1, 4, config, source);

        // Should format line 1
        assert_eq!(edit.range.start.line, 1);
        assert_eq!(edit.range.end.line, 1);
        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 0); // No leading whitespace
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_format_indentation_single_character_line() {
        // Edge case: single character line
        let source = "x";
        let config = make_config(4, true);

        let edit = format_indentation(0, 4, config, source);

        assert_eq!(edit.range.start.character, 0);
        assert_eq!(edit.range.end.character, 0); // No leading whitespace
        assert_eq!(edit.new_text, "    ");
    }

    #[test]
    fn test_format_indentation_tabs_mode_alignment() {
        // Test tabs mode with alignment (trailing spaces)
        let source = "code";
        let config = make_config(4, false);

        // Target column 6: 1 tab (4 cols) + 2 spaces
        let edit = format_indentation(0, 6, config, source);

        assert_eq!(edit.new_text, "\t  ");
    }

    #[test]
    fn test_format_indentation_tabs_mode_exact_multiple() {
        // Test tabs mode when target is exact multiple of tab_size
        let source = "code";
        let config = make_config(4, false);

        // Target column 8: 2 tabs, no spaces
        let edit = format_indentation(0, 8, config, source);

        assert_eq!(edit.new_text, "\t\t");
    }

    #[test]
    fn test_generate_whitespace_various_tab_sizes() {
        // Test different tab sizes for tabs mode
        let config_1 = make_config(1, false);
        assert_eq!(generate_whitespace(3, &config_1), "\t\t\t"); // 3 tabs (1 col each)

        let config_2 = make_config(2, false);
        assert_eq!(generate_whitespace(3, &config_2), "\t "); // 1 tab (2 cols) + 1 space

        let config_4 = make_config(4, false);
        assert_eq!(generate_whitespace(6, &config_4), "\t  "); // 1 tab (4 cols) + 2 spaces

        let config_8 = make_config(8, false);
        assert_eq!(generate_whitespace(10, &config_8), "\t  "); // 1 tab (8 cols) + 2 spaces
    }

    #[test]
    fn test_generate_whitespace_spaces_various_tab_sizes() {
        // Test different tab sizes for spaces mode
        // Note: tab_size doesn't affect space generation, but we verify consistency
        let config_1 = make_config(1, true);
        assert_eq!(generate_whitespace(4, &config_1), "    ");

        let config_2 = make_config(2, true);
        assert_eq!(generate_whitespace(4, &config_2), "    ");

        let config_4 = make_config(4, true);
        assert_eq!(generate_whitespace(4, &config_4), "    ");

        let config_8 = make_config(8, true);
        assert_eq!(generate_whitespace(4, &config_8), "    ");
    }

    // ========================================================================
    // Property-Based Tests
    // ========================================================================

    // Feature: r-smart-indentation, Property 10: FormattingOptions Respect
    // *For any* indentation computation, the system should read and apply both
    // tab_size and insert_spaces values from the LSP FormattingOptions parameter,
    // such that different tab_size values produce proportionally different
    // indentation amounts.
    // **Validates: Requirements 6.1, 6.2**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn property_formatting_options_respect(
            tab_size in 1u32..9,
            insert_spaces in proptest::bool::ANY,
            target_column in 0u32..100,
        ) {
            let config = IndentationConfig {
                tab_size,
                insert_spaces,
                style: IndentationStyle::RStudio,
            };

            let whitespace = generate_whitespace(target_column, &config);

            // Property 1: insert_spaces=true produces only spaces
            if insert_spaces {
                prop_assert!(
                    whitespace.chars().all(|c| c == ' '),
                    "When insert_spaces=true, whitespace should contain only spaces, \
                     but got {:?} for target_column={}, tab_size={}",
                    whitespace,
                    target_column,
                    tab_size
                );
                // The length should equal target_column
                prop_assert_eq!(
                    whitespace.len() as u32,
                    target_column,
                    "When insert_spaces=true, whitespace length should equal target_column"
                );
            } else {
                // Property 2: insert_spaces=false produces tabs (with possible trailing spaces)
                prop_assert!(
                    whitespace.chars().all(|c| c == '\t' || c == ' '),
                    "When insert_spaces=false, whitespace should contain only tabs and spaces, \
                     but got {:?} for target_column={}, tab_size={}",
                    whitespace,
                    target_column,
                    tab_size
                );

                // Verify tabs come before spaces (no interleaving)
                let tab_count = whitespace.chars().filter(|&c| c == '\t').count();
                let space_count = whitespace.chars().filter(|&c| c == ' ').count();
                let expected_structure = format!(
                    "{}{}",
                    "\t".repeat(tab_count),
                    " ".repeat(space_count)
                );
                prop_assert_eq!(
                    &whitespace,
                    &expected_structure,
                    "Tabs should come before spaces (no interleaving)"
                );
            }

            // Property 3: Visual width should always equal target_column
            let tab_count = whitespace.chars().filter(|&c| c == '\t').count() as u32;
            let space_count = whitespace.chars().filter(|&c| c == ' ').count() as u32;
            let visual_width = if insert_spaces {
                whitespace.len() as u32
            } else {
                (tab_count * tab_size) + space_count
            };
            prop_assert_eq!(
                visual_width,
                target_column,
                "Visual width should equal target_column regardless of insert_spaces setting"
            );
        }

        #[test]
        fn property_formatting_options_proportional_indentation(
            tab_size_a in 1u32..5,
            tab_size_b in 5u32..9,
            multiplier in 1u32..10,
        ) {
            // Property: Different tab_size values produce proportionally different
            // indentation amounts when using spaces mode
            let target_a = tab_size_a * multiplier;
            let target_b = tab_size_b * multiplier;

            let config_a = IndentationConfig {
                tab_size: tab_size_a,
                insert_spaces: true,
                style: IndentationStyle::RStudio,
            };
            let config_b = IndentationConfig {
                tab_size: tab_size_b,
                insert_spaces: true,
                style: IndentationStyle::RStudio,
            };

            let whitespace_a = generate_whitespace(target_a, &config_a);
            let whitespace_b = generate_whitespace(target_b, &config_b);

            // Property: The ratio of whitespace lengths should equal the ratio of targets
            // Since we're using spaces mode, length == target_column
            prop_assert_eq!(
                whitespace_a.len() as u32,
                target_a,
                "Whitespace length should equal target for config_a"
            );
            prop_assert_eq!(
                whitespace_b.len() as u32,
                target_b,
                "Whitespace length should equal target for config_b"
            );

            // Property: Different tab_sizes with same multiplier produce different amounts
            // (unless tab_size_a == tab_size_b, which our ranges prevent)
            prop_assert!(
                tab_size_a < tab_size_b,
                "tab_size_a should be less than tab_size_b by construction"
            );
            prop_assert!(
                target_a < target_b,
                "target_a should be less than target_b when tab_size_a < tab_size_b"
            );
            prop_assert!(
                whitespace_a.len() < whitespace_b.len(),
                "Smaller tab_size should produce smaller indentation for same multiplier"
            );
        }

        #[test]
        fn property_formatting_options_tab_size_affects_tab_count(
            tab_size in 1u32..9,
            target_column in 1u32..100,
        ) {
            // Property: In tabs mode, tab_size determines how many tabs are used
            let config = IndentationConfig {
                tab_size,
                insert_spaces: false,
                style: IndentationStyle::RStudio,
            };

            let whitespace = generate_whitespace(target_column, &config);
            let tab_count = whitespace.chars().filter(|&c| c == '\t').count() as u32;
            let space_count = whitespace.chars().filter(|&c| c == ' ').count() as u32;

            // Property: tab_count should equal target_column / tab_size
            prop_assert_eq!(
                tab_count,
                target_column / tab_size,
                "Tab count should equal target_column / tab_size. \
                 Expected {} / {} = {}, got {}",
                target_column,
                tab_size,
                target_column / tab_size,
                tab_count
            );

            // Property: space_count should equal target_column % tab_size
            prop_assert_eq!(
                space_count,
                target_column % tab_size,
                "Space count should equal target_column %% tab_size. \
                 Expected {} %% {} = {}, got {}",
                target_column,
                tab_size,
                target_column % tab_size,
                space_count
            );
        }

        #[test]
        fn property_format_indentation_respects_options(
            existing_ws_len in 0usize..20,
            target_column in 0u32..50,
            tab_size in 1u32..9,
            insert_spaces in proptest::bool::ANY,
        ) {
            // Property: format_indentation correctly applies FormattingOptions
            let existing_ws = " ".repeat(existing_ws_len);
            let source = format!("{}code", existing_ws);

            let config = IndentationConfig {
                tab_size,
                insert_spaces,
                style: IndentationStyle::RStudio,
            };

            let edit = format_indentation(0, target_column, config, &source);

            // Property 1: The new_text respects insert_spaces
            if insert_spaces {
                prop_assert!(
                    edit.new_text.chars().all(|c| c == ' '),
                    "When insert_spaces=true, TextEdit new_text should contain only spaces"
                );
                prop_assert_eq!(
                    edit.new_text.len() as u32,
                    target_column,
                    "When insert_spaces=true, new_text length should equal target_column"
                );
            } else {
                prop_assert!(
                    edit.new_text.chars().all(|c| c == '\t' || c == ' '),
                    "When insert_spaces=false, TextEdit new_text should contain only tabs and spaces"
                );
                // Verify visual width
                let tab_count = edit.new_text.chars().filter(|&c| c == '\t').count() as u32;
                let space_count = edit.new_text.chars().filter(|&c| c == ' ').count() as u32;
                let visual_width = (tab_count * tab_size) + space_count;
                prop_assert_eq!(
                    visual_width,
                    target_column,
                    "Visual width of new_text should equal target_column"
                );
            }
        }
    }

    // Feature: r-smart-indentation, Property 11: Whitespace Character Generation
    // *For any* indentation computation, when insert_spaces is true, the generated
    // indentation should contain only space characters; when insert_spaces is false,
    // the generated indentation should contain tab characters (with possible trailing
    // spaces for alignment).
    // **Validates: Requirements 6.3, 6.4**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn property_whitespace_character_generation_spaces(
            target_column in 0u32..200,
            tab_size in 1u32..9,
            style_idx in 0usize..2,
        ) {
            // Map index to style
            let style = match style_idx {
                0 => IndentationStyle::RStudio,
                _ => IndentationStyle::RStudioMinus,
            };

            let config = IndentationConfig {
                tab_size,
                insert_spaces: true,
                style,
            };

            let whitespace = generate_whitespace(target_column, &config);

            // Property: when insert_spaces is true, generated indentation should
            // contain ONLY space characters (no tabs)
            prop_assert!(
                whitespace.chars().all(|c| c == ' '),
                "When insert_spaces=true, whitespace should contain only spaces, \
                 but got {:?} for target_column={}, tab_size={}",
                whitespace,
                target_column,
                tab_size
            );

            // Additional property: length should equal target_column
            prop_assert_eq!(
                whitespace.len() as u32,
                target_column,
                "When insert_spaces=true, whitespace length should equal target_column"
            );
        }

        #[test]
        fn property_whitespace_character_generation_tabs(
            target_column in 0u32..200,
            tab_size in 1u32..9,
            style_idx in 0usize..2,
        ) {
            // Map index to style
            let style = match style_idx {
                0 => IndentationStyle::RStudio,
                _ => IndentationStyle::RStudioMinus,
            };

            let config = IndentationConfig {
                tab_size,
                insert_spaces: false,
                style,
            };

            let whitespace = generate_whitespace(target_column, &config);

            // Property: when insert_spaces is false, generated indentation should
            // contain only tabs and spaces (tabs for full tab_size increments,
            // trailing spaces for alignment)
            prop_assert!(
                whitespace.chars().all(|c| c == '\t' || c == ' '),
                "When insert_spaces=false, whitespace should contain only tabs and spaces, \
                 but got {:?} for target_column={}, tab_size={}",
                whitespace,
                target_column,
                tab_size
            );

            // Property: tabs should come before spaces (no interleaving)
            let tab_count = whitespace.chars().filter(|&c| c == '\t').count();
            let space_count = whitespace.chars().filter(|&c| c == ' ').count();

            // Verify structure: all tabs first, then all spaces
            let expected_structure = format!(
                "{}{}",
                "\t".repeat(tab_count),
                " ".repeat(space_count)
            );
            prop_assert_eq!(
                whitespace,
                expected_structure,
                "Tabs should come before spaces (no interleaving)"
            );

            // Property: the visual width should equal target_column
            // Visual width = (tab_count * tab_size) + space_count
            let visual_width = (tab_count as u32 * tab_size) + space_count as u32;
            prop_assert_eq!(
                visual_width,
                target_column,
                "Visual width of whitespace should equal target_column. \
                 Got {} tabs * {} + {} spaces = {}, expected {}",
                tab_count,
                tab_size,
                space_count,
                visual_width,
                target_column
            );

            // Property: number of trailing spaces should be less than tab_size
            // (otherwise we could use another tab)
            prop_assert!(
                space_count < tab_size as usize,
                "Trailing spaces ({}) should be less than tab_size ({})",
                space_count,
                tab_size
            );
        }

        // Feature: r-smart-indentation, Property 12: TextEdit Range Replacement
        // *For any* line with existing leading whitespace of length W, the generated
        // TextEdit should have a range spanning from (line, 0) to (line, W), ensuring
        // complete replacement of existing indentation.
        // **Validates: Requirements 6.5**
        #[test]
        fn property_textedit_range_replacement(
            existing_ws_len in 0usize..50,
            target_column in 0u32..100,
            tab_size in 1u32..9,
            insert_spaces in proptest::bool::ANY,
            line_content_len in 0usize..50,
            line_num in 0u32..10,
        ) {
            // Generate source code with specified whitespace and content
            let existing_ws = " ".repeat(existing_ws_len);
            let line_content: String = (0..line_content_len)
                .map(|i| (b'a' + (i % 26) as u8) as char)
                .collect();

            // Build multi-line source with the target line at the specified position
            let mut lines: Vec<String> = Vec::new();
            for i in 0..=line_num {
                if i == line_num {
                    lines.push(format!("{}{}", existing_ws, line_content));
                } else {
                    lines.push(format!("line{}", i));
                }
            }
            let source = lines.join("\n");

            let config = IndentationConfig {
                tab_size,
                insert_spaces,
                style: IndentationStyle::RStudio,
            };

            let edit = format_indentation(line_num, target_column, config, &source);

            // Property 1: Range start should always be at column 0
            prop_assert_eq!(
                edit.range.start.character,
                0,
                "TextEdit range should start at column 0"
            );

            // Property 2: Range start and end should be on the same line
            prop_assert_eq!(
                edit.range.start.line,
                line_num,
                "TextEdit range start should be on the target line"
            );
            prop_assert_eq!(
                edit.range.end.line,
                line_num,
                "TextEdit range end should be on the target line"
            );

            // Property 3: Range end should equal the existing whitespace length
            prop_assert_eq!(
                edit.range.end.character as usize,
                existing_ws_len,
                "TextEdit range end should equal existing whitespace length. \
                 Expected {}, got {} for source line {:?}",
                existing_ws_len,
                edit.range.end.character,
                source.lines().nth(line_num as usize)
            );

            // Property 4: The range should span exactly the existing whitespace
            let range_length = edit.range.end.character - edit.range.start.character;
            prop_assert_eq!(
                range_length as usize,
                existing_ws_len,
                "TextEdit range length should equal existing whitespace length"
            );
        }

        // Feature: r-smart-indentation, Property 12: TextEdit Range Replacement (mixed whitespace)
        // Additional test for lines with mixed tab/space whitespace
        // **Validates: Requirements 6.5**
        #[test]
        fn property_textedit_range_replacement_mixed_whitespace(
            num_tabs in 0usize..10,
            num_spaces in 0usize..20,
            target_column in 0u32..100,
            tab_size in 1u32..9,
            insert_spaces in proptest::bool::ANY,
        ) {
            // Generate source with mixed tabs and spaces as leading whitespace
            let existing_ws = format!("{}{}", "\t".repeat(num_tabs), " ".repeat(num_spaces));
            let total_ws_chars = num_tabs + num_spaces;
            let source = format!("{}code", existing_ws);

            let config = IndentationConfig {
                tab_size,
                insert_spaces,
                style: IndentationStyle::RStudio,
            };

            let edit = format_indentation(0, target_column, config, &source);

            // Property: Range should span all whitespace characters (tabs + spaces)
            // regardless of their visual width
            prop_assert_eq!(
                edit.range.start.character,
                0,
                "TextEdit range should start at column 0"
            );
            prop_assert_eq!(
                edit.range.end.character as usize,
                total_ws_chars,
                "TextEdit range end should equal total whitespace character count \
                 (tabs + spaces). Expected {} ({} tabs + {} spaces), got {}",
                total_ws_chars,
                num_tabs,
                num_spaces,
                edit.range.end.character
            );
        }

        // Feature: r-smart-indentation, Property 14: TextEdit Response Structure
        // *For any* onTypeFormatting request, the handler should return a result
        // containing a Vec<TextEdit>, where each TextEdit specifies a range and
        // new_text for indentation replacement.
        // **Validates: Requirements 8.4**
        #[test]
        fn property_textedit_response_structure(
            existing_ws_len in 0usize..30,
            target_column in 0u32..100,
            tab_size in 1u32..9,
            insert_spaces in proptest::bool::ANY,
            line_num in 0u32..5,
            content_len in 1usize..20,
        ) {
            // Generate random source code with varying whitespace
            let existing_ws = " ".repeat(existing_ws_len);
            let content: String = (0..content_len)
                .map(|i| (b'a' + (i % 26) as u8) as char)
                .collect();

            // Build multi-line source
            let mut lines: Vec<String> = Vec::new();
            for i in 0..=line_num {
                if i == line_num {
                    lines.push(format!("{}{}", existing_ws, content));
                } else {
                    lines.push(format!("line{}", i));
                }
            }
            let source = lines.join("\n");

            let config = IndentationConfig {
                tab_size,
                insert_spaces,
                style: IndentationStyle::RStudio,
            };

            // Call format_indentation to get a TextEdit
            let edit = format_indentation(line_num, target_column, config, &source);

            // Property 1: TextEdit has a valid range where start.line == end.line == target line
            prop_assert_eq!(
                edit.range.start.line,
                line_num,
                "TextEdit range start line should equal target line"
            );
            prop_assert_eq!(
                edit.range.end.line,
                line_num,
                "TextEdit range end line should equal target line"
            );

            // Property 2: The range starts at column 0
            prop_assert_eq!(
                edit.range.start.character,
                0,
                "TextEdit range should start at column 0"
            );

            // Property 3: The range ends at the existing whitespace length
            prop_assert_eq!(
                edit.range.end.character as usize,
                existing_ws_len,
                "TextEdit range end should equal existing whitespace length. \
                 Expected {}, got {}",
                existing_ws_len,
                edit.range.end.character
            );

            // Property 4: The new_text contains the correct whitespace for the target column
            if insert_spaces {
                // When using spaces, new_text length should equal target_column
                prop_assert_eq!(
                    edit.new_text.len() as u32,
                    target_column,
                    "When insert_spaces=true, new_text length should equal target_column"
                );
                prop_assert!(
                    edit.new_text.chars().all(|c| c == ' '),
                    "When insert_spaces=true, new_text should contain only spaces"
                );
            } else {
                // When using tabs, verify visual width equals target_column
                let tab_count = edit.new_text.chars().filter(|&c| c == '\t').count() as u32;
                let space_count = edit.new_text.chars().filter(|&c| c == ' ').count() as u32;
                let visual_width = (tab_count * tab_size) + space_count;
                prop_assert_eq!(
                    visual_width,
                    target_column,
                    "Visual width of new_text should equal target_column"
                );
                prop_assert!(
                    edit.new_text.chars().all(|c| c == '\t' || c == ' '),
                    "When insert_spaces=false, new_text should contain only tabs and spaces"
                );
            }

            // Property 5: The TextEdit is well-formed (range is valid, new_text is not None)
            prop_assert!(
                edit.range.start.character <= edit.range.end.character,
                "TextEdit range start should be <= end"
            );
        }

        // Feature: r-smart-indentation, Property 14: TextEdit Response Structure (edge cases)
        // Additional test for edge cases: empty lines, whitespace-only lines, various configs
        // **Validates: Requirements 8.4**
        #[test]
        fn property_textedit_response_structure_edge_cases(
            tab_size in 1u32..9,
            insert_spaces in proptest::bool::ANY,
            target_column in 0u32..50,
            style_idx in 0usize..2,
        ) {
            let style = match style_idx {
                0 => IndentationStyle::RStudio,
                _ => IndentationStyle::RStudioMinus,
            };

            let config = IndentationConfig {
                tab_size,
                insert_spaces,
                style,
            };

            // Test case 1: Empty line (no whitespace, no content)
            let empty_source = "line0\n\nline2";
            let edit_empty = format_indentation(1, target_column, config.clone(), empty_source);

            prop_assert_eq!(
                edit_empty.range.start.line,
                1,
                "Empty line: range start line should be 1"
            );
            prop_assert_eq!(
                edit_empty.range.end.line,
                1,
                "Empty line: range end line should be 1"
            );
            prop_assert_eq!(
                edit_empty.range.start.character,
                0,
                "Empty line: range should start at column 0"
            );
            prop_assert_eq!(
                edit_empty.range.end.character,
                0,
                "Empty line: range should end at column 0 (no existing whitespace)"
            );

            // Test case 2: Whitespace-only line
            let ws_only_source = "line0\n    \nline2";
            let edit_ws = format_indentation(1, target_column, config.clone(), ws_only_source);

            prop_assert_eq!(
                edit_ws.range.start.line,
                1,
                "Whitespace-only line: range start line should be 1"
            );
            prop_assert_eq!(
                edit_ws.range.end.line,
                1,
                "Whitespace-only line: range end line should be 1"
            );
            prop_assert_eq!(
                edit_ws.range.start.character,
                0,
                "Whitespace-only line: range should start at column 0"
            );
            prop_assert_eq!(
                edit_ws.range.end.character,
                4,
                "Whitespace-only line: range should end at column 4 (4 spaces)"
            );

            // Test case 3: Line with no existing whitespace
            let no_ws_source = "code";
            let edit_no_ws = format_indentation(0, target_column, config.clone(), no_ws_source);

            prop_assert_eq!(
                edit_no_ws.range.start.character,
                0,
                "No whitespace: range should start at column 0"
            );
            prop_assert_eq!(
                edit_no_ws.range.end.character,
                0,
                "No whitespace: range should end at column 0"
            );

            // Verify new_text is correctly generated for all cases
            if insert_spaces {
                prop_assert_eq!(
                    edit_empty.new_text.len() as u32,
                    target_column,
                    "Empty line: new_text length should equal target_column"
                );
                prop_assert_eq!(
                    edit_ws.new_text.len() as u32,
                    target_column,
                    "Whitespace-only line: new_text length should equal target_column"
                );
                prop_assert_eq!(
                    edit_no_ws.new_text.len() as u32,
                    target_column,
                    "No whitespace: new_text length should equal target_column"
                );
            }
        }
    }
}
