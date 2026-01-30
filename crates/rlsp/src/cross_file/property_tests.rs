//
// cross_file/property_tests.rs
//
// Property-based tests for cross-file awareness
//

#![cfg(test)]

use proptest::prelude::*;
use std::path::PathBuf;

use super::directive::parse_directives;
use super::path_resolve::{resolve_working_directory, PathContext};
use super::types::{CallSiteSpec, CrossFileMetadata};

// ============================================================================
// Generators for valid R file paths
// ============================================================================

/// Generate a valid R file path component (no special chars that break parsing)
fn path_component() -> impl Strategy<Value = String> {
    "[a-zA-Z][a-zA-Z0-9_]{0,10}"
}

/// Generate a valid relative R file path
fn relative_path() -> impl Strategy<Value = String> {
    prop::collection::vec(path_component(), 1..=3)
        .prop_map(|parts| format!("{}.R", parts.join("/")))
}

/// Generate a valid relative path with optional parent directory navigation
fn relative_path_with_parents() -> impl Strategy<Value = String> {
    (0..3usize, relative_path()).prop_map(|(parents, path)| {
        let prefix = "../".repeat(parents);
        format!("{}{}", prefix, path)
    })
}

// ============================================================================
// Property 1: Backward Directive Synonym Equivalence
// Validates: Requirements 1.1, 1.2, 1.3
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 1: For any valid path string, parsing @lsp-sourced-by, @lsp-run-by,
    /// and @lsp-included-by SHALL produce equivalent BackwardDirective structures.
    #[test]
    fn prop_backward_directive_synonym_equivalence(path in relative_path_with_parents()) {
        let sourced_by = format!("# @lsp-sourced-by {}", path);
        let run_by = format!("# @lsp-run-by {}", path);
        let included_by = format!("# @lsp-included-by {}", path);

        let meta1 = parse_directives(&sourced_by);
        let meta2 = parse_directives(&run_by);
        let meta3 = parse_directives(&included_by);

        // All should produce exactly one backward directive
        prop_assert_eq!(meta1.sourced_by.len(), 1);
        prop_assert_eq!(meta2.sourced_by.len(), 1);
        prop_assert_eq!(meta3.sourced_by.len(), 1);

        // All should have the same path
        prop_assert_eq!(&meta1.sourced_by[0].path, &path);
        prop_assert_eq!(&meta2.sourced_by[0].path, &path);
        prop_assert_eq!(&meta3.sourced_by[0].path, &path);

        // All should have the same call site (Default)
        prop_assert_eq!(&meta1.sourced_by[0].call_site, &CallSiteSpec::Default);
        prop_assert_eq!(&meta2.sourced_by[0].call_site, &CallSiteSpec::Default);
        prop_assert_eq!(&meta3.sourced_by[0].call_site, &CallSiteSpec::Default);
    }

    /// Property 1 extended: Synonyms with colon should also be equivalent
    #[test]
    fn prop_backward_directive_synonym_with_colon(path in relative_path_with_parents()) {
        let sourced_by = format!("# @lsp-sourced-by: {}", path);
        let run_by = format!("# @lsp-run-by: {}", path);
        let included_by = format!("# @lsp-included-by: {}", path);

        let meta1 = parse_directives(&sourced_by);
        let meta2 = parse_directives(&run_by);
        let meta3 = parse_directives(&included_by);

        prop_assert_eq!(meta1.sourced_by.len(), 1);
        prop_assert_eq!(meta2.sourced_by.len(), 1);
        prop_assert_eq!(meta3.sourced_by.len(), 1);

        prop_assert_eq!(&meta1.sourced_by[0].path, &path);
        prop_assert_eq!(&meta2.sourced_by[0].path, &path);
        prop_assert_eq!(&meta3.sourced_by[0].path, &path);
    }

    /// Property 1 extended: Synonyms with quotes should also be equivalent
    #[test]
    fn prop_backward_directive_synonym_with_quotes(path in relative_path_with_parents()) {
        let sourced_by = format!("# @lsp-sourced-by \"{}\"", path);
        let run_by = format!("# @lsp-run-by \"{}\"", path);
        let included_by = format!("# @lsp-included-by \"{}\"", path);

        let meta1 = parse_directives(&sourced_by);
        let meta2 = parse_directives(&run_by);
        let meta3 = parse_directives(&included_by);

        prop_assert_eq!(meta1.sourced_by.len(), 1);
        prop_assert_eq!(meta2.sourced_by.len(), 1);
        prop_assert_eq!(meta3.sourced_by.len(), 1);

        prop_assert_eq!(&meta1.sourced_by[0].path, &path);
        prop_assert_eq!(&meta2.sourced_by[0].path, &path);
        prop_assert_eq!(&meta3.sourced_by[0].path, &path);
    }
}

