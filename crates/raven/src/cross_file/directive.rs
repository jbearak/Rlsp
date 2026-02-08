//
// cross_file/directive.rs
//
// Directive parsing for cross-file awareness
//

use regex::Regex;
use std::sync::OnceLock;

use super::types::{
    BackwardDirective, CallSiteSpec, CrossFileMetadata, DeclaredSymbol, ForwardSource,
};

/// Compiled regex patterns for directive parsing
struct DirectivePatterns {
    backward: Regex,
    forward: Regex,
    working_dir: Regex,
    ignore: Regex,
    ignore_next: Regex,
    declare_var: Regex,
    declare_func: Regex,
}

/// Extract path from capture groups (double-quoted, single-quoted, or unquoted)
fn capture_path(caps: &regex::Captures, base_group: usize) -> Option<String> {
    // Try double-quoted (base_group)
    if let Some(m) = caps.get(base_group) {
        if !m.as_str().is_empty() {
            return Some(m.as_str().to_string());
        }
    }
    // Try single-quoted (base_group + 1)
    if let Some(m) = caps.get(base_group + 1) {
        if !m.as_str().is_empty() {
            return Some(m.as_str().to_string());
        }
    }
    // Try unquoted (base_group + 2)
    if let Some(m) = caps.get(base_group + 2) {
        if !m.as_str().is_empty() {
            return Some(m.as_str().to_string());
        }
    }
    None
}

/// Extract symbol name from capture groups (double-quoted, single-quoted, or unquoted).
/// Returns None if the symbol name is empty or whitespace-only.
/// Requirements: 1.4, 1.5, 2.4, 2.5, 3.4
fn capture_symbol_name(caps: &regex::Captures, base_group: usize) -> Option<String> {
    let name = capture_path(caps, base_group)?;
    // Skip empty or whitespace-only symbol names
    if name.trim().is_empty() {
        return None;
    }
    Some(name)
}

fn patterns() -> &'static DirectivePatterns {
    static PATTERNS: OnceLock<DirectivePatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // Path pattern: "quoted with spaces" or 'single quoted' or unquoted
        // Groups: 1=double-quoted, 2=single-quoted, 3=unquoted
        DirectivePatterns {
            backward: Regex::new(
                r#"#\s*@?lsp-(?:sourced-by|run-by|included-by)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+line\s*=\s*(\d+))?(?:\s+match\s*=\s*["']([^"']+)["'])?"#
            ).unwrap(),
            forward: Regex::new(
                r#"#\s*@?lsp-(?:source|run|include)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+line\s*=\s*(\d+))?"#
            ).unwrap(),
            working_dir: Regex::new(
                r#"#\s*@?lsp-(?:working-directory|working-dir|current-directory|current-dir|cd|wd)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))"#
            ).unwrap(),
            ignore: Regex::new(
                r"#\s*@?lsp-ignore\s*:?\s*$"
            ).unwrap(),
            ignore_next: Regex::new(
                r"#\s*@?lsp-ignore-next\s*:?\s*$"
            ).unwrap(),
            // Declaration directives for variables
            // Synonyms: @lsp-declare-variable, @lsp-declare-var, @lsp-variable, @lsp-var
            // Groups: 1=double-quoted, 2=single-quoted, 3=unquoted
            // Requirements: 1.1, 1.2, 1.3
            declare_var: Regex::new(
                r#"#\s*@lsp-(?:declare-variable|declare-var|variable|var)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))"#
            ).unwrap(),
            // Declaration directives for functions
            // Synonyms: @lsp-declare-function, @lsp-declare-func, @lsp-function, @lsp-func
            // Groups: 1=double-quoted, 2=single-quoted, 3=unquoted
            // Requirements: 2.1, 2.2, 2.3
            declare_func: Regex::new(
                r#"#\s*@lsp-(?:declare-function|declare-func|function|func)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))"#
            ).unwrap(),
        }
    })
}

