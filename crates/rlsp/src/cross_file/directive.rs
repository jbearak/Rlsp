//
// cross_file/directive.rs
//
// Directive parsing for cross-file awareness
//

use regex::Regex;
use std::sync::OnceLock;

use super::types::{BackwardDirective, CallSiteSpec, CrossFileMetadata, ForwardSource};

/// Compiled regex patterns for directive parsing
struct DirectivePatterns {
    backward: Regex,
    forward: Regex,
    working_dir: Regex,
    ignore: Regex,
    ignore_next: Regex,
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

fn patterns() -> &'static DirectivePatterns {
    static PATTERNS: OnceLock<DirectivePatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // Path pattern: "quoted with spaces" or 'single quoted' or unquoted
        // Groups: 1=double-quoted, 2=single-quoted, 3=unquoted
        DirectivePatterns {
            backward: Regex::new(
                r#"#\s*@lsp-(?:sourced-by|run-by|included-by)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))(?:\s+line\s*=\s*(\d+))?(?:\s+match\s*=\s*["']([^"']+)["'])?"#
            ).unwrap(),
            forward: Regex::new(
                r#"#\s*@lsp-source\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))"#
            ).unwrap(),
            working_dir: Regex::new(
                r#"#\s*@lsp-(?:working-directory|working-dir|current-directory|current-dir|cd|wd)\s*:?\s*(?:"([^"]+)"|'([^']+)'|(\S+))"#
            ).unwrap(),
            ignore: Regex::new(
                r"#\s*@lsp-ignore\s*:?\s*$"
            ).unwrap(),
            ignore_next: Regex::new(
                r"#\s*@lsp-ignore-next\s*:?\s*$"
            ).unwrap(),
        }
    })
}

/// Parse directives from file content
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
            log::trace!(
                "  Parsed forward directive at line {}: path='{}'",
                line_num,
                path
            );
            meta.sources.push(ForwardSource {
                path,
                line: line_num,
                column: 0,
                is_directive: true,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
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
        }
    }

    log::trace!(
        "Completed directive parsing: {} backward directives, {} forward directives, working_dir={:?}, {} ignored lines",
        meta.sourced_by.len(),
        meta.sources.len(),
        meta.working_directory,
        meta.ignored_lines.len() + meta.ignored_next_lines.len()
    );

    meta
}

/// Check if a line should have diagnostics suppressed
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
        assert_eq!(meta.sourced_by[0].call_site, CallSiteSpec::Match("source(".to_string()));
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
        for directive in ["@lsp-wd", "@lsp-cd", "@lsp-current-directory", "@lsp-current-dir", "@lsp-working-dir"] {
            let content = format!("# {} /data", directive);
            let meta = parse_directives(&content);
            assert_eq!(meta.working_directory, Some("/data".to_string()), "Failed for {}", directive);
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
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    /// Strategy for generating valid path characters (no quotes)
    fn path_char_strategy() -> impl Strategy<Value = char> {
        prop_oneof![
            Just('a'),
            Just('z'),
            Just('A'),
            Just('Z'),
            Just('0'),
            Just('9'),
            Just('_'),
            Just('-'),
            Just('.'),
            Just('/'),
            Just(' '),
        ]
    }

    /// Strategy for generating paths with spaces
    fn path_with_spaces_strategy() -> impl Strategy<Value = String> {
        prop::collection::vec(path_char_strategy(), 1..30)
            .prop_map(|chars| chars.into_iter().collect::<String>())
            .prop_filter("must contain space", |s| s.contains(' '))
            .prop_filter("must not be only spaces", |s| s.trim().len() > 0)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Property 2: Quoted path extraction preserves spaces
        /// For any path with spaces, parsing a double-quoted directive should preserve the path.
        #[test]
        fn prop_double_quoted_path_preserves_spaces(path in path_with_spaces_strategy()) {
            let content = format!(r#"# @lsp-sourced-by "{}""#, path);
            let meta = parse_directives(&content);
            prop_assert_eq!(meta.sourced_by.len(), 1);
            prop_assert_eq!(&meta.sourced_by[0].path, &path);
        }

        /// Property 2: Single-quoted path extraction preserves spaces
        #[test]
        fn prop_single_quoted_path_preserves_spaces(path in path_with_spaces_strategy()) {
            let content = format!("# @lsp-sourced-by '{}'", path);
            let meta = parse_directives(&content);
            prop_assert_eq!(meta.sourced_by.len(), 1);
            prop_assert_eq!(&meta.sourced_by[0].path, &path);
        }

        /// Property 2: Forward directive quoted path preserves spaces
        #[test]
        fn prop_forward_quoted_path_preserves_spaces(path in path_with_spaces_strategy()) {
            let content = format!(r#"# @lsp-source "{}""#, path);
            let meta = parse_directives(&content);
            prop_assert_eq!(meta.sources.len(), 1);
            prop_assert_eq!(&meta.sources[0].path, &path);
        }

        /// Property 2: Working directory quoted path preserves spaces
        #[test]
        fn prop_working_dir_quoted_path_preserves_spaces(path in path_with_spaces_strategy()) {
            let content = format!(r#"# @lsp-cd "{}""#, path);
            let meta = parse_directives(&content);
            prop_assert_eq!(meta.working_directory, Some(path));
        }
    }
}