// ============================================================================
// Property 2: Working Directory Synonym Equivalence
// Validates: Requirements 3.1-3.6
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 2: For any valid path string, all working directory directive synonyms
    /// SHALL produce equivalent working directory configurations.
    #[test]
    fn prop_working_directory_synonym_equivalence(path in relative_path_with_parents()) {
        let synonyms = [
            "@lsp-working-directory",
            "@lsp-working-dir",
            "@lsp-current-directory",
            "@lsp-current-dir",
            "@lsp-cd",
            "@lsp-wd",
        ];

        let results: Vec<_> = synonyms
            .iter()
            .map(|syn| {
                let content = format!("# {} {}", syn, path);
                parse_directives(&content)
            })
            .collect();

        // All should produce the same working directory
        for (i, meta) in results.iter().enumerate() {
            prop_assert_eq!(
                meta.working_directory.as_ref(),
                Some(&path),
                "Synonym {} failed", synonyms[i]
            );
        }
    }

    /// Property 2 extended: Working directory synonyms with colon
    #[test]
    fn prop_working_directory_synonym_with_colon(path in relative_path_with_parents()) {
        let synonyms = [
            "@lsp-working-directory:",
            "@lsp-working-dir:",
            "@lsp-current-directory:",
            "@lsp-current-dir:",
            "@lsp-cd:",
            "@lsp-wd:",
        ];

        let results: Vec<_> = synonyms
            .iter()
            .map(|syn| {
                let content = format!("# {} {}", syn, path);
                parse_directives(&content)
            })
            .collect();

        for (i, meta) in results.iter().enumerate() {
            prop_assert_eq!(
                meta.working_directory.as_ref(),
                Some(&path),
                "Synonym {} failed", synonyms[i]
            );
        }
    }

    /// Property 2 extended: Working directory synonyms with quotes
    #[test]
    fn prop_working_directory_synonym_with_quotes(path in relative_path_with_parents()) {
        let synonyms = [
            "@lsp-working-directory",
            "@lsp-wd",
            "@lsp-cd",
        ];

        for syn in synonyms {
            let double_quoted = format!("# {} \"{}\"", syn, path);
            let single_quoted = format!("# {} '{}'", syn, path);

            let meta1 = parse_directives(&double_quoted);
            let meta2 = parse_directives(&single_quoted);

            prop_assert_eq!(meta1.working_directory.as_ref(), Some(&path));
            prop_assert_eq!(meta2.working_directory.as_ref(), Some(&path));
        }
    }
}

// ============================================================================
// Property 2a: Workspace-Root-Relative Path Resolution
// Validates: Requirements 3.9
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 2a: Paths starting with / SHALL resolve relative to workspace root.
    #[test]
    fn prop_workspace_root_relative_path(subpath in relative_path()) {
        let workspace_root = PathBuf::from("/workspace");
        let file_path = PathBuf::from("/workspace/src/main.R");

        let ctx = PathContext {
            file_path,
            working_directory: None,
            inherited_working_directory: None,
            workspace_root: Some(workspace_root.clone()),
        };

        let path_str = format!("/{}", subpath);
        let resolved = resolve_working_directory(&path_str, &ctx);

        prop_assert!(resolved.is_some());
        let resolved = resolved.unwrap();

        // Should start with workspace root
        prop_assert!(resolved.starts_with(&workspace_root));

        // Should NOT be filesystem root
        prop_assert!(!resolved.starts_with("/") || resolved.starts_with(&workspace_root));
    }

    /// Property 2a extended: Workspace-root-relative without workspace returns None
    #[test]
    fn prop_workspace_root_relative_no_workspace(subpath in relative_path()) {
        let ctx = PathContext {
            file_path: PathBuf::from("/some/file.R"),
            working_directory: None,
            inherited_working_directory: None,
            workspace_root: None,
        };

        let path_str = format!("/{}", subpath);
        let resolved = resolve_working_directory(&path_str, &ctx);

        prop_assert!(resolved.is_none());
    }
}