/// Parse directives from file content.
/// Extracts @lsp-* directives including sourced-by, source, working-directory, and ignore directives.
pub fn parse_directives(content: &str) -> CrossFileMetadata {
    log::trace!("Starting directive parsing");
    let patterns = patterns();
    let mut meta = CrossFileMetadata::default();

    for (line_num, line) in content.lines().enumerate() {
        let line_num = line_num as u32;

        // Check backward directives
        if let Some(caps) = patterns.backward.captures(line) {
            let path = capture_path(&caps, 1).unwrap_or_default();
            let call_site = if let Some(line_match) = caps.get(4) {
                // Convert 1-based user input to 0-based internal
                let user_line: u32 = line_match.as_str().parse().unwrap_or(1);
                CallSiteSpec::Line(user_line.saturating_sub(1))
            } else if let Some(match_pattern) = caps.get(5) {
                CallSiteSpec::Match(match_pattern.as_str().to_string())
            } else {
                CallSiteSpec::Default
            };
            log::trace!(
                "  Parsed backward directive at line {}: path='{}' call_site={:?}",
                line_num,
                path,
                call_site
            );
            meta.sourced_by.push(BackwardDirective {
                path,
                call_site,
                directive_line: line_num,
            });
            continue;
        }

        // Check forward directive
        if let Some(caps) = patterns.forward.captures(line) {
            let path = capture_path(&caps, 1).unwrap_or_default();
            // Parse line=N parameter from capture group 4 if present
            // Convert from 1-based user input to 0-based internal (N-1)
            // Use directive's own line when no line= parameter
            let (call_site_line, has_explicit_line, is_line_zero) =
                if let Some(line_match) = caps.get(4) {
                    let user_line: u32 = line_match.as_str().parse().unwrap_or(0);
                    let is_zero = user_line == 0;
                    // For line=0, treat as line=1 (internal 0) but flag it as invalid
                    let effective_line = if user_line == 0 {
                        0
                    } else {
                        user_line.saturating_sub(1)
                    };
                    (effective_line, true, is_zero)
                } else {
                    (line_num, false, false)
                };
            log::trace!(
                "  Parsed forward directive at line {}: path='{}' call_site_line={} explicit_line={} user_line_zero={}",
                line_num,
                path,
                call_site_line,
                has_explicit_line,
                is_line_zero
            );
            meta.sources.push(ForwardSource {
                path,
                line: call_site_line,
                column: 0, // Always 0 for directive-based sources
                is_directive: true,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
                explicit_line: has_explicit_line,
                directive_line: line_num,
                user_line_zero: is_line_zero,
            });
            continue;
        }

        // Check working directory directive
        if let Some(caps) = patterns.working_dir.captures(line) {
            let path = capture_path(&caps, 1).unwrap_or_default();
            log::trace!(
                "  Parsed working directory directive at line {}: path='{}'",
                line_num,
                path
            );
            meta.working_directory = Some(path);
            continue;
        }

        // Check ignore directives
        if patterns.ignore.is_match(line) {
            log::trace!("  Parsed @lsp-ignore directive at line {}", line_num);
            meta.ignored_lines.insert(line_num);
            continue;
        }

        if patterns.ignore_next.is_match(line) {
            log::trace!("  Parsed @lsp-ignore-next directive at line {}", line_num);
            meta.ignored_next_lines.insert(line_num + 1);
            continue;
        }

        // Check variable declaration directives (@lsp-var, @lsp-variable, etc.)
        // Requirements: 1.1, 1.2, 1.3, 1.4, 1.5
        if let Some(caps) = patterns.declare_var.captures(line) {
            if let Some(name) = capture_symbol_name(&caps, 1) {
                log::trace!(
                    "  Parsed variable declaration directive at line {}: name='{}'",
                    line_num,
                    name
                );
                meta.declared_variables.push(DeclaredSymbol {
                    name,
                    line: line_num,
                    is_function: false,
                });
            }
            continue;
        }

        // Check function declaration directives (@lsp-func, @lsp-function, etc.)
        // Requirements: 2.1, 2.2, 2.3, 2.4, 2.5
        if let Some(caps) = patterns.declare_func.captures(line) {
            if let Some(name) = capture_symbol_name(&caps, 1) {
                log::trace!(
                    "  Parsed function declaration directive at line {}: name='{}'",
                    line_num,
                    name
                );
                meta.declared_functions.push(DeclaredSymbol {
                    name,
                    line: line_num,
                    is_function: true,
                });
            }
            continue;
        }
    }

    log::trace!(
        "Completed directive parsing: {} backward directives, {} forward directives, working_dir={:?}, {} ignored lines, {} declared vars, {} declared funcs",
        meta.sourced_by.len(),
        meta.sources.len(),
        meta.working_directory,
        meta.ignored_lines.len() + meta.ignored_next_lines.len(),
        meta.declared_variables.len(),
        meta.declared_functions.len()
    );

    meta
}