// ============================================================================
// Property 2b: File-Relative Path Resolution
// Validates: Requirements 3.10
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 2b: Paths not starting with / SHALL resolve relative to file's directory.
    #[test]
    fn prop_file_relative_path(subpath in relative_path()) {
        let file_path = PathBuf::from("/project/src/main.R");
        let file_dir = PathBuf::from("/project/src");

        let ctx = PathContext {
            file_path,
            working_directory: None,
            inherited_working_directory: None,
            workspace_root: Some(PathBuf::from("/project")),
        };

        let resolved = resolve_working_directory(&subpath, &ctx);

        prop_assert!(resolved.is_some());
        let resolved = resolved.unwrap();

        // Should start with file's directory
        prop_assert!(resolved.starts_with(&file_dir));
    }

    /// Property 2b extended: Parent directory navigation
    #[test]
    fn prop_file_relative_with_parent_nav(
        parents in 1..3usize,
        subpath in relative_path()
    ) {
        let file_path = PathBuf::from("/project/a/b/c/main.R");

        let ctx = PathContext {
            file_path,
            working_directory: None,
            inherited_working_directory: None,
            workspace_root: Some(PathBuf::from("/project")),
        };

        let prefix = "../".repeat(parents);
        let path_str = format!("{}{}", prefix, subpath);
        let resolved = resolve_working_directory(&path_str, &ctx);

        prop_assert!(resolved.is_some());
        let resolved = resolved.unwrap();

        // Should still be under /project (not escape workspace)
        prop_assert!(resolved.starts_with("/project"));
    }
}

// ============================================================================
// Property 8: Directive Serialization Round-Trip
// Validates: Requirements 14.1-14.4
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 8: Parsing then serializing then parsing SHALL produce equivalent structures.
    #[test]
    fn prop_directive_round_trip(path in relative_path_with_parents()) {
        let content = format!("# @lsp-sourced-by {}", path);
        let meta1 = parse_directives(&content);

        // Serialize to JSON
        let json = serde_json::to_string(&meta1).unwrap();

        // Deserialize back
        let meta2: CrossFileMetadata = serde_json::from_str(&json).unwrap();

        // Should be equivalent
        prop_assert_eq!(meta1.sourced_by.len(), meta2.sourced_by.len());
        if !meta1.sourced_by.is_empty() {
            prop_assert_eq!(&meta1.sourced_by[0].path, &meta2.sourced_by[0].path);
            prop_assert_eq!(&meta1.sourced_by[0].call_site, &meta2.sourced_by[0].call_site);
        }
    }

    /// Property 8 extended: Round-trip with all directive types
    #[test]
    fn prop_full_metadata_round_trip(
        backward_path in relative_path_with_parents(),
        forward_path in relative_path(),
        wd_path in relative_path(),
    ) {
        let content = format!(
            "# @lsp-sourced-by {}\n# @lsp-source {}\n# @lsp-working-directory {}\n# @lsp-ignore\n# @lsp-ignore-next",
            backward_path, forward_path, wd_path
        );
        let meta1 = parse_directives(&content);

        let json = serde_json::to_string(&meta1).unwrap();
        let meta2: CrossFileMetadata = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(meta1.sourced_by.len(), meta2.sourced_by.len());
        prop_assert_eq!(meta1.sources.len(), meta2.sources.len());
        prop_assert_eq!(meta1.working_directory, meta2.working_directory);
        prop_assert_eq!(meta1.ignored_lines, meta2.ignored_lines);
        prop_assert_eq!(meta1.ignored_next_lines, meta2.ignored_next_lines);
    }
}