/// Check if a line should have diagnostics suppressed.
/// Returns true if line has @lsp-ignore or is targeted by @lsp-ignore-next.
pub fn is_line_ignored(metadata: &CrossFileMetadata, line: u32) -> bool {
    metadata.ignored_lines.contains(&line) || metadata.ignored_next_lines.contains(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backward_directive_basic() {
        let content = "# @lsp-sourced-by ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Default);
    }

    #[test]
    fn test_backward_directive_with_colon() {
        let content = "# @lsp-sourced-by: ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_backward_directive_quoted() {
        let content = r#"# @lsp-sourced-by "../main.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_backward_directive_single_quoted() {
        let content = "# @lsp-sourced-by '../main.R'";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_backward_directive_with_line() {
        let content = "# @lsp-sourced-by ../main.R line=15";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Line(14)); // 0-based
    }

    #[test]
    fn test_backward_directive_with_match() {
        let content = r#"# @lsp-sourced-by ../main.R match="source(""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(
            meta.sourced_by[0].call_site,
            CallSiteSpec::Match("source(".to_string())
        );
    }

    #[test]
    fn test_backward_directive_synonyms() {
        let content = "# @lsp-run-by ../main.R\n# @lsp-included-by ../other.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 2);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
        assert_eq!(meta.sourced_by[1].path, "../other.R");
    }

    #[test]
    fn test_forward_directive() {
        let content = "# @lsp-source utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert!(meta.sources[0].is_directive);
    }

    #[test]
    fn test_forward_directive_with_colon_and_quotes() {
        let content = r#"# @lsp-source: "utils/helpers.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils/helpers.R");
    }

    #[test]
    fn test_working_directory_directive() {
        let content = "# @lsp-working-directory /data/scripts";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, Some("/data/scripts".to_string()));
    }

    #[test]
    fn test_working_directory_synonyms() {
        for directive in [
            "@lsp-wd",
            "@lsp-cd",
            "@lsp-current-directory",
            "@lsp-current-dir",
            "@lsp-working-dir",
        ] {
            let content = format!("# {} /data", directive);
            let meta = parse_directives(&content);
            assert_eq!(
                meta.working_directory,
                Some("/data".to_string()),
                "Failed for {}",
                directive
            );
        }
    }

    #[test]
    fn test_ignore_directive() {
        let content = "x <- 1\n# @lsp-ignore\ny <- undefined";
        let meta = parse_directives(content);
        assert!(meta.ignored_lines.contains(&1));
    }

    #[test]
    fn test_ignore_next_directive() {
        let content = "# @lsp-ignore-next\ny <- undefined";
        let meta = parse_directives(content);
        assert!(meta.ignored_next_lines.contains(&1));
    }

    #[test]
    fn test_is_line_ignored() {
        let content = "# @lsp-ignore\nx <- 1\n# @lsp-ignore-next\ny <- 2";
        let meta = parse_directives(content);
        assert!(is_line_ignored(&meta, 0)); // @lsp-ignore line
        assert!(!is_line_ignored(&meta, 1)); // x <- 1
        assert!(!is_line_ignored(&meta, 2)); // @lsp-ignore-next line
        assert!(is_line_ignored(&meta, 3)); // y <- 2 (next line after ignore-next)
    }

    #[test]
    fn test_multiple_directives() {
        let content = r#"# @lsp-sourced-by ../main.R line=10
# @lsp-working-directory /data
source("utils.R")
# @lsp-source helpers.R
# @lsp-ignore
x <- undefined"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sources.len(), 1); // Only directive, not source() call
        assert_eq!(meta.working_directory, Some("/data".to_string()));
        assert!(meta.ignored_lines.contains(&4));
    }

    // Tests for quoted paths with spaces (Requirements 2.1-2.6)
    #[test]
    fn test_backward_directive_double_quoted_with_spaces() {
        let content = r#"# @lsp-sourced-by "path with spaces/main.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "path with spaces/main.R");
    }

    #[test]
    fn test_backward_directive_single_quoted_with_spaces() {
        let content = "# @lsp-sourced-by 'path with spaces/main.R'";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "path with spaces/main.R");
    }

    #[test]
    fn test_backward_directive_with_colon_and_spaces() {
        let content = r#"# @lsp-sourced-by: "my folder/main.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "my folder/main.R");
    }

    #[test]
    fn test_backward_directive_with_spaces_and_line() {
        let content = r#"# @lsp-sourced-by "path with spaces/main.R" line=15"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "path with spaces/main.R");
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Line(14));
    }

    #[test]
    fn test_forward_directive_double_quoted_with_spaces() {
        let content = r#"# @lsp-source "utils folder/helpers.R""#;
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils folder/helpers.R");
    }

    #[test]
    fn test_forward_directive_single_quoted_with_spaces() {
        let content = "# @lsp-source 'utils folder/helpers.R'";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils folder/helpers.R");
    }

    // Tests for forward directive synonyms (@lsp-run, @lsp-include)
    #[test]
    fn test_forward_directive_lsp_run_synonym() {
        let content = "# @lsp-run utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert!(meta.sources[0].is_directive);
    }

    #[test]
    fn test_forward_directive_lsp_include_synonym() {
        let content = "# @lsp-include utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert!(meta.sources[0].is_directive);
    }

    #[test]
    fn test_forward_directive_synonyms_all() {
        // Test all three synonyms produce identical results
        for directive in ["@lsp-source", "@lsp-run", "@lsp-include"] {
            let content = format!("# {} utils.R", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(meta.sources[0].path, "utils.R", "Failed for {}", directive);
            assert!(meta.sources[0].is_directive, "Failed for {}", directive);
        }
    }

    #[test]
    fn test_forward_directive_synonyms_no_at_prefix() {
        for directive in ["lsp-source", "lsp-run", "lsp-include"] {
            let content = format!("# {} utils.R", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(meta.sources[0].path, "utils.R", "Failed for {}", directive);
        }
    }

    #[test]
    fn test_forward_directive_synonyms_with_colon() {
        for directive in ["@lsp-source:", "@lsp-run:", "@lsp-include:"] {
            let content = format!("# {} utils.R", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(meta.sources[0].path, "utils.R", "Failed for {}", directive);
        }
    }

    #[test]
    fn test_forward_directive_synonyms_with_quotes() {
        for directive in ["@lsp-source", "@lsp-run", "@lsp-include"] {
            let content = format!(r#"# {} "path/to/file.R""#, directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(
                meta.sources[0].path, "path/to/file.R",
                "Failed for {}",
                directive
            );
        }
    }

    // Tests for forward directive line=N parameter (regex capture verification)
    #[test]
    fn test_forward_directive_line_param_regex_capture() {
        // Verify the regex correctly captures the line=N parameter
        // The actual parsing of line= is done in task 1.2, but we verify the regex here
        let patterns = patterns();

        // Test with line= parameter
        let line = "# @lsp-source utils.R line=15";
        let caps = patterns.forward.captures(line).expect("Should match");

        // Path should be in group 3 (unquoted)
        assert_eq!(caps.get(3).map(|m| m.as_str()), Some("utils.R"));
        // Line should be in group 4
        assert_eq!(caps.get(4).map(|m| m.as_str()), Some("15"));
    }

    #[test]
    fn test_forward_directive_line_param_regex_capture_all_synonyms() {
        let patterns = patterns();

        for directive in ["@lsp-source", "@lsp-run", "@lsp-include"] {
            let line = format!("# {} utils.R line=42", directive);
            let caps = patterns
                .forward
                .captures(&line)
                .expect(&format!("Should match for {}", directive));

            // Path should be in group 3 (unquoted)
            assert_eq!(
                caps.get(3).map(|m| m.as_str()),
                Some("utils.R"),
                "Path failed for {}",
                directive
            );
            // Line should be in group 4
            assert_eq!(
                caps.get(4).map(|m| m.as_str()),
                Some("42"),
                "Line failed for {}",
                directive
            );
        }
    }

    #[test]
    fn test_forward_directive_line_param_regex_capture_with_quotes() {
        let patterns = patterns();

        // Test with double-quoted path and line=
        let line = r#"# @lsp-source "path/to/file.R" line=10"#;
        let caps = patterns.forward.captures(line).expect("Should match");

        // Path should be in group 1 (double-quoted)
        assert_eq!(caps.get(1).map(|m| m.as_str()), Some("path/to/file.R"));
        // Line should be in group 4
        assert_eq!(caps.get(4).map(|m| m.as_str()), Some("10"));
    }

    #[test]
    fn test_forward_directive_line_param_regex_capture_with_colon() {
        let patterns = patterns();

        // Test with colon and line=
        let line = "# @lsp-source: utils.R line=5";
        let caps = patterns.forward.captures(line).expect("Should match");

        // Path should be in group 3 (unquoted)
        assert_eq!(caps.get(3).map(|m| m.as_str()), Some("utils.R"));
        // Line should be in group 4
        assert_eq!(caps.get(4).map(|m| m.as_str()), Some("5"));
    }

    #[test]
    fn test_forward_directive_line_param_regex_capture_without_line() {
        let patterns = patterns();

        // Test without line= parameter (should still match, group 4 should be None)
        let line = "# @lsp-source utils.R";
        let caps = patterns.forward.captures(line).expect("Should match");

        // Path should be in group 3 (unquoted)
        assert_eq!(caps.get(3).map(|m| m.as_str()), Some("utils.R"));
        // Line should be None (not present)
        assert_eq!(caps.get(4), None);
    }

    // Tests for forward directive line=N parameter parsing (Requirements 2.1, 2.2, 2.3, 2.4)
    #[test]
    fn test_forward_directive_line_param_parsing_basic() {
        // Requirement 2.1: Convert from 1-based user input to 0-based internal (N-1)
        let content = "# @lsp-source utils.R line=15";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].line, 14); // 15 - 1 = 14 (0-based)
        assert_eq!(meta.sources[0].column, 0); // Requirement 2.4: column=0 for directives
        assert!(meta.sources[0].is_directive);
    }

    #[test]
    fn test_forward_directive_line_param_parsing_line_1() {
        // Edge case: line=1 should become 0
        let content = "# @lsp-source utils.R line=1";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].line, 0); // 1 - 1 = 0
        assert!(meta.sources[0].explicit_line);
        assert!(!meta.sources[0].user_line_zero);
    }

    #[test]
    fn test_forward_directive_line_param_parsing_line_0() {
        // Edge case: line=0 is invalid (1-based numbering), should be flagged
        let content = "# @lsp-source utils.R line=0";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].line, 0); // Treated as line 1 (internal 0)
        assert!(meta.sources[0].explicit_line);
        assert!(meta.sources[0].user_line_zero); // Flag that line=0 was specified
        assert_eq!(meta.sources[0].directive_line, 0); // Directive is on line 0
    }

    #[test]
    fn test_forward_directive_without_line_param_uses_directive_line() {
        // Requirement 2.2: Use directive's own line when no line= parameter
        let content = "x <- 1\ny <- 2\n# @lsp-source utils.R\nz <- 3";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].line, 2); // Directive is on line 2 (0-based)
        assert_eq!(meta.sources[0].column, 0);
    }

    #[test]
    fn test_forward_directive_line_param_all_synonyms() {
        // Verify line= works with all synonyms
        for directive in ["@lsp-source", "@lsp-run", "@lsp-include"] {
            let content = format!("# {} utils.R line=10", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.sources.len(), 1, "Failed for {}", directive);
            assert_eq!(
                meta.sources[0].line,
                9, // 10 - 1 = 9 (0-based)
                "Line conversion failed for {}",
                directive
            );
            assert_eq!(
                meta.sources[0].column, 0,
                "Column should be 0 for {}",
                directive
            );
        }
    }

    #[test]
    fn test_forward_directive_line_param_with_quotes() {
        // Test line= with quoted path
        let content = r#"# @lsp-source "path/to/file.R" line=20"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "path/to/file.R");
        assert_eq!(meta.sources[0].line, 19); // 20 - 1 = 19
        assert_eq!(meta.sources[0].column, 0);
    }

    #[test]
    fn test_forward_directive_line_param_with_colon() {
        // Test line= with colon separator
        let content = "# @lsp-source: utils.R line=5";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
        assert_eq!(meta.sources[0].line, 4); // 5 - 1 = 4
    }

    #[test]
    fn test_forward_directive_multiple_with_different_lines() {
        // Requirement 2.3: Multiple directives create separate ForwardSource entries
        let content = "# @lsp-source a.R line=10\n# @lsp-source b.R line=20\n# @lsp-source c.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 3);

        // First directive: explicit line=10 -> 9
        assert_eq!(meta.sources[0].path, "a.R");
        assert_eq!(meta.sources[0].line, 9);

        // Second directive: explicit line=20 -> 19
        assert_eq!(meta.sources[1].path, "b.R");
        assert_eq!(meta.sources[1].line, 19);

        // Third directive: no line=, uses directive's own line (2)
        assert_eq!(meta.sources[2].path, "c.R");
        assert_eq!(meta.sources[2].line, 2);
    }

    #[test]
    fn test_forward_directive_line_param_large_value() {
        // Test with a large line number
        let content = "# @lsp-source utils.R line=1000";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].line, 999); // 1000 - 1 = 999
    }

    #[test]
    fn test_forward_directive_column_always_zero() {
        // Requirement 2.4: column=0 for all directive-based sources
        let content = "    # @lsp-source utils.R line=5"; // Indented directive
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].column, 0); // Column is always 0, not the indentation
    }

    #[test]
    fn test_working_dir_double_quoted_with_spaces() {
        let content = r#"# @lsp-cd "/data/my project""#;
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, Some("/data/my project".to_string()));
    }

    #[test]
    fn test_working_dir_single_quoted_with_spaces() {
        let content = "# @lsp-wd '/data/my project'";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, Some("/data/my project".to_string()));
    }

    // Tests for directives without '@' prefix
    #[test]
    fn test_backward_directive_no_at_prefix() {
        let content = "# lsp-sourced-by ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_backward_directive_no_at_prefix_with_colon() {
        let content = "# lsp-sourced-by: ../main.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
    }

    #[test]
    fn test_backward_directive_no_at_prefix_synonyms() {
        let content = "# lsp-run-by ../main.R\n# lsp-included-by ../other.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 2);
        assert_eq!(meta.sourced_by[0].path, "../main.R");
        assert_eq!(meta.sourced_by[1].path, "../other.R");
    }

    #[test]
    fn test_forward_directive_no_at_prefix() {
        let content = "# lsp-source utils.R";
        let meta = parse_directives(content);
        assert_eq!(meta.sources.len(), 1);
        assert_eq!(meta.sources[0].path, "utils.R");
    }

    #[test]
    fn test_working_dir_no_at_prefix() {
        let content = "# lsp-wd /data/scripts";
        let meta = parse_directives(content);
        assert_eq!(meta.working_directory, Some("/data/scripts".to_string()));
    }

    #[test]
    fn test_working_dir_no_at_prefix_synonyms() {
        for directive in [
            "lsp-cd",
            "lsp-working-directory",
            "lsp-working-dir",
            "lsp-current-directory",
            "lsp-current-dir",
        ] {
            let content = format!("# {} /data", directive);
            let meta = parse_directives(&content);
            assert_eq!(
                meta.working_directory,
                Some("/data".to_string()),
                "Failed for {}",
                directive
            );
        }
    }

    #[test]
    fn test_ignore_directive_no_at_prefix() {
        let content = "x <- 1\n# lsp-ignore\ny <- undefined";
        let meta = parse_directives(content);
        assert!(meta.ignored_lines.contains(&1));
    }

    #[test]
    fn test_ignore_next_directive_no_at_prefix() {
        let content = "# lsp-ignore-next\ny <- undefined";
        let meta = parse_directives(content);
        assert!(meta.ignored_next_lines.contains(&1));
    }

    // ============================================================================
    // Tests for declaration directives (@lsp-var, @lsp-func, etc.)
    // Requirements: 1.1-1.6, 2.1-2.6, 3.4
    // ============================================================================

    // Variable declaration directive tests
    #[test]
    fn test_declare_var_basic() {
        let content = "# @lsp-var myvar";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "myvar");
        assert_eq!(meta.declared_variables[0].line, 0);
        assert!(!meta.declared_variables[0].is_function);
    }

    #[test]
    fn test_declare_var_with_colon() {
        let content = "# @lsp-var: myvar";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "myvar");
    }

    #[test]
    fn test_declare_var_double_quoted() {
        let content = r#"# @lsp-var "my.var""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "my.var");
    }

    #[test]
    fn test_declare_var_single_quoted() {
        let content = "# @lsp-var 'my.var'";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "my.var");
    }

    #[test]
    fn test_declare_var_all_synonyms() {
        // Test all 4 synonym forms: @lsp-declare-variable, @lsp-declare-var, @lsp-variable, @lsp-var
        for directive in [
            "@lsp-declare-variable",
            "@lsp-declare-var",
            "@lsp-variable",
            "@lsp-var",
        ] {
            let content = format!("# {} myvar", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.declared_variables.len(), 1, "Failed for {}", directive);
            assert_eq!(
                meta.declared_variables[0].name, "myvar",
                "Failed for {}",
                directive
            );
            assert!(
                !meta.declared_variables[0].is_function,
                "Should be variable for {}",
                directive
            );
        }
    }

    #[test]
    fn test_declare_var_line_number_recorded() {
        let content = "x <- 1\ny <- 2\n# @lsp-var myvar\nz <- 3";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].line, 2); // 0-based line number
    }

    #[test]
    fn test_declare_var_multiple() {
        let content = "# @lsp-var var1\n# @lsp-var var2\n# @lsp-var var3";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 3);
        assert_eq!(meta.declared_variables[0].name, "var1");
        assert_eq!(meta.declared_variables[0].line, 0);
        assert_eq!(meta.declared_variables[1].name, "var2");
        assert_eq!(meta.declared_variables[1].line, 1);
        assert_eq!(meta.declared_variables[2].name, "var3");
        assert_eq!(meta.declared_variables[2].line, 2);
    }

    #[test]
    fn test_declare_var_with_special_chars_quoted() {
        // R allows special characters in symbol names when quoted
        let content = r#"# @lsp-var "my.special_var""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "my.special_var");
    }

    #[test]
    fn test_declare_var_no_at_prefix_not_recognized() {
        // Requirement 1.6: Directives without @ prefix should NOT be recognized
        let content = "# lsp-var myvar";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 0);
    }

    // Function declaration directive tests
    #[test]
    fn test_declare_func_basic() {
        let content = "# @lsp-func myfunc";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "myfunc");
        assert_eq!(meta.declared_functions[0].line, 0);
        assert!(meta.declared_functions[0].is_function);
    }

    #[test]
    fn test_declare_func_with_colon() {
        let content = "# @lsp-func: myfunc";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "myfunc");
    }

    #[test]
    fn test_declare_func_double_quoted() {
        let content = r#"# @lsp-func "my.func""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my.func");
    }

    #[test]
    fn test_declare_func_single_quoted() {
        let content = "# @lsp-func 'my.func'";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my.func");
    }

    #[test]
    fn test_declare_func_all_synonyms() {
        // Test all 4 synonym forms: @lsp-declare-function, @lsp-declare-func, @lsp-function, @lsp-func
        for directive in [
            "@lsp-declare-function",
            "@lsp-declare-func",
            "@lsp-function",
            "@lsp-func",
        ] {
            let content = format!("# {} myfunc", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.declared_functions.len(), 1, "Failed for {}", directive);
            assert_eq!(
                meta.declared_functions[0].name, "myfunc",
                "Failed for {}",
                directive
            );
            assert!(
                meta.declared_functions[0].is_function,
                "Should be function for {}",
                directive
            );
        }
    }

    #[test]
    fn test_declare_func_line_number_recorded() {
        let content = "x <- 1\ny <- 2\n# @lsp-func myfunc\nz <- 3";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].line, 2); // 0-based line number
    }

    #[test]
    fn test_declare_func_multiple() {
        let content = "# @lsp-func func1\n# @lsp-func func2\n# @lsp-func func3";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 3);
        assert_eq!(meta.declared_functions[0].name, "func1");
        assert_eq!(meta.declared_functions[0].line, 0);
        assert_eq!(meta.declared_functions[1].name, "func2");
        assert_eq!(meta.declared_functions[1].line, 1);
        assert_eq!(meta.declared_functions[2].name, "func3");
        assert_eq!(meta.declared_functions[2].line, 2);
    }

    #[test]
    fn test_declare_func_with_special_chars_quoted() {
        // R allows special characters in symbol names when quoted
        let content = r#"# @lsp-func "my.special_func""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my.special_func");
    }

    #[test]
    fn test_declare_func_no_at_prefix_not_recognized() {
        // Requirement 2.6: Directives without @ prefix should NOT be recognized
        let content = "# lsp-func myfunc";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 0);
    }

    // Mixed declaration tests
    #[test]
    fn test_declare_mixed_vars_and_funcs() {
        let content = "# @lsp-var myvar\n# @lsp-func myfunc\n# @lsp-variable another_var\n# @lsp-function another_func";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 2);
        assert_eq!(meta.declared_functions.len(), 2);
        assert_eq!(meta.declared_variables[0].name, "myvar");
        assert_eq!(meta.declared_variables[1].name, "another_var");
        assert_eq!(meta.declared_functions[0].name, "myfunc");
        assert_eq!(meta.declared_functions[1].name, "another_func");
    }

    #[test]
    fn test_declare_with_other_directives() {
        let content = r#"# @lsp-sourced-by ../main.R
# @lsp-var myvar
# @lsp-cd /data
# @lsp-func myfunc
# @lsp-ignore"#;
        let meta = parse_directives(content);
        assert_eq!(meta.sourced_by.len(), 1);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.working_directory, Some("/data".to_string()));
        assert!(meta.ignored_lines.contains(&4));
    }

    // Edge cases
    #[test]
    fn test_declare_var_empty_name_skipped() {
        // Empty symbol names should be skipped
        let content = "# @lsp-var ";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 0);
    }

    #[test]
    fn test_declare_func_empty_name_skipped() {
        // Empty symbol names should be skipped
        let content = "# @lsp-func ";
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 0);
    }

    #[test]
    fn test_declare_var_whitespace_only_quoted_skipped() {
        // Whitespace-only quoted names should be skipped
        let content = r#"# @lsp-var "   ""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 0);
    }

    #[test]
    fn test_declare_func_whitespace_only_quoted_skipped() {
        // Whitespace-only quoted names should be skipped
        let content = r#"# @lsp-func "   ""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 0);
    }

    #[test]
    fn test_declare_var_colon_and_quotes() {
        let content = r#"# @lsp-var: "my.var""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_variables.len(), 1);
        assert_eq!(meta.declared_variables[0].name, "my.var");
    }

    #[test]
    fn test_declare_func_colon_and_quotes() {
        let content = r#"# @lsp-func: "my.func""#;
        let meta = parse_directives(content);
        assert_eq!(meta.declared_functions.len(), 1);
        assert_eq!(meta.declared_functions[0].name, "my.func");
    }
}