// ============================================================================
// Property 9: Call Site Line Parameter Extraction
// Validates: Requirements 1.6
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 9: For any backward directive with line=N, the parsed CallSiteSpec
    /// SHALL be Line(N-1) (converted to 0-based).
    #[test]
    fn prop_call_site_line_extraction(
        path in relative_path_with_parents(),
        line in 1..1000u32
    ) {
        let content = format!("# @lsp-sourced-by {} line={}", path, line);
        let meta = parse_directives(&content);

        prop_assert_eq!(meta.sourced_by.len(), 1);
        prop_assert_eq!(
            &meta.sourced_by[0].call_site,
            &CallSiteSpec::Line(line - 1) // 0-based
        );
    }

    /// Property 9 extended: Line=1 should become Line(0)
    #[test]
    fn prop_call_site_line_one_based_to_zero_based(path in relative_path_with_parents()) {
        let content = format!("# @lsp-sourced-by {} line=1", path);
        let meta = parse_directives(&content);

        prop_assert_eq!(meta.sourced_by.len(), 1);
        prop_assert_eq!(&meta.sourced_by[0].call_site, &CallSiteSpec::Line(0));
    }

    /// Property 9 extended: Line extraction with different directive synonyms
    #[test]
    fn prop_call_site_line_with_synonyms(
        path in relative_path_with_parents(),
        line in 1..100u32
    ) {
        let synonyms = ["@lsp-sourced-by", "@lsp-run-by", "@lsp-included-by"];

        for syn in synonyms {
            let content = format!("# {} {} line={}", syn, path, line);
            let meta = parse_directives(&content);

            prop_assert_eq!(meta.sourced_by.len(), 1);
            prop_assert_eq!(
                &meta.sourced_by[0].call_site,
                &CallSiteSpec::Line(line - 1)
            );
        }
    }
}

// ============================================================================
// Property 10: Call Site Match Parameter Extraction
// Validates: Requirements 1.7
// ============================================================================

/// Generate a valid match pattern (no quotes inside)
fn match_pattern() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_()., ]{1,20}"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 10: For any backward directive with match="pattern", the parsed
    /// CallSiteSpec SHALL be Match(pattern) with the exact pattern string.
    #[test]
    fn prop_call_site_match_extraction(
        path in relative_path_with_parents(),
        pattern in match_pattern()
    ) {
        let content = format!("# @lsp-sourced-by {} match=\"{}\"", path, pattern);
        let meta = parse_directives(&content);

        prop_assert_eq!(meta.sourced_by.len(), 1);
        prop_assert_eq!(
            &meta.sourced_by[0].call_site,
            &CallSiteSpec::Match(pattern)
        );
    }

    /// Property 10 extended: Match with single quotes
    #[test]
    fn prop_call_site_match_single_quotes(
        path in relative_path_with_parents(),
        pattern in match_pattern()
    ) {
        let content = format!("# @lsp-sourced-by {} match='{}'", path, pattern);
        let meta = parse_directives(&content);

        prop_assert_eq!(meta.sourced_by.len(), 1);
        prop_assert_eq!(
            &meta.sourced_by[0].call_site,
            &CallSiteSpec::Match(pattern)
        );
    }

    /// Property 10 extended: Match extraction with different directive synonyms
    #[test]
    fn prop_call_site_match_with_synonyms(
        path in relative_path_with_parents(),
        pattern in match_pattern()
    ) {
        let synonyms = ["@lsp-sourced-by", "@lsp-run-by", "@lsp-included-by"];

        for syn in synonyms {
            let content = format!("# {} {} match=\"{}\"", syn, path, pattern);
            let meta = parse_directives(&content);

            prop_assert_eq!(meta.sourced_by.len(), 1);
            prop_assert_eq!(
                &meta.sourced_by[0].call_site,
                &CallSiteSpec::Match(pattern.clone())
            );
        }
    }
}
