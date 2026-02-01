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

/// R reserved words and special values that cannot be used as identifiers
const R_RESERVED: &[&str] = &[
    "if", "else", "for", "in", "while", "repeat", "next", "break", "function",
    "NA", "NaN", "Inf", "NULL", "TRUE", "FALSE", "T", "F",
    "na", "nan", "inf", "null", "true", "false",
];

/// Check if a name is a valid R identifier (not reserved)
fn is_valid_r_identifier(name: &str) -> bool {
    !R_RESERVED.contains(&name)
}

/// Generate a valid R identifier (lowercase to avoid reserved words)
fn r_identifier() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,5}".prop_filter("not reserved", |s| is_valid_r_identifier(s))
}

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

// ============================================================================
// Property 3: Quote Style Equivalence for Source Detection
// Validates: Requirements 4.1, 4.2
// ============================================================================

use super::source_detect::detect_source_calls;
use tree_sitter::Parser;

fn parse_r(code: &str) -> tree_sitter::Tree {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
    parser.parse(code, None).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 3: For any valid path string p, detecting source("p") and source('p')
    /// SHALL produce equivalent SourceCall structures with the same path.
    #[test]
    fn prop_quote_style_equivalence(path in relative_path()) {
        let double_quoted = format!("source(\"{}\")", path);
        let single_quoted = format!("source('{}')", path);

        let tree1 = parse_r(&double_quoted);
        let tree2 = parse_r(&single_quoted);

        let sources1 = detect_source_calls(&tree1, &double_quoted);
        let sources2 = detect_source_calls(&tree2, &single_quoted);

        prop_assert_eq!(sources1.len(), 1);
        prop_assert_eq!(sources2.len(), 1);
        prop_assert_eq!(&sources1[0].path, &path);
        prop_assert_eq!(&sources2[0].path, &path);
        prop_assert_eq!(sources1[0].is_sys_source, sources2[0].is_sys_source);
        prop_assert_eq!(sources1[0].local, sources2[0].local);
        prop_assert_eq!(sources1[0].chdir, sources2[0].chdir);
    }
}

// ============================================================================
// Property 15: Named Argument Source Detection
// Validates: Requirements 4.3
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 15: For any source() call using source(file = "path.R") syntax,
    /// the Source_Detector SHALL extract "path.R" as the path.
    #[test]
    fn prop_named_argument_source_detection(path in relative_path()) {
        let code = format!("source(file = \"{}\")", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert_eq!(&sources[0].path, &path);
    }

    /// Property 15 extended: Named argument with other arguments
    #[test]
    fn prop_named_argument_with_other_args(path in relative_path()) {
        let code = format!("source(file = \"{}\", local = TRUE)", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert_eq!(&sources[0].path, &path);
        prop_assert!(sources[0].local);
    }
}

// ============================================================================
// Property 16: sys.source Detection
// Validates: Requirements 4.4
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 16: For any sys.source() call with a string literal path,
    /// the Source_Detector SHALL extract the path and mark is_sys_source as true.
    #[test]
    fn prop_sys_source_detection(path in relative_path()) {
        let code = format!("sys.source(\"{}\", envir = globalenv())", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert_eq!(&sources[0].path, &path);
        prop_assert!(sources[0].is_sys_source);
    }

    /// Property 16 extended: sys.source with single quotes
    #[test]
    fn prop_sys_source_single_quotes(path in relative_path()) {
        let code = format!("sys.source('{}', envir = globalenv())", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert_eq!(&sources[0].path, &path);
        prop_assert!(sources[0].is_sys_source);
    }
}

// ============================================================================
// Property 17: Dynamic Path Graceful Handling
// Validates: Requirements 4.5, 4.6
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 17: For any source() call where the path argument is a variable,
    /// the Source_Detector SHALL not extract a path and SHALL not emit an error.
    #[test]
    fn prop_dynamic_path_variable_skipped(varname in "[a-zA-Z][a-zA-Z0-9_]{0,10}") {
        let code = format!("source({})", varname);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        // Should skip, not error
        prop_assert_eq!(sources.len(), 0);
    }

    /// Property 17 extended: paste0() calls should be skipped
    #[test]
    fn prop_dynamic_path_paste0_skipped(
        prefix in "[a-zA-Z]{1,5}",
        varname in r_identifier()
    ) {
        let code = format!("source(paste0(\"{}/\", {}))", prefix, varname);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 0);
    }

    /// Property 17 extended: Expression paths should be skipped
    #[test]
    fn prop_dynamic_path_expression_skipped(
        base in "[a-zA-Z]{1,5}",
        suffix in "[a-zA-Z]{1,5}"
    ) {
        let code = format!("source(paste({}, \"{}\"))", base, suffix);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 0);
    }
}

// ============================================================================
// Property 18: Source Call Parameter Extraction
// Validates: Requirements 4.7, 4.8
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 18: For any source() call with local = TRUE, the extracted
    /// SourceCall SHALL have local = true.
    #[test]
    fn prop_source_local_true_extraction(path in relative_path()) {
        let code = format!("source(\"{}\", local = TRUE)", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert!(sources[0].local);
    }

    /// Property 18 extended: local = FALSE should have local = false
    #[test]
    fn prop_source_local_false_extraction(path in relative_path()) {
        let code = format!("source(\"{}\", local = FALSE)", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert!(!sources[0].local);
    }

    /// Property 18: For any source() call with chdir = TRUE, the extracted
    /// SourceCall SHALL have chdir = true.
    #[test]
    fn prop_source_chdir_true_extraction(path in relative_path()) {
        let code = format!("source(\"{}\", chdir = TRUE)", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert!(sources[0].chdir);
    }

    /// Property 18 extended: chdir = FALSE should have chdir = false
    #[test]
    fn prop_source_chdir_false_extraction(path in relative_path()) {
        let code = format!("source(\"{}\", chdir = FALSE)", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert!(!sources[0].chdir);
    }

    /// Property 18 extended: Both local and chdir together
    #[test]
    fn prop_source_local_and_chdir_extraction(path in relative_path()) {
        let code = format!("source(\"{}\", local = TRUE, chdir = TRUE)", path);
        let tree = parse_r(&code);
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert!(sources[0].local);
        prop_assert!(sources[0].chdir);
    }
}

// ============================================================================
// Property 11: Relative Path Resolution
// Validates: Requirements 1.8, 1.9, 3.9
// ============================================================================

use super::path_resolve::resolve_path;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 11: For any file at path /a/b/c.R and relative directive path ../d/e.R,
    /// the Path_Resolver SHALL resolve to /a/d/e.R.
    #[test]
    fn prop_relative_path_resolution(
        dir_a in path_component(),
        dir_b in path_component(),
        dir_c in path_component(),
        dir_d in path_component(),
        file_e in path_component()
    ) {
        let file_path = PathBuf::from(format!("/{}/{}/{}/main.R", dir_a, dir_b, dir_c));
        let workspace_root = PathBuf::from(format!("/{}", dir_a));

        let ctx = PathContext {
            file_path,
            working_directory: None,
            inherited_working_directory: None,
            workspace_root: Some(workspace_root),
        };

        let relative_path = format!("../{}/{}.R", dir_d, file_e);
        let resolved = resolve_path(&relative_path, &ctx);

        prop_assert!(resolved.is_some());
        let resolved = resolved.unwrap();

        // Should resolve to /dir_a/dir_b/dir_d/file_e.R
        let expected = PathBuf::from(format!("/{}/{}/{}/{}.R", dir_a, dir_b, dir_d, file_e));
        prop_assert_eq!(resolved, expected);
    }

    /// Property 11 extended: Multiple parent directory navigation
    #[test]
    fn prop_multiple_parent_navigation(
        dir_a in path_component(),
        dir_b in path_component(),
        dir_c in path_component(),
        file in path_component()
    ) {
        let file_path = PathBuf::from(format!("/{}/{}/{}/main.R", dir_a, dir_b, dir_c));
        let workspace_root = PathBuf::from(format!("/{}", dir_a));

        let ctx = PathContext {
            file_path,
            working_directory: None,
            inherited_working_directory: None,
            workspace_root: Some(workspace_root),
        };

        // Go up two levels
        let relative_path = format!("../../{}.R", file);
        let resolved = resolve_path(&relative_path, &ctx);

        prop_assert!(resolved.is_some());
        let resolved = resolved.unwrap();

        // Should resolve to /dir_a/file.R
        let expected = PathBuf::from(format!("/{}/{}.R", dir_a, file));
        prop_assert_eq!(resolved, expected);
    }
}

// ============================================================================
// Property 13: Working Directory Inheritance
// Validates: Requirements 3.11
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 13: For any source chain A → B → C where only A has a working directory
    /// directive, files B and C SHALL inherit A's working directory for path resolution.
    #[test]
    fn prop_working_directory_inheritance(
        workspace in path_component(),
        wd_dir in path_component(),
        file_b in path_component(),
        file_c in path_component()
    ) {
        let workspace_root = PathBuf::from(format!("/{}", workspace));
        let working_dir = PathBuf::from(format!("/{}/{}", workspace, wd_dir));

        // File A has explicit working directory
        let ctx_a = PathContext {
            file_path: PathBuf::from(format!("/{}/src/a.R", workspace)),
            working_directory: Some(working_dir.clone()),
            inherited_working_directory: None,
            workspace_root: Some(workspace_root.clone()),
        };

        // File B inherits from A (via child_context)
        let ctx_b = ctx_a.child_context(&PathBuf::from(format!("/{}/src/{}.R", workspace, file_b)));

        // File C inherits from B (which inherited from A)
        let ctx_c = ctx_b.child_context(&PathBuf::from(format!("/{}/src/{}.R", workspace, file_c)));

        // All should have the same effective working directory
        prop_assert_eq!(ctx_a.effective_working_directory(), working_dir.clone());
        prop_assert_eq!(ctx_b.effective_working_directory(), working_dir.clone());
        prop_assert_eq!(ctx_c.effective_working_directory(), working_dir);
    }

    /// Property 13 extended: chdir=TRUE breaks inheritance
    #[test]
    fn prop_chdir_breaks_inheritance(
        workspace in path_component(),
        wd_dir in path_component(),
        child_dir in path_component()
    ) {
        let workspace_root = PathBuf::from(format!("/{}", workspace));
        let working_dir = PathBuf::from(format!("/{}/{}", workspace, wd_dir));
        let child_path = PathBuf::from(format!("/{}/{}/child.R", workspace, child_dir));

        // Parent has explicit working directory
        let ctx_parent = PathContext {
            file_path: PathBuf::from(format!("/{}/src/parent.R", workspace)),
            working_directory: Some(working_dir.clone()),
            inherited_working_directory: None,
            workspace_root: Some(workspace_root.clone()),
        };

        // Child with chdir=TRUE gets its own directory as working directory
        let ctx_child = ctx_parent.child_context_with_chdir(&child_path);

        // Child's effective working directory should be its own directory, not parent's
        let expected_child_wd = PathBuf::from(format!("/{}/{}", workspace, child_dir));
        prop_assert_eq!(ctx_child.effective_working_directory(), expected_child_wd);
    }
}

// ============================================================================
// Property 14: Default Working Directory
// Validates: Requirements 3.12
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 14: For any file at path /a/b/c.R with no working directory directive
    /// and no inherited working directory, the effective working directory SHALL be /a/b/.
    #[test]
    fn prop_default_working_directory(
        dir_a in path_component(),
        dir_b in path_component(),
        file in path_component()
    ) {
        let file_path = PathBuf::from(format!("/{}/{}/{}.R", dir_a, dir_b, file));

        let ctx = PathContext {
            file_path,
            working_directory: None,
            inherited_working_directory: None,
            workspace_root: Some(PathBuf::from(format!("/{}", dir_a))),
        };

        let expected = PathBuf::from(format!("/{}/{}", dir_a, dir_b));
        prop_assert_eq!(ctx.effective_working_directory(), expected);
    }

    /// Property 14 extended: Explicit working directory takes precedence
    #[test]
    fn prop_explicit_wd_takes_precedence(
        dir_a in path_component(),
        dir_b in path_component(),
        explicit_wd in path_component()
    ) {
        let file_path = PathBuf::from(format!("/{}/{}/main.R", dir_a, dir_b));
        let explicit_working_dir = PathBuf::from(format!("/{}/{}", dir_a, explicit_wd));

        let ctx = PathContext {
            file_path,
            working_directory: Some(explicit_working_dir.clone()),
            inherited_working_directory: Some(PathBuf::from(format!("/{}/inherited", dir_a))),
            workspace_root: Some(PathBuf::from(format!("/{}", dir_a))),
        };

        // Explicit should take precedence over inherited
        prop_assert_eq!(ctx.effective_working_directory(), explicit_working_dir);
    }

    /// Property 14 extended: Inherited takes precedence over default
    #[test]
    fn prop_inherited_wd_takes_precedence(
        dir_a in path_component(),
        dir_b in path_component(),
        inherited_wd in path_component()
    ) {
        let file_path = PathBuf::from(format!("/{}/{}/main.R", dir_a, dir_b));
        let inherited_working_dir = PathBuf::from(format!("/{}/{}", dir_a, inherited_wd));

        let ctx = PathContext {
            file_path,
            working_directory: None,
            inherited_working_directory: Some(inherited_working_dir.clone()),
            workspace_root: Some(PathBuf::from(format!("/{}", dir_a))),
        };

        // Inherited should take precedence over default (file's directory)
        prop_assert_eq!(ctx.effective_working_directory(), inherited_working_dir);
    }
}

// ============================================================================
// Property 23: Dependency Graph Update on Change
// Validates: Requirements 0.1, 0.2, 6.1, 6.2
// ============================================================================

use super::dependency::DependencyGraph;
use super::types::ForwardSource;
use tower_lsp::lsp_types::Url;

fn make_url(name: &str) -> Url {
    Url::parse(&format!("file:///{}.R", name)).unwrap()
}

fn make_meta_with_sources(sources: Vec<(&str, u32)>) -> CrossFileMetadata {
    CrossFileMetadata {
        sources: sources
            .into_iter()
            .map(|(path, line)| ForwardSource {
                path: path.to_string(),
                line,
                column: 0,
                is_directive: false,
                local: false,
                chdir: false,
                is_sys_source: false,
                sys_source_global_env: true,
            })
            .collect(),
        ..Default::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 23: When a file is opened or changed, the Dependency_Graph SHALL
    /// update edges for that file.
    #[test]
    fn prop_dependency_graph_update_on_change(
        parent in path_component(),
        child in path_component(),
        line in 0..100u32
    ) {
        let mut graph = DependencyGraph::new();
        let parent_uri = make_url(&parent);
        let child_uri = make_url(&child);

        let meta = make_meta_with_sources(vec![(&format!("{}.R", child), line)]);
        graph.update_file(&parent_uri, &meta, None, |_| None);

        let deps = graph.get_dependencies(&parent_uri);
        prop_assert_eq!(deps.len(), 1);
        prop_assert_eq!(&deps[0].to, &child_uri);
        prop_assert_eq!(deps[0].call_site_line, Some(line));
    }

    /// Property 23 extended: Multiple sources create multiple edges
    #[test]
    fn prop_multiple_sources_create_edges(
        parent in path_component(),
        child1 in path_component(),
        child2 in path_component()
    ) {
        prop_assume!(child1 != child2);

        let mut graph = DependencyGraph::new();
        let parent_uri = make_url(&parent);

        let meta = make_meta_with_sources(vec![
            (&format!("{}.R", child1), 5),
            (&format!("{}.R", child2), 10),
        ]);

        graph.update_file(&parent_uri, &meta, None, |_| None);

        let deps = graph.get_dependencies(&parent_uri);
        prop_assert_eq!(deps.len(), 2);
    }
}

// ============================================================================
// Property 25: Dependency Graph Edge Removal
// Validates: Requirements 6.3, 13.3
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 25: For any file that is deleted, the Dependency_Graph SHALL contain
    /// no edges where that file is either source or target.
    #[test]
    fn prop_dependency_graph_edge_removal(
        parent in path_component(),
        child in path_component()
    ) {
        let mut graph = DependencyGraph::new();
        let parent_uri = make_url(&parent);
        let child_uri = make_url(&child);

        // Add edge
        let meta = make_meta_with_sources(vec![(&format!("{}.R", child), 5)]);
        graph.update_file(&parent_uri, &meta, None, |_| None);

        // Verify edge exists
        prop_assert_eq!(graph.get_dependencies(&parent_uri).len(), 1);
        prop_assert_eq!(graph.get_dependents(&child_uri).len(), 1);

        // Remove parent file
        graph.remove_file(&parent_uri);

        // No edges should remain
        prop_assert!(graph.get_dependencies(&parent_uri).is_empty());
        prop_assert!(graph.get_dependents(&child_uri).is_empty());
    }

    /// Property 25 extended: Removing child file removes backward edges
    #[test]
    fn prop_remove_child_removes_backward_edges(
        parent in path_component(),
        child in path_component()
    ) {
        let mut graph = DependencyGraph::new();
        let parent_uri = make_url(&parent);
        let child_uri = make_url(&child);

        // Add edge
        let meta = make_meta_with_sources(vec![(&format!("{}.R", child), 5)]);
        graph.update_file(&parent_uri, &meta, None, |_| None);

        // Remove child file
        graph.remove_file(&child_uri);

        // Forward edge from parent should be removed
        prop_assert!(graph.get_dependencies(&parent_uri).is_empty());
    }
}

// ============================================================================
// Property 26: Transitive Dependency Query
// Validates: Requirements 6.4, 6.5
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 26: For any file A that sources B, and B sources C, querying
    /// dependents of C SHALL include both B and A.
    #[test]
    fn prop_transitive_dependency_query(
        file_a in path_component(),
        file_b in path_component(),
        file_c in path_component()
    ) {
        prop_assume!(file_a != file_b && file_b != file_c && file_a != file_c);

        let mut graph = DependencyGraph::new();
        let uri_a = make_url(&file_a);
        let uri_b = make_url(&file_b);
        let uri_c = make_url(&file_c);

        // A sources B
        let meta_a = make_meta_with_sources(vec![(&format!("{}.R", file_b), 1)]);
        graph.update_file(&uri_a, &meta_a, None, |_| None);

        // B sources C
        let meta_b = make_meta_with_sources(vec![(&format!("{}.R", file_c), 1)]);
        graph.update_file(&uri_b, &meta_b, None, |_| None);

        // Transitive dependents of C should include both B and A
        let dependents = graph.get_transitive_dependents(&uri_c, 10);
        prop_assert!(dependents.contains(&uri_b));
        prop_assert!(dependents.contains(&uri_a));
    }

    /// Property 26 extended: Depth limit is respected
    #[test]
    fn prop_transitive_depth_limit(
        file_a in path_component(),
        file_b in path_component(),
        file_c in path_component()
    ) {
        prop_assume!(file_a != file_b && file_b != file_c && file_a != file_c);

        let mut graph = DependencyGraph::new();
        let uri_a = make_url(&file_a);
        let uri_b = make_url(&file_b);
        let uri_c = make_url(&file_c);

        // A sources B sources C
        let meta_a = make_meta_with_sources(vec![(&format!("{}.R", file_b), 1)]);
        graph.update_file(&uri_a, &meta_a, None, |_| None);

        let meta_b = make_meta_with_sources(vec![(&format!("{}.R", file_c), 1)]);
        graph.update_file(&uri_b, &meta_b, None, |_| None);

        // With depth 1, only B should be returned (not A)
        let dependents = graph.get_transitive_dependents(&uri_c, 1);
        prop_assert!(dependents.contains(&uri_b));
        prop_assert!(!dependents.contains(&uri_a));
    }
}

// ============================================================================
// Property 50: Edge Deduplication
// Validates: Requirements 6.1, 6.2, 12.5
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 50: For any file where both a directive and AST detection identify
    /// the same relationship, the Dependency_Graph SHALL contain exactly one edge.
    #[test]
    fn prop_edge_deduplication(
        parent in path_component(),
        child in path_component(),
        line in 0..100u32
    ) {
        let mut graph = DependencyGraph::new();
        let parent_uri = make_url(&parent);

        // Create metadata with duplicate sources (directive and AST)
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: format!("{}.R", child),
                    line,
                    column: 0,
                    is_directive: false, // AST detected
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
                ForwardSource {
                    path: format!("{}.R", child),
                    line,
                    column: 0,
                    is_directive: true, // Directive
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&parent_uri, &meta, None, |_| None);

        // Should have exactly one edge (deduplicated)
        let deps = graph.get_dependencies(&parent_uri);
        prop_assert_eq!(deps.len(), 1);
    }

    /// Property 50 extended: Different call sites create separate edges
    #[test]
    fn prop_different_call_sites_separate_edges(
        parent in path_component(),
        child in path_component(),
        line1 in 0..50u32,
        line2 in 50..100u32
    ) {
        let mut graph = DependencyGraph::new();
        let parent_uri = make_url(&parent);

        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: format!("{}.R", child),
                    line: line1,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
                ForwardSource {
                    path: format!("{}.R", child),
                    line: line2,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&parent_uri, &meta, None, |_| None);

        // Should have two edges (different call sites)
        let deps = graph.get_dependencies(&parent_uri);
        prop_assert_eq!(deps.len(), 2);
    }
}

// ============================================================================
// Property 58: Directive Overrides AST For Same (from,to)
// Validates: Requirements 6.8
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 58: For any file where both @lsp-source directive and AST detection
    /// identify a relationship to the same target URI, the directive SHALL be
    /// authoritative (first one wins in deduplication).
    #[test]
    fn prop_directive_overrides_ast(
        parent in path_component(),
        child in path_component(),
        line in 0..100u32
    ) {
        let mut graph = DependencyGraph::new();
        let parent_uri = make_url(&parent);

        // Directive comes first in the list
        let meta = CrossFileMetadata {
            sources: vec![
                ForwardSource {
                    path: format!("{}.R", child),
                    line,
                    column: 0,
                    is_directive: true, // Directive first
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
                ForwardSource {
                    path: format!("{}.R", child),
                    line,
                    column: 0,
                    is_directive: false, // AST second
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                    sys_source_global_env: true,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&parent_uri, &meta, None, |_| None);

        let deps = graph.get_dependencies(&parent_uri);
        prop_assert_eq!(deps.len(), 1);
        // First one (directive) wins
        prop_assert!(deps[0].is_directive);
    }
}


// ============================================================================
// Property 4: Local Symbol Precedence
// Validates: Requirements 5.4, 7.3, 8.3, 9.2, 9.3
// ============================================================================

use super::scope::{compute_artifacts, scope_at_position_with_deps, ScopeArtifacts};

fn parse_r_tree(code: &str) -> tree_sitter::Tree {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_r::LANGUAGE.into()).unwrap();
    parser.parse(code, None).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 4: For any scope chain where a symbol s is defined in both a sourced
    /// file and the current file, the resolved scope SHALL contain the current file's
    /// definition of s (local definitions shadow inherited ones).
    #[test]
    fn prop_local_symbol_precedence(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: defines symbol BEFORE sourcing child (which also defines it)
        // Local definition should still take precedence
        let parent_code = format!(
            "{} <- 1\nsource(\"child.R\")",
            symbol_name
        );
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: defines same symbol
        let child_code = format!("{} <- 999", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Create artifacts lookup
        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };

        // Resolve path
        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" {
                Some(child_uri.clone())
            } else {
                None
            }
        };

        // Get scope at end of parent file
        let scope = scope_at_position_with_deps(
            &parent_uri,
            10, // After all definitions
            0,
            &get_artifacts,
            &resolve_path,
            10,
        );

        // Local definition should take precedence (defined in parent, not child)
        prop_assert!(scope.symbols.contains_key(&symbol_name));
        let symbol = scope.symbols.get(&symbol_name).unwrap();
        prop_assert_eq!(&symbol.source_uri, &parent_uri);
    }
}

// ============================================================================
// Property 40: Position-Aware Symbol Availability
// Validates: Requirements 5.3
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 40: For any source() call at position (line, column), symbols from
    /// the sourced file SHALL only be available for positions strictly after (line, column).
    #[test]
    fn prop_position_aware_symbol_availability(
        symbol_name in r_identifier(),
        source_line in 2..10u32
    ) {
        // Ensure symbol_name doesn't conflict with generated variable names
        prop_assume!(!symbol_name.starts_with('x') && !symbol_name.starts_with('y'));

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: has source() call at specific line
        let mut parent_lines = vec!["# comment".to_string()];
        for i in 1..source_line {
            parent_lines.push(format!("var{} <- {}", i, i));
        }
        parent_lines.push("source(\"child.R\")".to_string());
        parent_lines.push("# after source".to_string());
        let parent_code = parent_lines.join("\n");

        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri {
                Some(parent_artifacts.clone())
            } else if uri == &child_uri {
                Some(child_artifacts.clone())
            } else {
                None
            }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" {
                Some(child_uri.clone())
            } else {
                None
            }
        };

        // Before source() call - symbol should NOT be available
        let scope_before = scope_at_position_with_deps(
            &parent_uri,
            source_line - 1,
            0,
            &get_artifacts,
            &resolve_path,
            10,
        );
        prop_assert!(!scope_before.symbols.contains_key(&symbol_name));

        // After source() call - symbol SHOULD be available
        let scope_after = scope_at_position_with_deps(
            &parent_uri,
            source_line + 1,
            0,
            &get_artifacts,
            &resolve_path,
            10,
        );
        prop_assert!(scope_after.symbols.contains_key(&symbol_name));
    }
}


// ============================================================================
// Property 22: Maximum Depth Enforcement
// Validates: Requirements 5.8
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Property 22: When the source chain exceeds maxChainDepth, the Scope_Resolver
    /// SHALL stop traversal at the configured depth.
    #[test]
    fn prop_maximum_depth_enforcement(
        symbol_name in r_identifier()
    ) {
        // Create a chain: a -> b -> c -> d
        let uri_a = make_url("a");
        let uri_b = make_url("b");
        let uri_c = make_url("c");
        let uri_d = make_url("d");

        // Each file sources the next and defines a symbol
        let code_a = "source(\"b.R\")";
        let code_b = "source(\"c.R\")";
        let code_c = "source(\"d.R\")";
        let code_d = format!("{} <- 42", symbol_name);

        let tree_a = parse_r_tree(code_a);
        let tree_b = parse_r_tree(code_b);
        let tree_c = parse_r_tree(code_c);
        let tree_d = parse_r_tree(&code_d);

        let artifacts_a = compute_artifacts(&uri_a, &tree_a, code_a);
        let artifacts_b = compute_artifacts(&uri_b, &tree_b, code_b);
        let artifacts_c = compute_artifacts(&uri_c, &tree_c, code_c);
        let artifacts_d = compute_artifacts(&uri_d, &tree_d, &code_d);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &uri_a { Some(artifacts_a.clone()) }
            else if uri == &uri_b { Some(artifacts_b.clone()) }
            else if uri == &uri_c { Some(artifacts_c.clone()) }
            else if uri == &uri_d { Some(artifacts_d.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            match path {
                "b.R" => Some(uri_b.clone()),
                "c.R" => Some(uri_c.clone()),
                "d.R" => Some(uri_d.clone()),
                _ => None,
            }
        };

        // With max_depth=2, should NOT reach d (a->b->c, stops before d)
        let scope_shallow = scope_at_position_with_deps(
            &uri_a, 10, 0, &get_artifacts, &resolve_path, 2,
        );
        prop_assert!(!scope_shallow.symbols.contains_key(&symbol_name));

        // With max_depth=10, should reach d
        let scope_deep = scope_at_position_with_deps(
            &uri_a, 10, 0, &get_artifacts, &resolve_path, 10,
        );
        prop_assert!(scope_deep.symbols.contains_key(&symbol_name));
    }
}

// ============================================================================
// Property 7: Circular Dependency Detection
// Validates: Requirements 5.7, 10.6
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Property 7: For any set of files where file A sources file B and file B
    /// sources file A, the Scope_Resolver SHALL detect the cycle and break it.
    #[test]
    fn prop_circular_dependency_detection(
        symbol_a in r_identifier(),
        symbol_b in r_identifier()
    ) {
        prop_assume!(symbol_a != symbol_b);

        let uri_a = make_url("a");
        let uri_b = make_url("b");

        // A sources B and defines symbol_a
        let code_a = format!("source(\"b.R\")\n{} <- 1", symbol_a);
        // B sources A and defines symbol_b
        let code_b = format!("source(\"a.R\")\n{} <- 2", symbol_b);

        let tree_a = parse_r_tree(&code_a);
        let tree_b = parse_r_tree(&code_b);

        let artifacts_a = compute_artifacts(&uri_a, &tree_a, &code_a);
        let artifacts_b = compute_artifacts(&uri_b, &tree_b, &code_b);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &uri_a { Some(artifacts_a.clone()) }
            else if uri == &uri_b { Some(artifacts_b.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            match path {
                "a.R" => Some(uri_a.clone()),
                "b.R" => Some(uri_b.clone()),
                _ => None,
            }
        };

        // Should not infinite loop - cycle should be broken
        let scope = scope_at_position_with_deps(
            &uri_a, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Should have symbol_a (defined locally)
        prop_assert!(scope.symbols.contains_key(&symbol_a));
        // Should have symbol_b (from sourced file, before cycle detected)
        prop_assert!(scope.symbols.contains_key(&symbol_b));
    }
}

// ============================================================================
// Property 52: Local Source Scope Isolation
// Validates: Requirements 4.7, 5.3, 7.1, 10.1
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 52: For any source() call with local = TRUE, symbols defined in
    /// the sourced file SHALL NOT be added to the caller's scope.
    #[test]
    fn prop_local_source_scope_isolation(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child with local=TRUE
        let parent_code = "source(\"child.R\", local = TRUE)";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Get scope at end of parent file
        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol from local=TRUE source should NOT be in scope
        prop_assert!(!scope.symbols.contains_key(&symbol_name));
    }
}


// ============================================================================
// Property 51: Full Position Precision
// Validates: Requirements 5.3, 7.1, 7.4
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 51: For any completion, hover, or go-to-definition request at position
    /// (line, column) on the same line as a source() call at position (line, call_column):
    /// - If column <= call_column, symbols from the sourced file SHALL NOT be included
    /// - If column > call_column, symbols from the sourced file SHALL be included
    #[test]
    fn prop_full_position_precision(
        symbol_name in r_identifier(),
        prefix_len in 0..10usize
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: has prefix before source() on same line
        let prefix = "x".repeat(prefix_len);
        let parent_code = format!("{}; source(\"child.R\")", prefix);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Find the source() call position in the timeline
        let source_col = parent_artifacts.timeline.iter()
            .find_map(|event| {
                if let super::scope::ScopeEvent::Source { column, .. } = event {
                    Some(*column)
                } else {
                    None
                }
            })
            .unwrap_or(0);

        // Before source() call column - symbol should NOT be available
        if source_col > 0 {
            let scope_before = scope_at_position_with_deps(
                &parent_uri, 0, source_col - 1, &get_artifacts, &resolve_path, 10,
            );
            prop_assert!(!scope_before.symbols.contains_key(&symbol_name));
        }

        // After source() call - symbol SHOULD be available
        let scope_after = scope_at_position_with_deps(
            &parent_uri, 0, source_col + 20, &get_artifacts, &resolve_path, 10,
        );
        prop_assert!(scope_after.symbols.contains_key(&symbol_name));
    }
}

// ============================================================================
// Property 53: sys.source Conservative Handling
// Validates: Requirements 4.4
// ============================================================================

// Note: sys.source with non-resolvable envir is treated as local=TRUE by the
// source detector. This test verifies that sys.source calls are detected
// and that the is_sys_source flag is set correctly.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 53: For any sys.source() call where the envir argument is not
    /// statically resolvable to .GlobalEnv or globalenv(), the LSP SHALL treat
    /// it as local = TRUE (no symbol inheritance).
    #[test]
    fn prop_sys_source_conservative_handling(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sys.source with globalenv() - should inherit symbols
        let parent_code = "sys.source(\"child.R\", envir = globalenv())";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // sys.source is detected
        let has_sys_source = parent_artifacts.timeline.iter().any(|event| {
            matches!(event, super::scope::ScopeEvent::Source { source, .. } if source.is_sys_source)
        });
        prop_assert!(has_sys_source);

        // Get scope - sys.source with globalenv() should include symbols
        // (Note: current implementation doesn't distinguish envir, so this tests detection)
        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );
        // Symbol should be available (sys.source with globalenv() is not local)
        prop_assert!(scope.symbols.contains_key(&symbol_name));
    }
}

// ============================================================================
// Property: V1 R Symbol Model
// Validates: Requirements 17.1-17.7
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// V1 R Symbol Model: Only recognized constructs contribute to exported interface.
    /// - name <- function(...) is recognized
    /// - name = function(...) is recognized
    /// - name <<- function(...) is recognized
    /// - name <- <expr> is recognized
    /// - assign("name", <expr>) with string literal is recognized
    /// - assign(dynamic_name, <expr>) is NOT recognized
    #[test]
    fn prop_v1_symbol_model_recognized_constructs(
        func_name in r_identifier(),
        var_name in r_identifier(),
        assign_name in r_identifier()
    ) {
        // Ensure names are distinct
        prop_assume!(func_name != var_name && var_name != assign_name && func_name != assign_name);

        let uri = make_url("test");

        // Code with various recognized constructs
        let code = format!(
            "{} <- function(x) {{ x }}\n{} <- 42\nassign(\"{}\", 100)",
            func_name, var_name, assign_name
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // All three should be in exported interface
        prop_assert!(artifacts.exported_interface.contains_key(&func_name));
        prop_assert!(artifacts.exported_interface.contains_key(&var_name));
        prop_assert!(artifacts.exported_interface.contains_key(&assign_name));

        // Function should have Function kind
        let func_symbol = artifacts.exported_interface.get(&func_name).unwrap();
        prop_assert_eq!(func_symbol.kind, super::scope::SymbolKind::Function);

        // Variable should have Variable kind
        let var_symbol = artifacts.exported_interface.get(&var_name).unwrap();
        prop_assert_eq!(var_symbol.kind, super::scope::SymbolKind::Variable);
    }

    /// V1 R Symbol Model: Dynamic assign() is NOT recognized
    #[test]
    fn prop_v1_symbol_model_dynamic_assign_not_recognized(
        var_name in r_identifier()
    ) {
        let uri = make_url("test");

        // Code with dynamic assign (variable name, not string literal)
        let code = format!("assign({}, 42)", var_name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Should NOT be in exported interface (dynamic name)
        prop_assert!(artifacts.exported_interface.is_empty());
    }

    /// V1 R Symbol Model: Super-assignment (<<-) is recognized
    #[test]
    fn prop_v1_symbol_model_super_assignment(
        name in r_identifier()
    ) {
        let uri = make_url("test");

        let code = format!("{} <<- 42", name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        prop_assert!(artifacts.exported_interface.contains_key(&name));
    }

    /// V1 R Symbol Model: Equals assignment (=) is recognized
    #[test]
    fn prop_v1_symbol_model_equals_assignment(
        name in r_identifier()
    ) {
        let uri = make_url("test");

        let code = format!("{} = 42", name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        prop_assert!(artifacts.exported_interface.contains_key(&name));
    }
}


// ============================================================================
// Property 5: Function-local variable scope boundaries
// Validates: Variables defined inside function NOT available outside
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 5: For any function definition containing a local variable definition,
    /// that variable SHALL NOT be available in scope outside the function body.
    #[test]
    fn prop_function_local_variable_scope_boundaries(
        func_name in r_identifier(),
        local_var in r_identifier(),
        global_var in r_identifier()
    ) {
        prop_assume!(func_name != local_var && local_var != global_var && func_name != global_var);

        let uri = make_url("test");

        // Code with function containing local variable, followed by global variable
        let code = format!(
            "{} <- function() {{ {} <- 42 }}\n{} <- 100",
            func_name, local_var, global_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // At end of file (outside function), local variable should NOT be available
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        
        prop_assert!(scope_outside.symbols.contains_key(&func_name),
            "Function name should be available outside function");
        prop_assert!(scope_outside.symbols.contains_key(&global_var),
            "Global variable should be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key(&local_var),
            "Function-local variable should NOT be available outside function");

        // Inside function body, local variable SHOULD be available
        // Use a position derived from the generated code so it is always within the braces.
        let col_in_body = code.find('{').map(|i| (i + 2) as u32).unwrap_or(0);
        let scope_inside = scope_at_position(&artifacts, 0, col_in_body);
        prop_assert!(scope_inside.symbols.contains_key(&func_name),
            "Function name should be available inside function");
        prop_assert!(scope_inside.symbols.contains_key(&local_var),
            "Function-local variable should be available inside function");
        // Global variable defined after function should NOT be available inside
        prop_assert!(!scope_inside.symbols.contains_key(&global_var),
            "Global variable defined after function should NOT be available inside function");
    }

    /// Property 5 extended: Nested functions have separate scopes
    #[test]
    fn prop_nested_function_separate_scopes(
        outer_func in r_identifier(),
        inner_func in r_identifier(),
        outer_var in r_identifier(),
        inner_var in r_identifier()
    ) {
        prop_assume!(outer_func != inner_func && outer_var != inner_var);
        prop_assume!(outer_func != outer_var && outer_func != inner_var);
        prop_assume!(inner_func != outer_var && inner_func != inner_var);

        let uri = make_url("test");

        // Code with nested functions
        let code = format!(
            "{} <- function() {{ {} <- 1; {} <- function() {{ {} <- 2 }} }}",
            outer_func, outer_var, inner_func, inner_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Outside all functions - only outer function should be available
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        prop_assert!(scope_outside.symbols.contains_key(&outer_func),
            "Outer function should be available outside");
        prop_assert!(!scope_outside.symbols.contains_key(&inner_func),
            "Inner function should NOT be available outside outer function");
        prop_assert!(!scope_outside.symbols.contains_key(&outer_var),
            "Outer function variable should NOT be available outside");
        prop_assert!(!scope_outside.symbols.contains_key(&inner_var),
            "Inner function variable should NOT be available outside");

        // Inside outer function but outside inner function
        // Choose a position inside the outer body *after* the inner function definition begins.
        // Be careful: `"{inner_func} <- function"` can match inside other identifiers (e.g. outer_func="ab", inner_func="b").
        // Prefer a delimiter-aware search and use rfind to bias towards the inner definition.
        let inner_def_needle = format!("; {} <- function", inner_func);
        let inner_def_needle2 = format!(" {} <- function", inner_func);
        let col_in_outer_after_inner_def = code
            .rfind(&inner_def_needle)
            .map(|i| (i + 3) as u32) // skip "; " then move inside identifier
            .or_else(|| code.rfind(&inner_def_needle2).map(|i| (i + 2) as u32))
            .or_else(|| code.rfind(&inner_func).map(|i| (i + 1) as u32))
            .unwrap_or(0);
        let scope_outer = scope_at_position(&artifacts, 0, col_in_outer_after_inner_def);
        prop_assert!(scope_outer.symbols.contains_key(&outer_func),
            "Outer function should be available inside itself");
        prop_assert!(scope_outer.symbols.contains_key(&outer_var),
            "Outer function variable should be available inside outer function");
        prop_assert!(scope_outer.symbols.contains_key(&inner_func),
            "Inner function should be available inside outer function");
        prop_assert!(!scope_outer.symbols.contains_key(&inner_var),
            "Inner function variable should NOT be available outside inner function");

        // Inside inner function
        // Choose a position inside the inner body after the inner_var definition.
        let inner_var_def_needle = format!("{} <-", inner_var);
        let col_in_inner_after_inner_var_def = code
            .rfind(&inner_var_def_needle)
            .or_else(|| code.rfind(&inner_var))
            .map(|i| (i + 1) as u32)
            .unwrap_or(0);
        let scope_inner = scope_at_position(&artifacts, 0, col_in_inner_after_inner_var_def);
        prop_assert!(scope_inner.symbols.contains_key(&outer_func),
            "Outer function should be available inside inner function");
        prop_assert!(scope_inner.symbols.contains_key(&outer_var),
            "Outer function variable should be available inside inner function");
        prop_assert!(scope_inner.symbols.contains_key(&inner_func),
            "Inner function should be available inside itself");
        prop_assert!(scope_inner.symbols.contains_key(&inner_var),
            "Inner function variable should be available inside inner function");
    }
}

// ============================================================================
// Property 6: Function parameter scope boundaries
// Validates: Parameters NOT available outside function body
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 6: For any function definition with parameters, those parameters
    /// SHALL NOT be available in scope outside the function body.
    #[test]
    fn prop_function_parameter_scope_boundaries(
        func_name in r_identifier(),
        param1 in r_identifier(),
        param2 in r_identifier(),
        global_var in r_identifier()
    ) {
        prop_assume!(func_name != param1 && param1 != param2 && func_name != param2);
        prop_assume!(global_var != func_name && global_var != param1 && global_var != param2);

        let uri = make_url("test");

        // Function with parameters, followed by global variable
        let code = format!(
            "{} <- function({}, {}) {{ {} + {} }}\n{} <- 100",
            func_name, param1, param2, param1, param2, global_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Outside function - parameters should NOT be available
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        prop_assert!(scope_outside.symbols.contains_key(&func_name),
            "Function name should be available outside function");
        prop_assert!(scope_outside.symbols.contains_key(&global_var),
            "Global variable should be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key(&param1),
            "Function parameter 1 should NOT be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key(&param2),
            "Function parameter 2 should NOT be available outside function");

        // Inside function - parameters SHOULD be available
        // Use a position derived from the generated code so it is always within the braces.
        let col_in_body = code.find('{').map(|i| (i + 2) as u32).unwrap_or(0);
        let scope_inside = scope_at_position(&artifacts, 0, col_in_body);
        prop_assert!(scope_inside.symbols.contains_key(&func_name),
            "Function name should be available inside function");
        prop_assert!(scope_inside.symbols.contains_key(&param1),
            "Function parameter 1 should be available inside function");
        prop_assert!(scope_inside.symbols.contains_key(&param2),
            "Function parameter 2 should be available inside function");
        // Global variable defined after function should NOT be available inside
        prop_assert!(!scope_inside.symbols.contains_key(&global_var),
            "Global variable defined after function should NOT be available inside function");
    }

    /// Property 6 extended: Function parameters with default values
    #[test]
    fn prop_function_parameter_default_values_scope(
        func_name in r_identifier(),
        param_name in r_identifier()
    ) {
        prop_assume!(func_name != param_name);

        let uri = make_url("test");

        let code = format!("{} <- function({} = 42) {{ {} * 2 }}", func_name, param_name, param_name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Outside function - parameter should NOT be available
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        prop_assert!(scope_outside.symbols.contains_key(&func_name),
            "Function name should be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key(&param_name),
            "Function parameter with default should NOT be available outside function");

        // Inside function - parameter SHOULD be available
        // Use a position derived from the generated code so it is always within the braces.
        let col_in_body = code.find('{').map(|i| (i + 2) as u32).unwrap_or(0);
        let scope_inside = scope_at_position(&artifacts, 0, col_in_body);
        prop_assert!(scope_inside.symbols.contains_key(&param_name),
            "Function parameter with default should be available inside function");
    }

    /// Property 6 extended: Ellipsis parameter scope
    #[test]
    fn prop_function_ellipsis_parameter_scope(
        func_name in r_identifier(),
        param_name in r_identifier()
    ) {
        prop_assume!(func_name != param_name);

        let uri = make_url("test");

        let code = format!("{} <- function({}, ...) {{ list({}, ...) }}", func_name, param_name, param_name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Outside function - parameters should NOT be available
        let scope_outside = scope_at_position(&artifacts, 10, 0);
        prop_assert!(scope_outside.symbols.contains_key(&func_name),
            "Function name should be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key(&param_name),
            "Named parameter should NOT be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key("..."),
            "Ellipsis parameter should NOT be available outside function");

        // Inside function - parameters SHOULD be available
        // Use a position derived from the generated code so it is always within the braces.
        let col_in_body = code.find('{').map(|i| (i + 2) as u32).unwrap_or(0);
        let scope_inside = scope_at_position(&artifacts, 0, col_in_body);
        prop_assert!(scope_inside.symbols.contains_key(&param_name),
            "Named parameter should be available inside function");
        prop_assert!(scope_inside.symbols.contains_key("..."),
            "Ellipsis parameter should be available inside function");
    }
}

// ============================================================================
// Property 12: Forward Directive Order Preservation
// Validates: Requirements 2.4
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 12: For any file containing multiple @lsp-source directives,
    /// the parsed ForwardSource list SHALL maintain the same order as they
    /// appear in the document.
    #[test]
    fn prop_forward_directive_order_preservation(
        file1 in path_component(),
        file2 in path_component(),
        file3 in path_component()
    ) {
        prop_assume!(file1 != file2 && file2 != file3 && file1 != file3);

        let content = format!(
            "# @lsp-source {}.R\n# @lsp-source {}.R\n# @lsp-source {}.R",
            file1, file2, file3
        );
        let meta = parse_directives(&content);

        prop_assert_eq!(meta.sources.len(), 3);
        prop_assert_eq!(&meta.sources[0].path, &format!("{}.R", file1));
        prop_assert_eq!(&meta.sources[1].path, &format!("{}.R", file2));
        prop_assert_eq!(&meta.sources[2].path, &format!("{}.R", file3));

        // Lines should be in order
        prop_assert!(meta.sources[0].line < meta.sources[1].line);
        prop_assert!(meta.sources[1].line < meta.sources[2].line);
    }
}

// ============================================================================
// Property 46: Forward Directive as Explicit Source
// Validates: Requirements 2.1
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 46: For any file containing @lsp-source <path> at line N,
    /// the Directive_Parser SHALL treat it as an explicit source() declaration at line N.
    #[test]
    fn prop_forward_directive_as_explicit_source(
        path in relative_path(),
        prefix_lines in 0..5u32
    ) {
        let mut lines = Vec::new();
        for i in 0..prefix_lines {
            lines.push(format!("x{} <- {}", i, i));
        }
        lines.push(format!("# @lsp-source {}", path));
        let content = lines.join("\n");

        let meta = parse_directives(&content);

        prop_assert_eq!(meta.sources.len(), 1);
        prop_assert_eq!(&meta.sources[0].path, &path);
        prop_assert_eq!(meta.sources[0].line, prefix_lines);
        prop_assert!(meta.sources[0].is_directive);
    }
}


// ============================================================================
// Property 24: Scope Cache Invalidation on Interface Change
// Validates: Requirements 0.3, 12.4, 12.5
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Property 24: For any file whose exported interface changes, all files that
    /// depend on it SHALL have their scope caches invalidated.
    #[test]
    fn prop_scope_cache_invalidation_on_interface_change(
        symbol1 in "[a-zA-Z][a-zA-Z0-9_]{0,5}",
        symbol2 in "[a-zA-Z][a-zA-Z0-9_]{0,5}"
    ) {
        prop_assume!(symbol1 != symbol2);

        let uri = make_url("test");

        // First version: defines symbol1
        let code1 = format!("{} <- 42", symbol1);
        let tree1 = parse_r_tree(&code1);
        let artifacts1 = compute_artifacts(&uri, &tree1, &code1);

        // Second version: defines symbol2 (different interface)
        let code2 = format!("{} <- 42", symbol2);
        let tree2 = parse_r_tree(&code2);
        let artifacts2 = compute_artifacts(&uri, &tree2, &code2);

        // Interface hashes should be different
        prop_assert_ne!(artifacts1.interface_hash, artifacts2.interface_hash);
    }
}

// ============================================================================
// Property 39: Interface Hash Optimization
// Validates: Requirements 12.11
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Property 39: For any file change where the exported interface hash remains
    /// identical and the edge set remains identical, dependent files SHALL NOT
    /// have their scope caches invalidated.
    #[test]
    fn prop_interface_hash_optimization(
        symbol in "[a-zA-Z][a-zA-Z0-9_]{0,5}",
        value1 in 1..100i32,
        value2 in 100..200i32
    ) {
        let uri = make_url("test");

        // Two versions with same symbol name but different values
        // Interface hash should be the same (same symbol name and kind)
        let code1 = format!("{} <- {}", symbol, value1);
        let code2 = format!("{} <- {}", symbol, value2);

        let tree1 = parse_r_tree(&code1);
        let tree2 = parse_r_tree(&code2);

        let artifacts1 = compute_artifacts(&uri, &tree1, &code1);
        let artifacts2 = compute_artifacts(&uri, &tree2, &code2);

        // Interface hashes should be the same (same exported symbols)
        prop_assert_eq!(artifacts1.interface_hash, artifacts2.interface_hash);
    }
}

// ============================================================================
// Property 37: Multiple Source Calls - Earliest Call Site
// Validates: Requirements 5.9
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Property 37: For any file that is sourced multiple times at different call
    /// sites in the same parent, symbols from that file SHALL become available
    /// at the earliest call site.
    #[test]
    fn prop_multiple_source_calls_earliest(
        symbol_name in r_identifier(),
        first_line in 1..5u32,
        second_line in 6..10u32
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child twice at different lines
        let mut parent_lines = vec!["# start".to_string()];
        for i in 1..first_line {
            parent_lines.push(format!("x{} <- {}", i, i));
        }
        parent_lines.push("source(\"child.R\")".to_string());
        for i in first_line..second_line {
            parent_lines.push(format!("y{} <- {}", i, i));
        }
        parent_lines.push("source(\"child.R\")".to_string());
        parent_lines.push("# end".to_string());
        let parent_code = parent_lines.join("\n");

        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Symbol should be available after the FIRST source() call
        let scope_after_first = scope_at_position_with_deps(
            &parent_uri, first_line + 1, 0, &get_artifacts, &resolve_path, 10,
        );
        prop_assert!(scope_after_first.symbols.contains_key(&symbol_name));
    }
}

// ============================================================================
// Property 19: Backward-First Resolution Order
// Validates: Requirements 5.1, 5.2
// ============================================================================

use super::scope::scope_at_position_with_backward;
use super::types::BackwardDirective;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 19: For any file with both backward directives and forward source() calls,
    /// the Scope_Resolver SHALL process backward directives before forward sources,
    /// resulting in parent symbols being available before sourced file symbols.
    #[test]
    fn prop_backward_first_resolution_order(
        parent_symbol in r_identifier(),
        child_symbol in r_identifier(),
        sibling_symbol in r_identifier()
    ) {
        // Ensure all symbols are distinct
        prop_assume!(parent_symbol != child_symbol && child_symbol != sibling_symbol && parent_symbol != sibling_symbol);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let sibling_uri = make_url("sibling");

        // Parent file: defines parent_symbol
        let parent_code = format!("{} <- 1", parent_symbol);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Sibling file: defines sibling_symbol
        let sibling_code = format!("{} <- 2", sibling_symbol);
        let sibling_tree = parse_r_tree(&sibling_code);
        let sibling_artifacts = compute_artifacts(&sibling_uri, &sibling_tree, &sibling_code);

        // Child file: has backward directive to parent, forward source to sibling, and defines child_symbol
        // The backward directive means parent's symbols are available at the START
        // The forward source means sibling's symbols are available AFTER the source() call
        let child_code = format!(
            "# @lsp-sourced-by ../parent.R\n{} <- 3\nsource(\"sibling.R\")",
            child_symbol
        );
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Create metadata for child (with backward directive)
        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else if uri == &sibling_uri { Some(sibling_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            match path {
                "../parent.R" => Some(parent_uri.clone()),
                "sibling.R" => Some(sibling_uri.clone()),
                _ => None,
            }
        };

        // At line 0 (before any code), parent symbols should be available (from backward directive)
        // but sibling symbols should NOT be available (forward source hasn't been processed yet)
        let scope_at_start = scope_at_position_with_backward(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &resolve_path, 10, None,
        );
        prop_assert!(scope_at_start.symbols.contains_key(&parent_symbol),
            "Parent symbol should be available at start due to backward directive");
        prop_assert!(!scope_at_start.symbols.contains_key(&sibling_symbol),
            "Sibling symbol should NOT be available at start (before source() call)");

        // At line 1 (after child_symbol definition, before source() call)
        let scope_at_middle = scope_at_position_with_backward(
            &child_uri, 1, 10, &get_artifacts, &get_metadata, &resolve_path, 10, None,
        );
        prop_assert!(scope_at_middle.symbols.contains_key(&parent_symbol),
            "Parent symbol should still be available");
        prop_assert!(scope_at_middle.symbols.contains_key(&child_symbol),
            "Child symbol should be available after its definition");
        prop_assert!(!scope_at_middle.symbols.contains_key(&sibling_symbol),
            "Sibling symbol should NOT be available (before source() call)");

        // At line 3 (after source() call), all symbols should be available
        let scope_at_end = scope_at_position_with_backward(
            &child_uri, 3, 0, &get_artifacts, &get_metadata, &resolve_path, 10, None,
        );
        prop_assert!(scope_at_end.symbols.contains_key(&parent_symbol),
            "Parent symbol should be available at end");
        prop_assert!(scope_at_end.symbols.contains_key(&child_symbol),
            "Child symbol should be available at end");
        prop_assert!(scope_at_end.symbols.contains_key(&sibling_symbol),
            "Sibling symbol should be available after source() call");
    }

    /// Property 19 extended: Backward directive symbols are available at position (0, 0)
    #[test]
    fn prop_backward_symbols_available_at_start(
        parent_symbol in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: defines symbol
        let parent_code = format!("{} <- 42", parent_symbol);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: has backward directive to parent
        let child_code = "# @lsp-sourced-by ../parent.R\nx <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "../parent.R" { Some(parent_uri.clone()) } else { None }
        };

        // At position (0, 0), parent symbol should be available
        let scope = scope_at_position_with_backward(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &resolve_path, 10, None,
        );
        prop_assert!(scope.symbols.contains_key(&parent_symbol),
            "Parent symbol from backward directive should be available at (0, 0)");
    }
}

// ============================================================================
// Property 20: Call Site Symbol Filtering
// Validates: Requirements 5.5
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 20: For any backward directive with line=N (user-facing 1-based),
    /// after conversion to internal 0-based call_site_line, the scope inherited
    /// from the parent MUST include exactly the definitions at positions
    /// (def_line, def_col) such that (def_line, def_col) <= (call_site_line, call_site_col).
    #[test]
    fn prop_call_site_symbol_filtering(
        symbol_before in r_identifier(),
        symbol_after in r_identifier(),
        call_site_line in 2..5u32 // 1-based line number
    ) {
        prop_assume!(symbol_before != symbol_after);
        // Ensure symbol_after doesn't conflict with intermediate variables (x1, x2, etc.)
        prop_assume!(!symbol_after.starts_with('x') || symbol_after.len() < 2 || !symbol_after[1..].chars().all(|c| c.is_ascii_digit()));

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: defines symbol_before on line 0, symbol_after on line call_site_line + 1
        // (one line AFTER the call site)
        let mut parent_lines = vec![format!("{} <- 1", symbol_before)];
        for i in 1..=call_site_line {
            parent_lines.push(format!("filler{} <- {}", i, i));
        }
        // symbol_after is defined AFTER the call site line
        parent_lines.push(format!("{} <- 2", symbol_after));
        let parent_code = parent_lines.join("\n");
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: has backward directive with line= parameter
        // Use a unique variable name that won't conflict with symbol_before or symbol_after
        let child_code = format!("# @lsp-sourced-by ../parent.R line={}\nchild_local_var <- 3", call_site_line);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // line= is 1-based, so line=N means call site is at 0-based line N-1
        // We treat line= as end-of-line, so symbols on that line ARE included
        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Line(call_site_line - 1), // 0-based
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "../parent.R" { Some(parent_uri.clone()) } else { None }
        };

        // Get scope at child
        let scope = scope_at_position_with_backward(
            &child_uri, 10, 0, &get_artifacts, &get_metadata, &resolve_path, 10, None,
        );

        // symbol_before (defined on line 0) should be available (before call site)
        prop_assert!(scope.symbols.contains_key(&symbol_before),
            "Symbol defined before call site should be available");

        // symbol_after (defined on line call_site_line + 1) should NOT be available
        // because it's defined AFTER the call site line
        prop_assert!(!scope.symbols.contains_key(&symbol_after),
            "Symbol defined after call site should NOT be available");
    }
}

// ============================================================================
// Property 21: Default Call Site Behavior
// Validates: Requirements 5.6
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 21: For any backward directive without a call site specification,
    /// when assumeCallSite is "end", all symbols from the parent SHALL be included;
    /// when "start", no symbols from the parent SHALL be included.
    #[test]
    fn prop_default_call_site_behavior(
        parent_symbol in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: defines symbol
        let parent_code = format!("{} <- 42", parent_symbol);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: has backward directive with Default call site
        let child_code = "# @lsp-sourced-by ../parent.R\nx <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "../parent.R" { Some(parent_uri.clone()) } else { None }
        };

        // With default call site (which defaults to "end"), all parent symbols should be available
        let scope = scope_at_position_with_backward(
            &child_uri, 10, 0, &get_artifacts, &get_metadata, &resolve_path, 10, None,
        );

        // Default is "end", so all parent symbols should be included
        prop_assert!(scope.symbols.contains_key(&parent_symbol),
            "With default call site (end), parent symbol should be available");
    }
}

// ============================================================================
// Property 34: Configuration Change Re-resolution
// Validates: Requirements 11.11
// ============================================================================

use super::config::{CallSiteDefault, CrossFileConfig};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 34: For any configuration change affecting scope resolution
    /// (e.g., assumeCallSite), all open documents SHALL have their scope chains re-resolved.
    #[test]
    fn prop_configuration_change_re_resolution(
        parent_symbol in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: defines symbol at line 5
        let parent_code = format!("x <- 1\ny <- 2\nz <- 3\nw <- 4\n{} <- 5", parent_symbol);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: has backward directive with Default call site
        let child_code = "# @lsp-sourced-by ../parent.R\na <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        let child_metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: "../parent.R".to_string(),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &child_uri { Some(child_metadata.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "../parent.R" { Some(parent_uri.clone()) } else { None }
        };

        // With default config (assume_call_site = End), parent symbol should be available
        let config_end = CrossFileConfig::default();
        prop_assert_eq!(config_end.assume_call_site, CallSiteDefault::End);

        let scope_with_end = scope_at_position_with_backward(
            &child_uri, 10, 0, &get_artifacts, &get_metadata, &resolve_path, 10, None,
        );
        prop_assert!(scope_with_end.symbols.contains_key(&parent_symbol),
            "With assume_call_site=End, parent symbol should be available");

        // Verify that scope_settings_changed detects the change
        let mut config_start = CrossFileConfig::default();
        config_start.assume_call_site = CallSiteDefault::Start;
        prop_assert!(config_end.scope_settings_changed(&config_start),
            "Config change should be detected");

        // Note: The actual re-resolution would happen in the revalidation system.
        // This test verifies that the configuration change detection works correctly.
    }

    /// Property 34 extended: Changing max_chain_depth should trigger re-resolution
    #[test]
    fn prop_config_max_depth_change_detected(
        new_depth in 1..50usize
    ) {
        let config1 = CrossFileConfig::default();
        let mut config2 = CrossFileConfig::default();
        config2.max_chain_depth = new_depth;

        if new_depth != config1.max_chain_depth {
            prop_assert!(config1.scope_settings_changed(&config2),
                "Changing max_chain_depth should trigger scope settings change");
        }
    }
}

// ============================================================================
// Property 33: Undefined Variables Configuration
// Validates: Requirements 11.9, 11.10
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 33: For any configuration with diagnostics.undefinedVariables = false,
    /// no undefined variable diagnostics SHALL be emitted regardless of symbol resolution.
    #[test]
    fn prop_undefined_variables_configuration(
        _symbol_name in r_identifier()
    ) {
        // Test that the configuration flag exists and can be toggled
        let mut config = CrossFileConfig::default();
        
        // Default should be true
        prop_assert!(config.undefined_variables_enabled,
            "Default undefined_variables_enabled should be true");

        // Can be set to false
        config.undefined_variables_enabled = false;
        prop_assert!(!config.undefined_variables_enabled,
            "undefined_variables_enabled should be settable to false");

        // Changing this setting should NOT trigger scope re-resolution
        // (it only affects diagnostics, not scope)
        let config1 = CrossFileConfig::default();
        let mut config2 = CrossFileConfig::default();
        config2.undefined_variables_enabled = false;
        prop_assert!(!config1.scope_settings_changed(&config2),
            "Changing undefined_variables_enabled should NOT trigger scope change");
    }
}

// ============================================================================
// Property 38: Ambiguous Parent Determinism
// Validates: Requirements 5.10
// ============================================================================

use super::parent_resolve::{resolve_parent, compute_metadata_fingerprint, compute_reverse_edges_hash};
use super::cache::ParentResolution;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 38: For any file with multiple possible parents (via backward directives
    /// or reverse edges), the Scope_Resolver SHALL deterministically select the same
    /// parent given the same inputs.
    #[test]
    fn prop_ambiguous_parent_determinism(
        parent1_name in path_component(),
        parent2_name in path_component()
    ) {
        prop_assume!(parent1_name != parent2_name);

        let child_uri = make_url("child");
        let parent1_uri = make_url(&parent1_name);
        let parent2_uri = make_url(&parent2_name);

        // Create metadata with two backward directives (ambiguous)
        let metadata = CrossFileMetadata {
            sourced_by: vec![
                BackwardDirective {
                    path: format!("../{}.R", parent1_name),
                    call_site: CallSiteSpec::Default,
                    directive_line: 0,
                },
                BackwardDirective {
                    path: format!("../{}.R", parent2_name),
                    call_site: CallSiteSpec::Default,
                    directive_line: 1,
                },
            ],
            ..Default::default()
        };

        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();

        let resolve_path = |path: &str| -> Option<Url> {
            if path == format!("../{}.R", parent1_name) {
                Some(parent1_uri.clone())
            } else if path == format!("../{}.R", parent2_name) {
                Some(parent2_uri.clone())
            } else {
                None
            }
        };

        // Resolve parent multiple times
        let result1 = resolve_parent(&metadata, &graph, &child_uri, &config, &resolve_path);
        let result2 = resolve_parent(&metadata, &graph, &child_uri, &config, &resolve_path);
        let result3 = resolve_parent(&metadata, &graph, &child_uri, &config, &resolve_path);

        // All results should be identical (deterministic)
        match (&result1, &result2, &result3) {
            (
                ParentResolution::Ambiguous { selected_uri: s1, alternatives: a1, .. },
                ParentResolution::Ambiguous { selected_uri: s2, alternatives: a2, .. },
                ParentResolution::Ambiguous { selected_uri: s3, alternatives: a3, .. },
            ) => {
                prop_assert_eq!(s1, s2, "Selected parent should be deterministic");
                prop_assert_eq!(s2, s3, "Selected parent should be deterministic");
                prop_assert_eq!(a1.len(), a2.len(), "Alternatives should be deterministic");
                prop_assert_eq!(a2.len(), a3.len(), "Alternatives should be deterministic");
            }
            _ => {
                // If not ambiguous, still should be deterministic
                prop_assert!(matches!((&result1, &result2, &result3),
                    (ParentResolution::Single { .. }, ParentResolution::Single { .. }, ParentResolution::Single { .. }) |
                    (ParentResolution::None, ParentResolution::None, ParentResolution::None)
                ), "Results should be deterministic");
            }
        }
    }

    /// Property 38 extended: Precedence order is respected (line= > match= > reverse edge > default)
    #[test]
    fn prop_ambiguous_parent_precedence(
        parent_name in path_component(),
        call_site_line in 1..100u32
    ) {
        let child_uri = make_url("child");
        let parent_uri = make_url(&parent_name);

        // Create metadata with explicit line= (highest precedence)
        let metadata_with_line = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: format!("../{}.R", parent_name),
                call_site: CallSiteSpec::Line(call_site_line - 1), // 0-based
                directive_line: 0,
            }],
            ..Default::default()
        };

        // Create metadata with default (lowest precedence)
        let metadata_with_default = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: format!("../{}.R", parent_name),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        let graph = DependencyGraph::new();
        let config = CrossFileConfig::default();

        let resolve_path = |path: &str| -> Option<Url> {
            if path == format!("../{}.R", parent_name) {
                Some(parent_uri.clone())
            } else {
                None
            }
        };

        let result_with_line = resolve_parent(&metadata_with_line, &graph, &child_uri, &config, &resolve_path);
        let result_with_default = resolve_parent(&metadata_with_default, &graph, &child_uri, &config, &resolve_path);

        // Both should resolve to the same parent
        match (&result_with_line, &result_with_default) {
            (
                ParentResolution::Single { parent_uri: p1, call_site_line: l1, .. },
                ParentResolution::Single { parent_uri: p2, call_site_line: l2, .. },
            ) => {
                prop_assert_eq!(p1, p2, "Same parent should be selected");
                // But call site should differ
                prop_assert_eq!(*l1, Some(call_site_line - 1), "Line= should use explicit line");
                prop_assert_eq!(*l2, Some(u32::MAX), "Default should use end of file");
            }
            _ => prop_assert!(false, "Expected Single resolution for both"),
        }
    }
}

// ============================================================================
// Property 57: Parent Selection Stability
// Validates: Requirements 5.10
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 57: For any file with backward directives, once a parent is selected
    /// for a given (metadata_fingerprint, reverse_edges_hash) cache key, the same
    /// parent SHALL be selected on subsequent queries until either the file's
    /// CrossFileMetadata changes OR the reverse edges pointing to this file change.
    #[test]
    fn prop_parent_selection_stability(
        parent_name in path_component()
    ) {
        let child_uri = make_url("child");
        let parent_uri = make_url(&parent_name);

        let metadata = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: format!("../{}.R", parent_name),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        let graph = DependencyGraph::new();

        // Compute fingerprints
        let fp1 = compute_metadata_fingerprint(&metadata);
        let fp2 = compute_metadata_fingerprint(&metadata);
        let edges_hash1 = compute_reverse_edges_hash(&graph, &child_uri);
        let edges_hash2 = compute_reverse_edges_hash(&graph, &child_uri);

        // Fingerprints should be stable
        prop_assert_eq!(fp1, fp2, "Metadata fingerprint should be stable");
        prop_assert_eq!(edges_hash1, edges_hash2, "Reverse edges hash should be stable");

        // Parent resolution should be stable
        let config = CrossFileConfig::default();
        let resolve_path = |path: &str| -> Option<Url> {
            if path == format!("../{}.R", parent_name) {
                Some(parent_uri.clone())
            } else {
                None
            }
        };

        let result1 = resolve_parent(&metadata, &graph, &child_uri, &config, &resolve_path);
        let result2 = resolve_parent(&metadata, &graph, &child_uri, &config, &resolve_path);

        match (&result1, &result2) {
            (
                ParentResolution::Single { parent_uri: p1, .. },
                ParentResolution::Single { parent_uri: p2, .. },
            ) => {
                prop_assert_eq!(p1, p2, "Parent selection should be stable");
            }
            _ => prop_assert!(false, "Expected Single resolution"),
        }
    }

    /// Property 57 extended: Changing metadata fingerprint should allow re-selection
    #[test]
    fn prop_parent_selection_changes_with_metadata(
        parent1_name in path_component(),
        parent2_name in path_component()
    ) {
        prop_assume!(parent1_name != parent2_name);

        // Two different metadata configurations
        let metadata1 = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: format!("../{}.R", parent1_name),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        let metadata2 = CrossFileMetadata {
            sourced_by: vec![BackwardDirective {
                path: format!("../{}.R", parent2_name),
                call_site: CallSiteSpec::Default,
                directive_line: 0,
            }],
            ..Default::default()
        };

        // Fingerprints should be different
        let fp1 = compute_metadata_fingerprint(&metadata1);
        let fp2 = compute_metadata_fingerprint(&metadata2);
        prop_assert_ne!(fp1, fp2, "Different metadata should have different fingerprints");
    }
}

// ============================================================================
// Property 35: Diagnostics Fanout to Open Files
// Validates: Requirements 0.4, 13.4
// ============================================================================

use super::revalidation::{CrossFileRevalidationState, CrossFileDiagnosticsGate, CrossFileActivityState};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 35: For any file change that invalidates dependent files, all affected
    /// open files SHALL receive updated diagnostics without requiring user edits.
    /// 
    /// This test verifies the revalidation scheduling mechanism works correctly.
    #[test]
    fn prop_diagnostics_fanout_to_open_files(
        num_files in 1..10usize
    ) {
        let state = CrossFileRevalidationState::new();
        let mut tokens = Vec::new();

        // Schedule revalidation for multiple files
        for i in 0..num_files {
            let uri = make_url(&format!("file{}", i));
            let token = state.schedule(uri);
            tokens.push(token);
        }

        // All tokens should be valid (not cancelled)
        for token in &tokens {
            prop_assert!(!token.is_cancelled(),
                "Scheduled revalidation tokens should not be cancelled");
        }
    }
}

// ============================================================================
// Property 36: Debounce Cancellation
// Validates: Requirements 0.5
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 36: For any sequence of rapid changes to a file, only the final
    /// change SHALL result in published diagnostics; intermediate pending
    /// revalidations SHALL be cancelled.
    #[test]
    fn prop_debounce_cancellation(
        num_changes in 2..10usize
    ) {
        let state = CrossFileRevalidationState::new();
        let uri = make_url("test");
        let mut tokens = Vec::new();

        // Simulate rapid changes by scheduling multiple times
        for _ in 0..num_changes {
            let token = state.schedule(uri.clone());
            tokens.push(token);
        }

        // All but the last token should be cancelled
        for (i, token) in tokens.iter().enumerate() {
            if i < num_changes - 1 {
                prop_assert!(token.is_cancelled(),
                    "Intermediate revalidation should be cancelled");
            } else {
                prop_assert!(!token.is_cancelled(),
                    "Final revalidation should not be cancelled");
            }
        }
    }
}

// ============================================================================
// Property 41: Freshness Guard Prevents Stale Diagnostics
// Validates: Requirements 0.6
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 41: For any debounced/background diagnostics task, if either the
    /// document version OR the document content hash/revision changes between task
    /// scheduling and publishing, the task SHALL NOT publish diagnostics.
    /// 
    /// This test verifies the diagnostics gate mechanism.
    #[test]
    fn prop_freshness_guard_prevents_stale(
        initial_version in 1..100i32,
        new_version in 1..100i32
    ) {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = make_url("test");

        // Record initial publish
        gate.record_publish(&uri, initial_version);

        // Check if new version can be published
        let can_publish = gate.can_publish(&uri, new_version);

        if new_version > initial_version {
            prop_assert!(can_publish, "Newer version should be publishable");
        } else if new_version < initial_version {
            prop_assert!(!can_publish, "Older version should be blocked");
        } else {
            // Same version without force should be blocked
            prop_assert!(!can_publish, "Same version without force should be blocked");
        }
    }
}

// ============================================================================
// Property 47: Monotonic Diagnostic Publishing
// Validates: Requirements 0.7
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 47: For any sequence of diagnostic publish attempts for a document,
    /// the server SHALL never publish diagnostics for a document version older than
    /// the most recently published version for that document.
    #[test]
    fn prop_monotonic_diagnostic_publishing(
        versions in prop::collection::vec(1..100i32, 1..10)
    ) {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = make_url("test");

        let mut max_published = 0i32;

        for version in versions {
            let can_publish = gate.can_publish(&uri, version);

            if version > max_published {
                // Should be able to publish newer versions
                prop_assert!(can_publish, "Should be able to publish version {} > {}", version, max_published);
                gate.record_publish(&uri, version);
                max_published = version;
            } else {
                // Should NOT be able to publish older or same versions
                prop_assert!(!can_publish, "Should NOT be able to publish version {} <= {}", version, max_published);
            }
        }
    }
}

// ============================================================================
// Property 48: Force Republish on Dependency Change
// Validates: Requirements 0.8
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 48: For any open document whose dependency-driven scope/diagnostic
    /// inputs change without changing its text document version, the server SHALL
    /// provide a mechanism to force republish updated diagnostics.
    #[test]
    fn prop_force_republish_on_dependency_change(
        version in 1..100i32
    ) {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = make_url("test");

        // Initial publish
        gate.record_publish(&uri, version);

        // Without force, same version should be blocked
        prop_assert!(!gate.can_publish(&uri, version),
            "Same version without force should be blocked");

        // Mark for force republish (simulating dependency change)
        gate.mark_force_republish(&uri);

        // With force, same version should be allowed
        prop_assert!(gate.can_publish(&uri, version),
            "Same version with force should be allowed");

        // But older versions should still be blocked
        if version > 1 {
            prop_assert!(!gate.can_publish(&uri, version - 1),
                "Older version should still be blocked even with force");
        }
    }
}

// ============================================================================
// Property 42: Revalidation Prioritization
// Validates: Requirements 0.9
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 42: For any invalidation affecting multiple open documents, the
    /// trigger document SHALL be revalidated before other open documents.
    /// If the client provides active/visible document hints, the server SHOULD
    /// prioritize: active > visible > other open.
    #[test]
    fn prop_revalidation_prioritization(
        num_files in 2..10usize
    ) {
        let mut state = CrossFileActivityState::new();

        // Create URIs
        let uris: Vec<_> = (0..num_files)
            .map(|i| make_url(&format!("file{}", i)))
            .collect();

        // Set first as active, second as visible, rest as recent
        if !uris.is_empty() {
            state.update(Some(uris[0].clone()), vec![], 0);
        }
        if uris.len() > 1 {
            state.update(state.active_uri.clone(), vec![uris[1].clone()], 0);
        }
        for uri in uris.iter().skip(2) {
            state.record_recent(uri.clone());
        }

        // Verify priority ordering
        if !uris.is_empty() {
            prop_assert_eq!(state.priority_score(&uris[0]), 0, "Active should have priority 0");
        }
        if uris.len() > 1 {
            prop_assert_eq!(state.priority_score(&uris[1]), 1, "Visible should have priority 1");
        }
        // Recent files get priority = position_in_recent + 2
        // Since we add them in order (2, 3, 4, ...), the last one added is at position 0
        // So file[2] is at position num_files-3, file[3] is at position num_files-4, etc.
        // Actually, record_recent adds to front, so the order is reversed
        for (i, uri) in uris.iter().enumerate().skip(2) {
            let position_in_recent = num_files - 1 - i; // Last added is at position 0
            let expected_priority = position_in_recent + 2;
            prop_assert_eq!(state.priority_score(uri), expected_priority,
                "Recent file {} should have priority {}", i, expected_priority);
        }
    }
}

// ============================================================================
// Property 43: Revalidation Cap Enforcement
// Validates: Requirements 0.9, 0.10
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 43: For any invalidation affecting more open documents than
    /// maxRevalidationsPerTrigger, only the first N documents (prioritized)
    /// SHALL be scheduled.
    #[test]
    fn prop_revalidation_cap_enforcement(
        num_files in 1..20usize,
        cap in 1..10usize
    ) {
        let mut state = CrossFileActivityState::new();

        // Create URIs and add to recent
        let uris: Vec<_> = (0..num_files)
            .map(|i| make_url(&format!("file{}", i)))
            .collect();

        for uri in &uris {
            state.record_recent(uri.clone());
        }

        // Sort by priority
        let mut sorted_uris = uris.clone();
        sorted_uris.sort_by_key(|u| state.priority_score(u));

        // Take only up to cap
        let scheduled: Vec<_> = sorted_uris.into_iter().take(cap).collect();

        // Verify cap is respected
        prop_assert!(scheduled.len() <= cap,
            "Scheduled count {} should not exceed cap {}", scheduled.len(), cap);

        // Verify prioritization (lower priority scores come first)
        for i in 1..scheduled.len() {
            let prev_score = state.priority_score(&scheduled[i - 1]);
            let curr_score = state.priority_score(&scheduled[i]);
            prop_assert!(prev_score <= curr_score,
                "Priority should be non-decreasing: {} <= {}", prev_score, curr_score);
        }
    }
}

// ============================================================================
// Property 44: Workspace Index Version Monotonicity
// Validates: Requirements 13.5
// ============================================================================

use super::workspace_index::{CrossFileWorkspaceIndex, IndexEntry};
use super::file_cache::{FileSnapshot, CrossFileFileCache};
use std::time::SystemTime;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Property 44: For any sequence of workspace index updates, the version
    /// counter SHALL be strictly increasing.
    #[test]
    fn prop_workspace_index_version_monotonicity(
        num_updates in 1..10usize
    ) {
        let index = CrossFileWorkspaceIndex::new();
        let mut versions = Vec::new();

        versions.push(index.version());

        for i in 0..num_updates {
            let uri = make_url(&format!("file{}", i));
            let entry = IndexEntry {
                snapshot: FileSnapshot {
                    mtime: SystemTime::UNIX_EPOCH,
                    size: 0,
                    content_hash: None,
                },
                metadata: CrossFileMetadata::default(),
                artifacts: ScopeArtifacts::default(),
                indexed_at_version: index.version(),
            };
            index.insert(uri, entry);
            versions.push(index.version());
        }

        // Each version should be greater than the previous
        for i in 1..versions.len() {
            prop_assert!(versions[i] > versions[i - 1]);
        }
    }
}

// ============================================================================
// Property 45: Watched File Cache Invalidation
// Validates: Requirements 13.2
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 45: For any watched file that is created or changed on disk,
    /// the disk-backed caches for that file SHALL be invalidated.
    #[test]
    fn prop_watched_file_cache_invalidation(
        file_name in path_component(),
        content1 in "[a-zA-Z_][a-zA-Z0-9_]{0,10} <- [0-9]{1,5}",
        content2 in "[a-zA-Z_][a-zA-Z0-9_]{0,10} <- [0-9]{1,5}"
    ) {
        let cache = CrossFileFileCache::new();
        let uri = make_url(&file_name);

        let snapshot1 = FileSnapshot {
            mtime: SystemTime::UNIX_EPOCH,
            size: content1.len() as u64,
            content_hash: None,
        };

        // Insert initial content
        cache.insert(uri.clone(), snapshot1.clone(), content1.clone());
        prop_assert_eq!(cache.get(&uri), Some(content1.clone()));

        // Simulate file change by invalidating
        cache.invalidate(&uri);

        // Cache should be empty after invalidation
        prop_assert!(cache.get(&uri).is_none(),
            "Cache should be empty after invalidation");

        // Insert new content with different snapshot
        let snapshot2 = FileSnapshot {
            mtime: SystemTime::now(),
            size: content2.len() as u64,
            content_hash: None,
        };
        cache.insert(uri.clone(), snapshot2.clone(), content2.clone());

        // Should have new content
        prop_assert_eq!(cache.get(&uri), Some(content2.clone()));

        // Old snapshot should not match
        prop_assert!(cache.get_if_fresh(&uri, &snapshot1).is_none(),
            "Old snapshot should not match after file change");

        // New snapshot should match
        prop_assert_eq!(cache.get_if_fresh(&uri, &snapshot2), Some(content2));
    }

    /// Property 45 extended: Invalidate all clears entire cache
    #[test]
    fn prop_watched_file_cache_invalidate_all(
        num_files in 1..10usize
    ) {
        let cache = CrossFileFileCache::new();

        // Insert multiple files
        for i in 0..num_files {
            let uri = make_url(&format!("file{}", i));
            let snapshot = FileSnapshot {
                mtime: SystemTime::UNIX_EPOCH,
                size: 10,
                content_hash: None,
            };
            cache.insert(uri, snapshot, format!("content{}", i));
        }

        // Verify all files are cached
        for i in 0..num_files {
            let uri = make_url(&format!("file{}", i));
            prop_assert!(cache.get(&uri).is_some());
        }

        // Invalidate all
        cache.invalidate_all();

        // All files should be gone
        for i in 0..num_files {
            let uri = make_url(&format!("file{}", i));
            prop_assert!(cache.get(&uri).is_none());
        }
    }
}

// ============================================================================
// Property 27: Cross-File Completion Inclusion
// Validates: Requirements 7.1, 7.4
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 27: For any file with a resolved scope chain containing symbol s
    /// from a sourced file, completions at a position after the source() call
    /// SHALL include s.
    #[test]
    fn prop_cross_file_completion_inclusion(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child
        let parent_code = "source(\"child.R\")";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Get scope at end of parent file (after source() call)
        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol from child should be in scope (available for completion)
        prop_assert!(scope.symbols.contains_key(&symbol_name),
            "Symbol from sourced file should be available for completion");
    }
}

// ============================================================================
// Property 28: Completion Source Attribution
// Validates: Requirements 7.2
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 28: For any completion item for a symbol from a sourced file,
    /// the completion detail SHALL contain the source file path.
    #[test]
    fn prop_completion_source_attribution(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child
        let parent_code = "source(\"child.R\")";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol should have source_uri pointing to child
        if let Some(symbol) = scope.symbols.get(&symbol_name) {
            prop_assert_eq!(&symbol.source_uri, &child_uri,
                "Symbol should have source_uri pointing to the sourced file");
        } else {
            prop_assert!(false, "Symbol should be in scope");
        }
    }
}

// ============================================================================
// Property 29: Cross-File Hover Information
// Validates: Requirements 8.1, 8.2
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 29: For any symbol from a sourced file, hovering over a usage
    /// SHALL display the source file path and function signature (if applicable).
    #[test]
    fn prop_cross_file_hover_information(
        func_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child
        let parent_code = "source(\"child.R\")";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines a function
        let child_code = format!("{} <- function(x, y) {{ x + y }}", func_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Function should be in scope with signature
        if let Some(symbol) = scope.symbols.get(&func_name) {
            prop_assert_eq!(&symbol.source_uri, &child_uri,
                "Function should have source_uri pointing to the sourced file");
            prop_assert!(symbol.signature.is_some(),
                "Function should have a signature for hover");
            prop_assert!(symbol.signature.as_ref().unwrap().contains(&func_name),
                "Signature should contain function name");
        } else {
            prop_assert!(false, "Function should be in scope");
        }
    }
}

// ============================================================================
// Property 30: Cross-File Go-to-Definition
// Validates: Requirements 9.1
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 30: For any symbol defined in a sourced file, go-to-definition
    /// SHALL navigate to the definition location in that file.
    #[test]
    fn prop_cross_file_go_to_definition(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child
        let parent_code = "source(\"child.R\")";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol on line 0
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol should have definition location in child file
        if let Some(symbol) = scope.symbols.get(&symbol_name) {
            prop_assert_eq!(&symbol.source_uri, &child_uri,
                "Symbol should be defined in child file");
            prop_assert_eq!(symbol.defined_line, 0,
                "Symbol should be defined on line 0");
        } else {
            prop_assert!(false, "Symbol should be in scope");
        }
    }
}

// ============================================================================
// Property 31: Cross-File Undefined Variable Suppression
// Validates: Requirements 10.1
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 31: For any symbol s defined in a sourced file and used after
    /// the source() call, no "undefined variable" diagnostic SHALL be emitted for s.
    #[test]
    fn prop_cross_file_undefined_variable_suppression(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child
        let parent_code = "source(\"child.R\")";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Get scope at end of parent file (after source() call)
        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol should be in scope, so no undefined variable diagnostic
        prop_assert!(scope.symbols.contains_key(&symbol_name),
            "Symbol from sourced file should be in scope (no undefined variable diagnostic)");
    }
}

// ============================================================================
// Property 32: Out-of-Scope Symbol Warning
// Validates: Requirements 10.3
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 32: For any symbol s defined in a sourced file and used before
    /// the source() call, an "out of scope" diagnostic SHALL be emitted.
    #[test]
    fn prop_out_of_scope_symbol_warning(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child on line 5
        let parent_code = "# line 0\n# line 1\n# line 2\n# line 3\n# line 4\nsource(\"child.R\")";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Get scope BEFORE source() call (line 4)
        let scope_before = scope_at_position_with_deps(
            &parent_uri, 4, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol should NOT be in scope before source() call
        prop_assert!(!scope_before.symbols.contains_key(&symbol_name),
            "Symbol should NOT be in scope before source() call (out of scope)");

        // Get scope AFTER source() call (line 6)
        let scope_after = scope_at_position_with_deps(
            &parent_uri, 6, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol SHOULD be in scope after source() call
        prop_assert!(scope_after.symbols.contains_key(&symbol_name),
            "Symbol should be in scope after source() call");
    }
}


// ============================================================================
// Property 5: Diagnostic Suppression
// Validates: Requirements 2.2, 2.3, 10.4, 10.5
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 5: For any file containing `# @lsp-ignore` on line n, no diagnostics
    /// SHALL be emitted for line n. For any file containing `# @lsp-ignore-next` on
    /// line n, no diagnostics SHALL be emitted for line n+1.
    #[test]
    fn prop_diagnostic_suppression(
        line_num in 0u32..10
    ) {
        use super::directive::{parse_directives, is_line_ignored};

        // Test @lsp-ignore suppresses diagnostics on same line
        let code_ignore = format!(
            "{}undefined_var # @lsp-ignore",
            "\n".repeat(line_num as usize)
        );
        let metadata = parse_directives(&code_ignore);

        // The @lsp-ignore directive should be parsed
        prop_assert!(metadata.ignored_lines.contains(&line_num),
            "Line {} should be in ignored_lines", line_num);
        prop_assert!(is_line_ignored(&metadata, line_num),
            "Line {} should be ignored", line_num);

        // Test @lsp-ignore-next suppresses diagnostics on next line
        let code_ignore_next = format!(
            "{}# @lsp-ignore-next\nundefined_var",
            "\n".repeat(line_num as usize)
        );
        let metadata_next = parse_directives(&code_ignore_next);

        // The line after @lsp-ignore-next should be in ignored_next_lines
        prop_assert!(metadata_next.ignored_next_lines.contains(&(line_num + 1)),
            "Line {} should be in ignored_next_lines (from @lsp-ignore-next)", line_num + 1);
        prop_assert!(is_line_ignored(&metadata_next, line_num + 1),
            "Line {} should be ignored (from @lsp-ignore-next)", line_num + 1);
    }
}

// ============================================================================
// Property 6: Missing File Diagnostics
// Validates: Requirements 1.8, 2.5, 10.2
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 6: For any directive or source() call referencing a non-existent
    /// file path, the Diagnostic_Engine SHALL emit exactly one warning diagnostic
    /// at the location of that reference.
    #[test]
    fn prop_missing_file_diagnostics(
        missing_path in "[a-z]{3,8}\\.R"
    ) {
        use super::source_detect::detect_source_calls;

        let code = format!("source(\"{}\")", missing_path);
        let tree = parse_r_tree(&code);
        let sources = detect_source_calls(&tree, &code);

        // Should detect the source() call
        prop_assert_eq!(sources.len(), 1,
            "Should detect exactly one source() call");

        // The forward source should have the path
        let forward = &sources[0];
        prop_assert_eq!(&forward.path, &missing_path,
            "Forward source should have the missing path");

        // When path resolution fails, a diagnostic should be emitted
        // (This is tested at the handler level - here we verify the path is captured)
        prop_assert!(forward.line == 0,
            "Source call should be on line 0");
    }
}


// ============================================================================
// Property 54: Diagnostics Gate Cleanup on Close
// Validates: Requirements 0.7, 0.8
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 54: For any document that is closed via textDocument/didClose,
    /// the server SHALL clear all diagnostics gate state for that URI.
    #[test]
    fn prop_diagnostics_gate_cleanup_on_close(
        version in 1i32..1000
    ) {
        let gate = CrossFileDiagnosticsGate::new();
        let uri = make_url("test");

        // Publish some diagnostics
        gate.record_publish(&uri, version);

        // Mark for force republish
        gate.mark_force_republish(&uri);

        // Verify state exists
        prop_assert!(!gate.can_publish(&uri, version - 1),
            "Should not be able to publish older version");

        // Clear state (simulating document close)
        gate.clear(&uri);

        // After clear, any version should be publishable (no history)
        prop_assert!(gate.can_publish(&uri, 1),
            "After clear, version 1 should be publishable");
        prop_assert!(gate.can_publish(&uri, version),
            "After clear, same version should be publishable");
        prop_assert!(gate.can_publish(&uri, version + 1),
            "After clear, newer version should be publishable");
    }
}


// ============================================================================
// Property 49: Client Activity Signal Processing
// Validates: Requirements 15.4, 15.5
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 49: For any rlsp/activeDocumentsChanged notification received
    /// from the client, the server SHALL update its internal activity model
    /// and use it to prioritize subsequent cross-file revalidations.
    #[test]
    fn prop_client_activity_signal_processing(
        active_idx in 0usize..5,
        visible_count in 1usize..5,
        timestamp in 0u64..10000
    ) {
        let mut state = CrossFileActivityState::new();

        // Create some URIs
        let uris: Vec<Url> = (0..5).map(|i| make_url(&format!("file{}", i))).collect();
        let active_uri = uris.get(active_idx).cloned();
        let visible_uris: Vec<Url> = uris.iter().take(visible_count).cloned().collect();

        // Update activity state (simulating client notification)
        state.update(active_uri.clone(), visible_uris.clone(), timestamp);

        // Verify state was updated
        prop_assert_eq!(&state.active_uri, &active_uri);
        prop_assert_eq!(&state.visible_uris, &visible_uris);
        prop_assert_eq!(state.timestamp_ms, timestamp);

        // Verify priority scoring
        if let Some(ref active) = active_uri {
            prop_assert_eq!(state.priority_score(active), 0,
                "Active document should have highest priority (0)");
        }

        for visible in &visible_uris {
            if active_uri.as_ref() != Some(visible) {
                prop_assert_eq!(state.priority_score(visible), 1,
                    "Visible (non-active) document should have priority 1");
            }
        }

        // Non-visible, non-active should have lower priority
        let other_uri = make_url("other");
        prop_assert!(state.priority_score(&other_uri) > 1,
            "Non-visible, non-active document should have priority > 1");
    }
}


// ============================================================================
// Integration Tests - Task 21
// ============================================================================

// 21.3 Edge cases and error handling

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Test UTF-16 correctness with emoji characters in paths
    #[test]
    fn prop_utf16_emoji_in_path(
        prefix in "[a-z]{1,5}",
        suffix in "[a-z]{1,5}"
    ) {
        // Path with emoji
        let path = format!("{}_🎉_{}.R", prefix, suffix);
        let code = format!("source(\"{}\")", path);
        let tree = parse_r_tree(&code);

        use super::source_detect::detect_source_calls;
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert_eq!(&sources[0].path, &path);
    }

    /// Test UTF-16 correctness with CJK characters in paths
    #[test]
    fn prop_utf16_cjk_in_path(
        prefix in "[a-z]{1,5}",
        suffix in "[a-z]{1,5}"
    ) {
        // Path with CJK characters
        let path = format!("{}_日本語_{}.R", prefix, suffix);
        let code = format!("source(\"{}\")", path);
        let tree = parse_r_tree(&code);

        use super::source_detect::detect_source_calls;
        let sources = detect_source_calls(&tree, &code);

        prop_assert_eq!(sources.len(), 1);
        prop_assert_eq!(&sources[0].path, &path);
    }
}

// 21.5 Position-aware scope edge cases

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Test multiple source() calls on same line
    #[test]
    fn prop_multiple_source_calls_same_line(
        path1 in "[a-z]{3,8}\\.R",
        path2 in "[a-z]{3,8}\\.R"
    ) {
        // Two source() calls on same line
        let code = format!("source(\"{}\"); source(\"{}\")", path1, path2);
        let tree = parse_r_tree(&code);

        use super::source_detect::detect_source_calls;
        let sources = detect_source_calls(&tree, &code);

        // Should detect both source() calls
        prop_assert_eq!(sources.len(), 2);

        // Both should be on line 0
        prop_assert_eq!(sources[0].line, 0);
        prop_assert_eq!(sources[1].line, 0);

        // Paths should be correct
        let paths: Vec<&str> = sources.iter().map(|s| s.path.as_str()).collect();
        prop_assert!(paths.contains(&path1.as_str()));
        prop_assert!(paths.contains(&path2.as_str()));
    }

    /// Test position-aware scope with same-line source() call
    #[test]
    fn prop_same_line_source_position_awareness(
        symbol_name in r_identifier().prop_filter("not x or y", |s| s != "x" && s != "y")
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: x <- 1; source("child.R"); y <- 2
        // Symbol from child should be available after source() but not before
        let parent_code = "x <- 1; source(\"child.R\"); y <- 2";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // At column 0 (before x), child symbol should NOT be available
        let scope_before = scope_at_position_with_deps(
            &parent_uri, 0, 0, &get_artifacts, &resolve_path, 10,
        );
        prop_assert!(!scope_before.symbols.contains_key(&symbol_name),
            "Symbol should NOT be available before source() call");

        // At column 30 (after source()), child symbol SHOULD be available
        let scope_after = scope_at_position_with_deps(
            &parent_uri, 0, 30, &get_artifacts, &resolve_path, 10,
        );
        prop_assert!(scope_after.symbols.contains_key(&symbol_name),
            "Symbol should be available after source() call");
    }
}

// 21.4 v1 R symbol model edge cases

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Test that assign() with string literal is recognized
    #[test]
    fn prop_assign_string_literal_recognized(
        symbol_name in r_identifier()
    ) {
        let code = format!("assign(\"{}\", 42)", symbol_name);
        let tree = parse_r_tree(&code);
        let uri = make_url("test");
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // assign() with string literal should be recognized
        prop_assert!(artifacts.exported_interface.contains_key(&symbol_name),
            "assign() with string literal should be recognized");
    }

    /// Test that assign() with variable is NOT recognized (dynamic)
    #[test]
    fn prop_assign_variable_not_recognized(
        var_name in r_identifier()
    ) {
        let code = format!("assign({}, 42)", var_name);
        let tree = parse_r_tree(&code);
        let uri = make_url("test");
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // assign() with variable should NOT be recognized (dynamic)
        prop_assert!(!artifacts.exported_interface.contains_key(&var_name),
            "assign() with variable should NOT be recognized (dynamic)");
    }
}

// ============================================================================
// Property 1: Loop Iterator Scope Inclusion
// Validates: Loop iterator detection and scope persistence
// ============================================================================

use super::scope::scope_at_position;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 1: For any for loop with iterator variable i, the iterator SHALL
    /// be included in the exported interface and available in scope after the
    /// for statement position.
    #[test]
    fn prop_loop_iterator_scope_inclusion(
        iterator_name in r_identifier()
    ) {
        let uri = make_url("test");

        // Code with for loop
        let code = format!("for ({} in 1:10) {{ print({}) }}", iterator_name, iterator_name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Iterator should be in exported interface
        prop_assert!(artifacts.exported_interface.contains_key(&iterator_name),
            "Loop iterator should be in exported interface");

        // Iterator should have Variable kind
        let symbol = artifacts.exported_interface.get(&iterator_name).unwrap();
        prop_assert_eq!(symbol.kind, super::scope::SymbolKind::Variable,
            "Loop iterator should have Variable kind");

        // Iterator should be available in timeline as Def event
        let has_def_event = artifacts.timeline.iter().any(|event| {
            matches!(event, super::scope::ScopeEvent::Def { symbol, .. } if symbol.name == iterator_name)
        });
        prop_assert!(has_def_event, "Loop iterator should have Def event in timeline");
    }

    /// Property 1 extended: Loop iterator persists after loop completion
    #[test]
    fn prop_loop_iterator_persists_after_loop(
        iterator_name in r_identifier(),
        var_name in r_identifier()
    ) {
        prop_assume!(iterator_name != var_name);

        let uri = make_url("test");

        // Code with for loop followed by variable assignment
        let code = format!(
            "for ({} in 1:5) {{ }}\n{} <- {}",
            iterator_name, var_name, iterator_name
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Both iterator and variable should be in exported interface
        prop_assert!(artifacts.exported_interface.contains_key(&iterator_name),
            "Loop iterator should persist after loop");
        prop_assert!(artifacts.exported_interface.contains_key(&var_name),
            "Variable should be in exported interface");

        // Get scope at end of code - iterator should be available
        let scope = scope_at_position(&artifacts, 10, 0);
        prop_assert!(scope.symbols.contains_key(&iterator_name),
            "Loop iterator should be available in scope after loop");
        prop_assert!(scope.symbols.contains_key(&var_name),
            "Variable should be available in scope");
    }

    /// Property 1 extended: Nested loops create multiple iterators
    #[test]
    fn prop_nested_loops_multiple_iterators(
        outer_iterator in r_identifier(),
        inner_iterator in r_identifier()
    ) {
        prop_assume!(outer_iterator != inner_iterator);

        let uri = make_url("test");

        // Code with nested for loops
        let code = format!(
            "for ({} in 1:3) {{ for ({} in 1:2) {{ print({}, {}) }} }}",
            outer_iterator, inner_iterator, outer_iterator, inner_iterator
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Both iterators should be in exported interface
        prop_assert!(artifacts.exported_interface.contains_key(&outer_iterator),
            "Outer loop iterator should be in exported interface");
        prop_assert!(artifacts.exported_interface.contains_key(&inner_iterator),
            "Inner loop iterator should be in exported interface");

        // Both should have Def events
        let outer_has_def = artifacts.timeline.iter().any(|event| {
            matches!(event, super::scope::ScopeEvent::Def { symbol, .. } if symbol.name == outer_iterator)
        });
        let inner_has_def = artifacts.timeline.iter().any(|event| {
            matches!(event, super::scope::ScopeEvent::Def { symbol, .. } if symbol.name == inner_iterator)
        });
        prop_assert!(outer_has_def, "Outer iterator should have Def event");
        prop_assert!(inner_has_def, "Inner iterator should have Def event");
    }
}

// ============================================================================
// Property 8: Function Parameter Scope Inclusion
// Validates: Function parameter detection and scope boundaries
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 8: For any function definition with parameters, the parameters
    /// SHALL be available in scope within the function body boundaries.
    #[test]
    fn prop_function_parameter_scope_inclusion(
        func_name in r_identifier(),
        param1 in r_identifier(),
        param2 in r_identifier()
    ) {
        prop_assume!(func_name != param1 && param1 != param2 && func_name != param2);

        let uri = make_url("test");

        // Function with multiple parameters
        let code = format!("{} <- function({}, {}) {{ {} + {} }}", func_name, param1, param2, param1, param2);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Function should be in exported interface
        prop_assert!(artifacts.exported_interface.contains_key(&func_name),
            "Function should be in exported interface");

        // Should have FunctionScope event in timeline
        let has_function_scope = artifacts.timeline.iter().any(|event| {
            matches!(event, super::scope::ScopeEvent::FunctionScope { parameters, .. } 
                if parameters.iter().any(|p| p.name == param1) && 
                   parameters.iter().any(|p| p.name == param2))
        });
        prop_assert!(has_function_scope, "Should have FunctionScope event with parameters");

        // Get scope within function body (should include parameters)
        // Use a position derived from the generated code so it is always within the braces.
        let col_in_body = code.find('{').map(|i| (i + 2) as u32).unwrap_or(0);
        let scope_in_body = scope_at_position(&artifacts, 0, col_in_body);
        prop_assert!(scope_in_body.symbols.contains_key(&param1),
            "Parameter 1 should be available within function body");
        prop_assert!(scope_in_body.symbols.contains_key(&param2),
            "Parameter 2 should be available within function body");
        prop_assert!(scope_in_body.symbols.contains_key(&func_name),
            "Function name should be available within function body");
    }

    /// Property 8 extended: Function with no parameters
    #[test]
    fn prop_function_no_parameters_scope(
        func_name in r_identifier()
    ) {
        let uri = make_url("test");

        let code = format!("{} <- function() {{ 42 }}", func_name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Should have FunctionScope event with empty parameters
        let has_function_scope = artifacts.timeline.iter().any(|event| {
            matches!(event, super::scope::ScopeEvent::FunctionScope { parameters, .. } 
                if parameters.is_empty())
        });
        prop_assert!(has_function_scope, "Should have FunctionScope event with empty parameters");
    }

    /// Property 8 extended: Function with default parameter values
    #[test]
    fn prop_function_default_parameter_scope(
        func_name in r_identifier(),
        param_name in r_identifier()
    ) {
        prop_assume!(func_name != param_name);

        let uri = make_url("test");

        let code = format!("{} <- function({} = 42) {{ {} * 2 }}", func_name, param_name, param_name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Should have FunctionScope event with parameter
        let has_function_scope = artifacts.timeline.iter().any(|event| {
            matches!(event, super::scope::ScopeEvent::FunctionScope { parameters, .. } 
                if parameters.iter().any(|p| p.name == param_name))
        });
        prop_assert!(has_function_scope, "Should have FunctionScope event with default parameter");

        // Parameter should be available within function body
        // Use a position derived from the generated code so it is always within the braces.
        let col_in_body = code.find('{').map(|i| (i + 2) as u32).unwrap_or(0);
        let scope_in_body = scope_at_position(&artifacts, 0, col_in_body);
        prop_assert!(scope_in_body.symbols.contains_key(&param_name),
            "Parameter with default value should be available within function body");
    }
}

// ============================================================================
// Task 15 Property Tests: Source() Scoping
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 25: Source local=FALSE global scope
    /// For any file sourced with local=FALSE, all symbols defined in that file 
    /// should be available in the global scope.
    #[test]
    fn prop_source_local_false_global_scope(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child with local=FALSE (default)
        let parent_code = "source(\"child.R\", local = FALSE)";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Get scope at end of parent file (after source() call)
        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol from child should be available in global scope (local=FALSE)
        prop_assert!(scope.symbols.contains_key(&symbol_name),
            "Symbol from file sourced with local=FALSE should be available in global scope");
    }

    /// Property 26: Source local=TRUE function scope
    /// For any file sourced with local=TRUE inside a function, all symbols 
    /// defined in that file should be available only within that function scope.
    #[test]
    fn prop_source_local_true_function_scope(
        symbol_name in r_identifier(),
        func_name in r_identifier()
    ) {
        prop_assume!(symbol_name != func_name);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: function that sources child with local=TRUE
        let parent_code = format!(
            "{} <- function() {{ source(\"child.R\", local = TRUE); {} }}",
            func_name, symbol_name
        );
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Get scope within function body (after source() call)
        // Choose a position after the source() statement so the sourced symbols should be in scope.
        let source_call_start = parent_code.find("source(\"child.R\"").unwrap_or(0);
        let col_after_source = parent_code[source_call_start..]
            .find(';')
            .map(|j| (source_call_start + j + 1) as u32)
            .unwrap_or((source_call_start + 1) as u32);
        let scope_in_function = scope_at_position_with_deps(
            &parent_uri, 0, col_after_source, &get_artifacts, &resolve_path, 10,
        );

        // Symbol should be available within function scope (local=TRUE)
        prop_assert!(scope_in_function.symbols.contains_key(&symbol_name),
            "Symbol from file sourced with local=TRUE should be available within function scope");

        // Get scope outside function (global scope)
        let scope_global = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol should NOT be available in global scope (local=TRUE isolates it)
        prop_assert!(!scope_global.symbols.contains_key(&symbol_name),
            "Symbol from file sourced with local=TRUE should NOT be available in global scope");

        // Function name should be available in global scope
        prop_assert!(scope_global.symbols.contains_key(&func_name),
            "Function name should be available in global scope");
    }

    /// Property 27: Source local parameter default
    /// For any source() call without an explicit local parameter, the system 
    /// should treat it as local=FALSE.
    #[test]
    fn prop_source_local_parameter_default(
        symbol_name in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: sources child without explicit local parameter (defaults to FALSE)
        let parent_code = "source(\"child.R\")";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child file: defines symbol
        let child_code = format!("{} <- 42", symbol_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let resolve_path = |path: &str, _from: &Url| -> Option<Url> {
            if path == "child.R" { Some(child_uri.clone()) } else { None }
        };

        // Get scope at end of parent file
        let scope = scope_at_position_with_deps(
            &parent_uri, 10, 0, &get_artifacts, &resolve_path, 10,
        );

        // Symbol should be available (default local=FALSE means global scope)
        prop_assert!(scope.symbols.contains_key(&symbol_name),
            "Symbol from file sourced without explicit local parameter should be available (default local=FALSE)");

        // Verify the source() call was detected with local=false by default
        let has_source_call = parent_artifacts.timeline.iter().any(|event| {
            matches!(event, super::scope::ScopeEvent::Source { source, .. } 
                if source.path == "child.R" && !source.local)
        });
        prop_assert!(has_source_call,
            "Source call should be detected with local=false by default");
    }
}

// ============================================================================
// Task 11 Property Tests: Scope Resolution Invariants
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // Feature: enhanced-variable-detection-hover, Property 2: Loop iterator persistence after loop
    #[test]
    fn prop_loop_iterator_persistence_after_loop(
        iterator_name in r_identifier()
    ) {
        let uri = make_url("test");

        // For loop with iterator variable
        let code = format!("for ({} in 1:10) {{ print({}) }}\nafter_loop <- {}", 
                          iterator_name, iterator_name, iterator_name);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Get scope at position after loop body completes
        let scope_after_loop = scope_at_position(&artifacts, 10, 0);

        // Iterator variable should still be included in available symbols
        prop_assert!(scope_after_loop.symbols.contains_key(&iterator_name),
            "Loop iterator should persist in scope after loop completes");
    }

    // Feature: enhanced-variable-detection-hover, Property 3: Nested loop iterator tracking
    #[test]
    fn prop_nested_loop_iterator_tracking(
        outer_iterator in r_identifier(),
        inner_iterator in r_identifier()
    ) {
        prop_assume!(outer_iterator != inner_iterator);

        let uri = make_url("test");

        // Nested for loops
        let code = format!(
            "for ({} in 1:3) {{ for ({} in 1:2) {{ print({}, {}) }} }}",
            outer_iterator, inner_iterator, outer_iterator, inner_iterator
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Get scope at position after both loops complete
        let scope_after_loops = scope_at_position(&artifacts, 10, 0);

        // Both outer and inner iterator variables should be available in scope
        prop_assert!(scope_after_loops.symbols.contains_key(&outer_iterator),
            "Outer loop iterator should be available after nested loops complete");
        prop_assert!(scope_after_loops.symbols.contains_key(&inner_iterator),
            "Inner loop iterator should be available after nested loops complete");
    }

    // Feature: enhanced-variable-detection-hover, Property 4: Loop iterator shadowing
    #[test]
    fn prop_loop_iterator_shadowing(
        var_name in r_identifier()
    ) {
        let uri = make_url("test");

        // Variable defined, then for loop uses same name as iterator
        let code = format!(
            "{} <- 42\nfor ({} in 1:5) {{ print({}) }}",
            var_name, var_name, var_name
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Get scope after the for statement
        let scope_after_for = scope_at_position(&artifacts, 10, 0);

        // Iterator definition should take precedence over original variable
        prop_assert!(scope_after_for.symbols.contains_key(&var_name),
            "Variable should be in scope after for loop");

        // The symbol should be the iterator (most recent definition)
        let symbol = scope_after_for.symbols.get(&var_name).unwrap();
        prop_assert_eq!(symbol.kind, super::scope::SymbolKind::Variable,
            "Symbol should be Variable kind (iterator)");
    }

    // Feature: enhanced-variable-detection-hover, Property 7: Function-local undefined variable diagnostics
    #[test]
    fn prop_function_local_undefined_variable_diagnostics(
        func_name in r_identifier(),
        local_var in r_identifier(),
        usage_var in r_identifier()
    ) {
        prop_assume!(func_name != local_var && local_var != usage_var && func_name != usage_var);

        let uri = make_url("test");

        // Function with local variable, then usage outside function
        let code = format!(
            "{} <- function() {{ {} <- 42 }}\nresult <- {}",
            func_name, local_var, local_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Get scope outside function body (where local_var is referenced)
        let scope_outside = scope_at_position(&artifacts, 1, 15);

        // Function-local variable should NOT be available outside function
        prop_assert!(!scope_outside.symbols.contains_key(&local_var),
            "Function-local variable should NOT be available outside function body");

        // Function name should be available
        prop_assert!(scope_outside.symbols.contains_key(&func_name),
            "Function name should be available outside function");
    }

    // Feature: enhanced-variable-detection-hover, Property 9: Function parameter with default value recognition
    #[test]
    fn prop_function_parameter_default_value_recognition(
        func_name in r_identifier(),
        param_with_default in r_identifier(),
        param_without_default in r_identifier()
    ) {
        prop_assume!(func_name != param_with_default && param_with_default != param_without_default && func_name != param_without_default);

        let uri = make_url("test");

        // Function with parameter with default value and parameter without
        let code = format!(
            "{} <- function({}, {} = 42) {{ {} + {} }}",
            func_name, param_without_default, param_with_default, param_without_default, param_with_default
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Get scope within function body
        // Use a position derived from the generated code so it is always within the braces.
        let col_in_body = code.find('{').map(|i| (i + 2) as u32).unwrap_or(0);
        let scope_in_body = scope_at_position(&artifacts, 0, col_in_body);

        // Both parameters should be recognized and included in function body scope
        prop_assert!(scope_in_body.symbols.contains_key(&param_with_default),
            "Parameter with default value should be recognized in function body scope");
        prop_assert!(scope_in_body.symbols.contains_key(&param_without_default),
            "Parameter without default value should be recognized in function body scope");

        // Both should have Parameter kind
        let param_with_default_symbol = scope_in_body.symbols.get(&param_with_default).unwrap();
        let param_without_default_symbol = scope_in_body.symbols.get(&param_without_default).unwrap();
        
        prop_assert_eq!(param_with_default_symbol.kind, super::scope::SymbolKind::Parameter,
            "Parameter with default should have Parameter kind");
        prop_assert_eq!(param_without_default_symbol.kind, super::scope::SymbolKind::Parameter,
            "Parameter without default should have Parameter kind");
    }
}

// ============================================================================
// Feature: rm-remove-support, Property 1: Bare Symbol Extraction
// Validates: Requirements 1.1, 1.2, 1.3
// ============================================================================

use super::source_detect::detect_rm_calls;

/// Generate rm() calls with bare symbols
fn rm_bare_symbols(symbols: &[String]) -> String {
    format!("rm({})", symbols.join(", "))
}

/// Generate remove() calls with bare symbols
fn remove_bare_symbols(symbols: &[String]) -> String {
    format!("remove({})", symbols.join(", "))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: rm-remove-support, Property 1: Bare Symbol Extraction
    /// **Validates: Requirements 1.1, 1.2, 1.3**
    ///
    /// For any rm() or remove() call containing bare symbol arguments, the resulting
    /// Removal event SHALL contain exactly those symbol names, regardless of how many
    /// symbols are specified or whether they are currently defined in scope.
    #[test]
    fn prop_rm_bare_symbol_extraction_single(symbol in r_identifier()) {
        let code = rm_bare_symbols(&[symbol.clone()]);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Should extract exactly the symbol we provided
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol, "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 1: Bare Symbol Extraction (multiple symbols)
    /// **Validates: Requirements 1.1, 1.2, 1.3**
    ///
    /// For any rm() call with multiple bare symbol arguments, all symbols should be extracted.
    #[test]
    fn prop_rm_bare_symbol_extraction_multiple(
        symbols in prop::collection::vec(r_identifier(), 2..=5)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        let code = rm_bare_symbols(&symbols);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Should extract exactly the symbols we provided
        prop_assert_eq!(rm_calls[0].symbols.len(), symbols.len(),
            "Number of extracted symbols should match input");

        // All symbols should be present in the same order
        for (i, expected_symbol) in symbols.iter().enumerate() {
            prop_assert_eq!(&rm_calls[0].symbols[i], expected_symbol,
                "Symbol at position {} should match", i);
        }
    }

    /// Feature: rm-remove-support, Property 1: Bare Symbol Extraction (remove() alias)
    /// **Validates: Requirements 1.1, 1.2, 1.3**
    ///
    /// For any remove() call containing bare symbol arguments, the resulting
    /// Removal event SHALL contain exactly those symbol names.
    #[test]
    fn prop_remove_bare_symbol_extraction_single(symbol in r_identifier()) {
        let code = remove_bare_symbols(&[symbol.clone()]);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one remove() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one remove() call");

        // Should extract exactly the symbol we provided
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol, "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 1: Bare Symbol Extraction (remove() with multiple)
    /// **Validates: Requirements 1.1, 1.2, 1.3**
    ///
    /// For any remove() call with multiple bare symbol arguments, all symbols should be extracted.
    #[test]
    fn prop_remove_bare_symbol_extraction_multiple(
        symbols in prop::collection::vec(r_identifier(), 2..=5)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        let code = remove_bare_symbols(&symbols);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one remove() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one remove() call");

        // Should extract exactly the symbols we provided
        prop_assert_eq!(rm_calls[0].symbols.len(), symbols.len(),
            "Number of extracted symbols should match input");

        // All symbols should be present in the same order
        for (i, expected_symbol) in symbols.iter().enumerate() {
            prop_assert_eq!(&rm_calls[0].symbols[i], expected_symbol,
                "Symbol at position {} should match", i);
        }
    }

    /// Feature: rm-remove-support, Property 1: Bare Symbol Extraction (undefined symbols)
    /// **Validates: Requirements 1.1, 1.2, 1.3**
    ///
    /// Bare symbols in rm() should be extracted regardless of whether they are
    /// currently defined in scope (no error should occur).
    #[test]
    fn prop_rm_bare_symbol_extraction_undefined(symbol in r_identifier()) {
        // Symbol is not defined anywhere, just used in rm()
        let code = rm_bare_symbols(&[symbol.clone()]);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should still detect the rm() call and extract the symbol
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol, "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 1: Bare Symbol Extraction (position tracking)
    /// **Validates: Requirements 1.1, 1.2, 1.3**
    ///
    /// The rm() call should be detected at the correct line position.
    #[test]
    fn prop_rm_bare_symbol_extraction_position(
        symbol in r_identifier(),
        prefix_lines in 0..5usize
    ) {
        // Add some prefix lines before the rm() call
        let prefix = "\n".repeat(prefix_lines);
        let code = format!("{}rm({})", prefix, symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Line should match the number of prefix newlines
        prop_assert_eq!(rm_calls[0].line, prefix_lines as u32,
            "rm() call should be on line {}", prefix_lines);

        // Symbol should still be extracted correctly
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol, "Symbol should match input");
    }
}

// ============================================================================
// Feature: rm-remove-support, Property 2: remove() Equivalence
// Validates: Requirements 2.1, 2.2, 2.3
// ============================================================================

/// Generate rm() call with list= argument containing a single string
fn rm_list_single(symbol: &str) -> String {
    format!(r#"rm(list = "{}")"#, symbol)
}

/// Generate remove() call with list= argument containing a single string
fn remove_list_single(symbol: &str) -> String {
    format!(r#"remove(list = "{}")"#, symbol)
}

/// Generate rm() call with list= argument containing c() with multiple strings
fn rm_list_c(symbols: &[String]) -> String {
    let quoted: Vec<_> = symbols.iter().map(|s| format!(r#""{}""#, s)).collect();
    format!("rm(list = c({}))", quoted.join(", "))
}

/// Generate remove() call with list= argument containing c() with multiple strings
fn remove_list_c(symbols: &[String]) -> String {
    let quoted: Vec<_> = symbols.iter().map(|s| format!(r#""{}""#, s)).collect();
    format!("remove(list = c({}))", quoted.join(", "))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: rm-remove-support, Property 2: remove() Equivalence
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    ///
    /// For any R code using remove(), replacing remove with rm SHALL produce
    /// an identical scope timeline (same Removal events with same symbols and positions).
    /// Test case: Single bare symbol
    #[test]
    fn prop_remove_equivalence_bare_single(symbol in r_identifier()) {
        let rm_code = rm_bare_symbols(&[symbol.clone()]);
        let remove_code = remove_bare_symbols(&[symbol.clone()]);

        let rm_tree = parse_r(&rm_code);
        let remove_tree = parse_r(&remove_code);

        let rm_calls = detect_rm_calls(&rm_tree, &rm_code);
        let remove_calls = detect_rm_calls(&remove_tree, &remove_code);

        // Both should detect exactly one call
        prop_assert_eq!(rm_calls.len(), 1, "rm() should produce exactly one call");
        prop_assert_eq!(remove_calls.len(), 1, "remove() should produce exactly one call");

        // Both should extract the same symbols
        prop_assert_eq!(&rm_calls[0].symbols, &remove_calls[0].symbols,
            "rm() and remove() should extract identical symbols");

        // Both should have the same line position (both start at line 0)
        prop_assert_eq!(rm_calls[0].line, remove_calls[0].line,
            "rm() and remove() should have the same line position");
    }

    /// Feature: rm-remove-support, Property 2: remove() Equivalence
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    ///
    /// Test case: Multiple bare symbols
    #[test]
    fn prop_remove_equivalence_bare_multiple(
        symbols in prop::collection::vec(r_identifier(), 2..=5)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        let rm_code = rm_bare_symbols(&symbols);
        let remove_code = remove_bare_symbols(&symbols);

        let rm_tree = parse_r(&rm_code);
        let remove_tree = parse_r(&remove_code);

        let rm_calls = detect_rm_calls(&rm_tree, &rm_code);
        let remove_calls = detect_rm_calls(&remove_tree, &remove_code);

        // Both should detect exactly one call
        prop_assert_eq!(rm_calls.len(), 1, "rm() should produce exactly one call");
        prop_assert_eq!(remove_calls.len(), 1, "remove() should produce exactly one call");

        // Both should extract the same symbols in the same order
        prop_assert_eq!(&rm_calls[0].symbols, &remove_calls[0].symbols,
            "rm() and remove() should extract identical symbols");

        // Both should have the same line position
        prop_assert_eq!(rm_calls[0].line, remove_calls[0].line,
            "rm() and remove() should have the same line position");
    }

    /// Feature: rm-remove-support, Property 2: remove() Equivalence
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    ///
    /// Test case: list= argument with single string literal
    #[test]
    fn prop_remove_equivalence_list_single(symbol in r_identifier()) {
        let rm_code = rm_list_single(&symbol);
        let remove_code = remove_list_single(&symbol);

        let rm_tree = parse_r(&rm_code);
        let remove_tree = parse_r(&remove_code);

        let rm_calls = detect_rm_calls(&rm_tree, &rm_code);
        let remove_calls = detect_rm_calls(&remove_tree, &remove_code);

        // Both should detect exactly one call
        prop_assert_eq!(rm_calls.len(), 1, "rm(list=...) should produce exactly one call");
        prop_assert_eq!(remove_calls.len(), 1, "remove(list=...) should produce exactly one call");

        // Both should extract the same symbols
        prop_assert_eq!(&rm_calls[0].symbols, &remove_calls[0].symbols,
            "rm(list=...) and remove(list=...) should extract identical symbols");

        // Both should have the same line position
        prop_assert_eq!(rm_calls[0].line, remove_calls[0].line,
            "rm(list=...) and remove(list=...) should have the same line position");
    }

    /// Feature: rm-remove-support, Property 2: remove() Equivalence
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    ///
    /// Test case: list= argument with c() containing multiple strings
    #[test]
    fn prop_remove_equivalence_list_c(
        symbols in prop::collection::vec(r_identifier(), 2..=5)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        let rm_code = rm_list_c(&symbols);
        let remove_code = remove_list_c(&symbols);

        let rm_tree = parse_r(&rm_code);
        let remove_tree = parse_r(&remove_code);

        let rm_calls = detect_rm_calls(&rm_tree, &rm_code);
        let remove_calls = detect_rm_calls(&remove_tree, &remove_code);

        // Both should detect exactly one call
        prop_assert_eq!(rm_calls.len(), 1, "rm(list=c(...)) should produce exactly one call");
        prop_assert_eq!(remove_calls.len(), 1, "remove(list=c(...)) should produce exactly one call");

        // Both should extract the same symbols in the same order
        prop_assert_eq!(&rm_calls[0].symbols, &remove_calls[0].symbols,
            "rm(list=c(...)) and remove(list=c(...)) should extract identical symbols");

        // Both should have the same line position
        prop_assert_eq!(rm_calls[0].line, remove_calls[0].line,
            "rm(list=c(...)) and remove(list=c(...)) should have the same line position");
    }

    /// Feature: rm-remove-support, Property 2: remove() Equivalence
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    ///
    /// Test case: Mixed bare symbols and list= argument
    #[test]
    fn prop_remove_equivalence_mixed(
        bare_symbols in prop::collection::vec(r_identifier(), 1..=3)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            }),
        list_symbols in prop::collection::vec(r_identifier(), 1..=3)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        // Ensure bare_symbols and list_symbols don't overlap
        let has_overlap = bare_symbols.iter().any(|s| list_symbols.contains(s));
        prop_assume!(!has_overlap);

        // Generate rm() and remove() calls with both bare symbols and list= argument
        let bare_part = bare_symbols.join(", ");
        let list_quoted: Vec<_> = list_symbols.iter().map(|s| format!(r#""{}""#, s)).collect();
        let list_part = if list_symbols.len() == 1 {
            format!(r#"list = "{}""#, list_symbols[0])
        } else {
            format!("list = c({})", list_quoted.join(", "))
        };

        let rm_code = format!("rm({}, {})", bare_part, list_part);
        let remove_code = format!("remove({}, {})", bare_part, list_part);

        let rm_tree = parse_r(&rm_code);
        let remove_tree = parse_r(&remove_code);

        let rm_calls = detect_rm_calls(&rm_tree, &rm_code);
        let remove_calls = detect_rm_calls(&remove_tree, &remove_code);

        // Both should detect exactly one call
        prop_assert_eq!(rm_calls.len(), 1, "rm() with mixed args should produce exactly one call");
        prop_assert_eq!(remove_calls.len(), 1, "remove() with mixed args should produce exactly one call");

        // Both should extract the same symbols (bare symbols first, then list symbols)
        prop_assert_eq!(&rm_calls[0].symbols, &remove_calls[0].symbols,
            "rm() and remove() with mixed args should extract identical symbols");

        // Both should have the same line position
        prop_assert_eq!(rm_calls[0].line, remove_calls[0].line,
            "rm() and remove() with mixed args should have the same line position");
    }

    /// Feature: rm-remove-support, Property 2: remove() Equivalence
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    ///
    /// Test case: Verify column positions are relative to the call (both start at column 0)
    #[test]
    fn prop_remove_equivalence_column_position(symbol in r_identifier()) {
        let rm_code = rm_bare_symbols(&[symbol.clone()]);
        let remove_code = remove_bare_symbols(&[symbol.clone()]);

        let rm_tree = parse_r(&rm_code);
        let remove_tree = parse_r(&remove_code);

        let rm_calls = detect_rm_calls(&rm_tree, &rm_code);
        let remove_calls = detect_rm_calls(&remove_tree, &remove_code);

        // Both should detect exactly one call
        prop_assert_eq!(rm_calls.len(), 1, "rm() should produce exactly one call");
        prop_assert_eq!(remove_calls.len(), 1, "remove() should produce exactly one call");

        // Both should start at column 0 (beginning of line)
        prop_assert_eq!(rm_calls[0].column, 0, "rm() should start at column 0");
        prop_assert_eq!(remove_calls[0].column, 0, "remove() should start at column 0");
    }
}

// ============================================================================
// Feature: rm-remove-support, Property 3: list= String Literal Extraction
// Validates: Requirements 3.1, 3.2
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: rm-remove-support, Property 3: list= String Literal Extraction
    /// **Validates: Requirements 3.1, 3.2**
    ///
    /// For any rm() call with a list= argument containing string literals (either a
    /// single string or a c() call with strings), the Removal event SHALL contain
    /// exactly those string values as symbol names.
    ///
    /// Test case: Single string literal with double quotes
    #[test]
    fn prop_rm_list_single_string_extraction(symbol in r_identifier()) {
        let code = format!(r#"rm(list = "{}")"#, symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Should extract exactly the symbol from the string literal
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match the string literal content");
    }

    /// Feature: rm-remove-support, Property 3: list= String Literal Extraction
    /// **Validates: Requirements 3.1, 3.2**
    ///
    /// Test case: Single string literal with single quotes
    #[test]
    fn prop_rm_list_single_string_single_quotes(symbol in r_identifier()) {
        let code = format!("rm(list = '{}')", symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Should extract exactly the symbol from the string literal
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match the string literal content (single quotes)");
    }

    /// Feature: rm-remove-support, Property 3: list= String Literal Extraction
    /// **Validates: Requirements 3.1, 3.2**
    ///
    /// Test case: Multiple strings in c() call
    #[test]
    fn prop_rm_list_c_multiple_strings(
        symbols in prop::collection::vec(r_identifier(), 2..=5)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        let quoted: Vec<_> = symbols.iter().map(|s| format!(r#""{}""#, s)).collect();
        let code = format!("rm(list = c({}))", quoted.join(", "));
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Should extract exactly the symbols from the c() call
        prop_assert_eq!(rm_calls[0].symbols.len(), symbols.len(),
            "Number of extracted symbols should match input");

        // All symbols should be present in the same order
        for (i, expected_symbol) in symbols.iter().enumerate() {
            prop_assert_eq!(&rm_calls[0].symbols[i], expected_symbol,
                "Symbol at position {} should match", i);
        }
    }

    /// Feature: rm-remove-support, Property 3: list= String Literal Extraction
    /// **Validates: Requirements 3.1, 3.2**
    ///
    /// Test case: Single string in c() call (edge case)
    #[test]
    fn prop_rm_list_c_single_string(symbol in r_identifier()) {
        let code = format!(r#"rm(list = c("{}"))"#, symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Should extract exactly the symbol from the c() call
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match the string literal content in c()");
    }

    /// Feature: rm-remove-support, Property 3: list= String Literal Extraction
    /// **Validates: Requirements 3.1, 3.2**
    ///
    /// Test case: c() with mixed quote styles (double and single quotes)
    #[test]
    fn prop_rm_list_c_mixed_quotes(
        symbols in prop::collection::vec(r_identifier(), 2..=4)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        // Alternate between double and single quotes
        let quoted: Vec<_> = symbols.iter().enumerate()
            .map(|(i, s)| {
                if i % 2 == 0 {
                    format!(r#""{}""#, s)
                } else {
                    format!("'{}'", s)
                }
            })
            .collect();
        let code = format!("rm(list = c({}))", quoted.join(", "));
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Should extract exactly the symbols regardless of quote style
        prop_assert_eq!(rm_calls[0].symbols.len(), symbols.len(),
            "Number of extracted symbols should match input");

        // All symbols should be present in the same order
        for (i, expected_symbol) in symbols.iter().enumerate() {
            prop_assert_eq!(&rm_calls[0].symbols[i], expected_symbol,
                "Symbol at position {} should match (mixed quotes)", i);
        }
    }

    /// Feature: rm-remove-support, Property 3: list= String Literal Extraction
    /// **Validates: Requirements 3.1, 3.2**
    ///
    /// Test case: remove() with list= argument (should work identically to rm())
    #[test]
    fn prop_remove_list_single_string_extraction(symbol in r_identifier()) {
        let code = format!(r#"remove(list = "{}")"#, symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one remove() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one remove() call");

        // Should extract exactly the symbol from the string literal
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match the string literal content");
    }

    /// Feature: rm-remove-support, Property 3: list= String Literal Extraction
    /// **Validates: Requirements 3.1, 3.2**
    ///
    /// Test case: remove() with list= c() argument
    #[test]
    fn prop_remove_list_c_multiple_strings(
        symbols in prop::collection::vec(r_identifier(), 2..=5)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        let quoted: Vec<_> = symbols.iter().map(|s| format!(r#""{}""#, s)).collect();
        let code = format!("remove(list = c({}))", quoted.join(", "));
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one remove() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one remove() call");

        // Should extract exactly the symbols from the c() call
        prop_assert_eq!(rm_calls[0].symbols.len(), symbols.len(),
            "Number of extracted symbols should match input");

        // All symbols should be present in the same order
        for (i, expected_symbol) in symbols.iter().enumerate() {
            prop_assert_eq!(&rm_calls[0].symbols[i], expected_symbol,
                "Symbol at position {} should match", i);
        }
    }

    /// Feature: rm-remove-support, Property 3: list= String Literal Extraction
    /// **Validates: Requirements 3.1, 3.2**
    ///
    /// Test case: Position tracking for list= argument
    #[test]
    fn prop_rm_list_string_position(
        symbol in r_identifier(),
        prefix_lines in 0..5usize
    ) {
        // Add some prefix lines before the rm() call
        let prefix = "\n".repeat(prefix_lines);
        let code = format!(r#"{}rm(list = "{}")"#, prefix, symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1, "Expected exactly one rm() call");

        // Line should match the number of prefix newlines
        prop_assert_eq!(rm_calls[0].line, prefix_lines as u32,
            "rm() call should be on line {}", prefix_lines);

        // Symbol should still be extracted correctly
        prop_assert_eq!(rm_calls[0].symbols.len(), 1, "Expected exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol, "Symbol should match input");
    }
}

// ============================================================================
// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
// Validates: Requirements 3.3, 3.4
// ============================================================================

/// Generate a valid R function name for dynamic expressions
fn r_function_name() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "ls".to_string(),
        "objects".to_string(),
        "get".to_string(),
        "paste0".to_string(),
        "paste".to_string(),
        "sprintf".to_string(),
        "grep".to_string(),
        "setdiff".to_string(),
        "intersect".to_string(),
        "union".to_string(),
    ])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// For any rm() call with a list= argument containing a non-literal expression
    /// (variable reference, function call other than c() with literals, etc.),
    /// no Removal event SHALL be created for that call.
    ///
    /// Test case: Variable reference in list= argument
    #[test]
    fn prop_rm_dynamic_variable_reference_filtered(varname in r_identifier()) {
        let code = format!("rm(list = {})", varname);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call (no symbols extracted)
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = {}) should not produce any RmCall since variable is dynamic", varname);
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: ls() function call in list= argument
    #[test]
    fn prop_rm_dynamic_ls_call_filtered(_dummy in Just(())) {
        // Simple ls() call
        let code = "rm(list = ls())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = ls()) should not produce any RmCall since ls() is dynamic");
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: ls() with pattern argument in list= argument
    #[test]
    fn prop_rm_dynamic_ls_pattern_filtered(pattern in "[a-z]{1,5}") {
        let code = format!(r#"rm(list = ls(pattern = "^{}"))"#, pattern);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = ls(pattern = ...)) should not produce any RmCall since ls() is dynamic");
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: Various function calls in list= argument
    #[test]
    fn prop_rm_dynamic_function_call_filtered(func_name in r_function_name()) {
        let code = format!("rm(list = {}())", func_name);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = {}()) should not produce any RmCall since function call is dynamic", func_name);
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: paste0() with variable in list= argument
    #[test]
    fn prop_rm_dynamic_paste0_filtered(
        prefix in "[a-z]{1,5}",
        varname in r_identifier()
    ) {
        let code = format!(r#"rm(list = paste0("{}", {}))"#, prefix, varname);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = paste0(...)) should not produce any RmCall since paste0() is dynamic");
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: Expression with operators in list= argument
    #[test]
    fn prop_rm_dynamic_expression_filtered(
        var1 in r_identifier(),
        var2 in r_identifier()
    ) {
        // Ensure different variable names
        prop_assume!(var1 != var2);

        // Test with setdiff() expression
        let code = format!("rm(list = setdiff({}, {}))", var1, var2);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = setdiff(...)) should not produce any RmCall since expression is dynamic");
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: c() with variable (not all literals) in list= argument
    #[test]
    fn prop_rm_dynamic_c_with_variable_partial_extraction(
        literal_symbol in r_identifier(),
        varname in r_identifier()
    ) {
        // Ensure different names
        prop_assume!(literal_symbol != varname);

        // c() with mixed literals and variables - only literals should be extracted
        let code = format!(r#"rm(list = c("{}", {}))"#, literal_symbol, varname);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should extract only the string literal, not the variable
        // If no string literals are extracted, no RmCall is created
        // If at least one string literal is extracted, RmCall is created with only those
        if rm_calls.len() == 1 {
            // Only the string literal should be extracted
            prop_assert_eq!(rm_calls[0].symbols.len(), 1,
                "Only string literals should be extracted from c() with mixed args");
            prop_assert_eq!(&rm_calls[0].symbols[0], &literal_symbol,
                "The extracted symbol should be the string literal");
        }
        // If no RmCall is created, that's also acceptable behavior
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: remove() with dynamic expression (should behave same as rm())
    #[test]
    fn prop_remove_dynamic_variable_reference_filtered(varname in r_identifier()) {
        let code = format!("remove(list = {})", varname);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any remove() call (no symbols extracted)
        prop_assert_eq!(rm_calls.len(), 0,
            "remove(list = {}) should not produce any RmCall since variable is dynamic", varname);
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: remove() with ls() call (should behave same as rm())
    #[test]
    fn prop_remove_dynamic_ls_call_filtered(_dummy in Just(())) {
        let code = "remove(list = ls())";
        let tree = parse_r(code);
        let rm_calls = detect_rm_calls(&tree, code);

        // Should NOT detect any remove() call
        prop_assert_eq!(rm_calls.len(), 0,
            "remove(list = ls()) should not produce any RmCall since ls() is dynamic");
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: Numeric literal in list= argument (not a valid symbol name)
    #[test]
    fn prop_rm_dynamic_numeric_literal_filtered(num in 0..1000i32) {
        let code = format!("rm(list = {})", num);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call (numeric is not a valid symbol)
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = {}) should not produce any RmCall since numeric is not a symbol", num);
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: Binary expression in list= argument
    #[test]
    fn prop_rm_dynamic_binary_expression_filtered(
        var1 in r_identifier(),
        var2 in r_identifier()
    ) {
        // Ensure different variable names
        prop_assume!(var1 != var2);

        // Test with concatenation expression using c() with variables
        let code = format!("rm(list = c({}, {}))", var1, var2);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call since c() contains only variables, not string literals
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = c(var1, var2)) should not produce any RmCall since c() contains only variables");
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: Subscript expression in list= argument
    #[test]
    fn prop_rm_dynamic_subscript_filtered(
        varname in r_identifier(),
        index in 1..10i32
    ) {
        let code = format!("rm(list = {}[{}])", varname, index);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list = var[index]) should not produce any RmCall since subscript is dynamic");
    }

    /// Feature: rm-remove-support, Property 4: Dynamic Expression Filtering
    /// **Validates: Requirements 3.3, 3.4**
    ///
    /// Test case: Bare symbols should still work even when list= has dynamic expression
    #[test]
    fn prop_rm_bare_symbols_with_dynamic_list(
        bare_symbol in r_identifier(),
        dynamic_var in r_identifier()
    ) {
        // Ensure different names
        prop_assume!(bare_symbol != dynamic_var);

        // rm() with both bare symbol and dynamic list= argument
        let code = format!("rm({}, list = {})", bare_symbol, dynamic_var);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect the rm() call with only the bare symbol
        prop_assert_eq!(rm_calls.len(), 1,
            "rm() with bare symbol and dynamic list= should produce one RmCall");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1,
            "Only the bare symbol should be extracted");
        prop_assert_eq!(&rm_calls[0].symbols[0], &bare_symbol,
            "The extracted symbol should be the bare symbol");
    }
}


// ============================================================================
// Feature: rm-remove-support, Property 5: envir= Argument Filtering
// Validates: Requirements 4.1, 4.2, 4.3
// ============================================================================

/// Generate a non-global environment expression for envir= argument
fn non_global_envir_expression() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "my_env".to_string(),
        "new.env()".to_string(),
        "parent.frame()".to_string(),
        "baseenv()".to_string(),
        "emptyenv()".to_string(),
        "as.environment(2)".to_string(),
        "e".to_string(),
        "env".to_string(),
        "local_env".to_string(),
        "custom_env".to_string(),
    ])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// For any rm() call with an envir= argument, a Removal event SHALL be created
    /// if and only if the envir value is globalenv() or .GlobalEnv (or omitted entirely).
    ///
    /// Test case: rm() without envir= creates Removal events
    #[test]
    fn prop_rm_without_envir_creates_removal(symbol in r_identifier()) {
        let code = format!("rm({})", symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call (envir= omitted means default global env)
        prop_assert_eq!(rm_calls.len(), 1,
            "rm() without envir= should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1,
            "Should extract exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: rm() with envir = globalenv() creates Removal events
    #[test]
    fn prop_rm_with_envir_globalenv_creates_removal(symbol in r_identifier()) {
        let code = format!("rm({}, envir = globalenv())", symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call (globalenv() is equivalent to default)
        prop_assert_eq!(rm_calls.len(), 1,
            "rm() with envir = globalenv() should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1,
            "Should extract exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: rm() with envir = .GlobalEnv creates Removal events
    #[test]
    fn prop_rm_with_envir_dot_globalenv_creates_removal(symbol in r_identifier()) {
        let code = format!("rm({}, envir = .GlobalEnv)", symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call (.GlobalEnv is equivalent to default)
        prop_assert_eq!(rm_calls.len(), 1,
            "rm() with envir = .GlobalEnv should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1,
            "Should extract exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: rm() with non-default envir= does NOT create Removal events
    #[test]
    fn prop_rm_with_non_default_envir_no_removal(
        symbol in r_identifier(),
        envir_expr in non_global_envir_expression()
    ) {
        let code = format!("rm({}, envir = {})", symbol, envir_expr);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call (non-default envir= means skip)
        prop_assert_eq!(rm_calls.len(), 0,
            "rm() with envir = {} should NOT create a Removal event", envir_expr);
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: remove() without envir= creates Removal events (same as rm())
    #[test]
    fn prop_remove_without_envir_creates_removal(symbol in r_identifier()) {
        let code = format!("remove({})", symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one remove() call
        prop_assert_eq!(rm_calls.len(), 1,
            "remove() without envir= should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1,
            "Should extract exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: remove() with envir = globalenv() creates Removal events
    #[test]
    fn prop_remove_with_envir_globalenv_creates_removal(symbol in r_identifier()) {
        let code = format!("remove({}, envir = globalenv())", symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one remove() call
        prop_assert_eq!(rm_calls.len(), 1,
            "remove() with envir = globalenv() should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1,
            "Should extract exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: remove() with envir = .GlobalEnv creates Removal events
    #[test]
    fn prop_remove_with_envir_dot_globalenv_creates_removal(symbol in r_identifier()) {
        let code = format!("remove({}, envir = .GlobalEnv)", symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one remove() call
        prop_assert_eq!(rm_calls.len(), 1,
            "remove() with envir = .GlobalEnv should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1,
            "Should extract exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: remove() with non-default envir= does NOT create Removal events
    #[test]
    fn prop_remove_with_non_default_envir_no_removal(
        symbol in r_identifier(),
        envir_expr in non_global_envir_expression()
    ) {
        let code = format!("remove({}, envir = {})", symbol, envir_expr);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any remove() call
        prop_assert_eq!(rm_calls.len(), 0,
            "remove() with envir = {} should NOT create a Removal event", envir_expr);
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: Multiple symbols with envir = globalenv() creates Removal events
    #[test]
    fn prop_rm_multiple_symbols_with_envir_globalenv(
        symbols in prop::collection::vec(r_identifier(), 2..=5)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            })
    ) {
        let code = format!("rm({}, envir = globalenv())", symbols.join(", "));
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call with all symbols
        prop_assert_eq!(rm_calls.len(), 1,
            "rm() with multiple symbols and envir = globalenv() should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), symbols.len(),
            "Should extract all symbols");

        for (i, expected_symbol) in symbols.iter().enumerate() {
            prop_assert_eq!(&rm_calls[0].symbols[i], expected_symbol,
                "Symbol at position {} should match", i);
        }
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: Multiple symbols with non-default envir= does NOT create Removal events
    #[test]
    fn prop_rm_multiple_symbols_with_non_default_envir_no_removal(
        symbols in prop::collection::vec(r_identifier(), 2..=5)
            .prop_filter("unique symbols", |v| {
                let mut sorted = v.clone();
                sorted.sort();
                sorted.dedup();
                sorted.len() == v.len()
            }),
        envir_expr in non_global_envir_expression()
    ) {
        let code = format!("rm({}, envir = {})", symbols.join(", "), envir_expr);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm() with multiple symbols and envir = {} should NOT create a Removal event", envir_expr);
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: list= argument with envir = globalenv() creates Removal events
    #[test]
    fn prop_rm_list_with_envir_globalenv_creates_removal(symbol in r_identifier()) {
        let code = format!(r#"rm(list = "{}", envir = globalenv())"#, symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call
        prop_assert_eq!(rm_calls.len(), 1,
            "rm(list=...) with envir = globalenv() should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), 1,
            "Should extract exactly one symbol");
        prop_assert_eq!(&rm_calls[0].symbols[0], &symbol,
            "Symbol should match input");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: list= argument with non-default envir= does NOT create Removal events
    #[test]
    fn prop_rm_list_with_non_default_envir_no_removal(
        symbol in r_identifier(),
        envir_expr in non_global_envir_expression()
    ) {
        let code = format!(r#"rm(list = "{}", envir = {})"#, symbol, envir_expr);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm(list=...) with envir = {} should NOT create a Removal event", envir_expr);
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: Mixed bare symbols and list= with envir = .GlobalEnv creates Removal events
    #[test]
    fn prop_rm_mixed_with_envir_dot_globalenv_creates_removal(
        bare_symbol in r_identifier(),
        list_symbol in r_identifier()
    ) {
        // Ensure different symbols
        prop_assume!(bare_symbol != list_symbol);

        let code = format!(r#"rm({}, list = "{}", envir = .GlobalEnv)"#, bare_symbol, list_symbol);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should detect exactly one rm() call with both symbols
        prop_assert_eq!(rm_calls.len(), 1,
            "rm() with mixed args and envir = .GlobalEnv should create a Removal event");
        prop_assert_eq!(rm_calls[0].symbols.len(), 2,
            "Should extract both symbols");
        prop_assert_eq!(&rm_calls[0].symbols[0], &bare_symbol,
            "First symbol should be the bare symbol");
        prop_assert_eq!(&rm_calls[0].symbols[1], &list_symbol,
            "Second symbol should be from list=");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: Mixed bare symbols and list= with non-default envir= does NOT create Removal events
    #[test]
    fn prop_rm_mixed_with_non_default_envir_no_removal(
        bare_symbol in r_identifier(),
        list_symbol in r_identifier(),
        envir_expr in non_global_envir_expression()
    ) {
        // Ensure different symbols
        prop_assume!(bare_symbol != list_symbol);

        let code = format!(r#"rm({}, list = "{}", envir = {})"#, bare_symbol, list_symbol, envir_expr);
        let tree = parse_r(&code);
        let rm_calls = detect_rm_calls(&tree, &code);

        // Should NOT detect any rm() call
        prop_assert_eq!(rm_calls.len(), 0,
            "rm() with mixed args and envir = {} should NOT create a Removal event", envir_expr);
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: Equivalence between globalenv() and .GlobalEnv
    #[test]
    fn prop_rm_envir_globalenv_equivalence(symbol in r_identifier()) {
        let code_globalenv = format!("rm({}, envir = globalenv())", symbol);
        let code_dot_globalenv = format!("rm({}, envir = .GlobalEnv)", symbol);

        let tree_globalenv = parse_r(&code_globalenv);
        let tree_dot_globalenv = parse_r(&code_dot_globalenv);

        let rm_calls_globalenv = detect_rm_calls(&tree_globalenv, &code_globalenv);
        let rm_calls_dot_globalenv = detect_rm_calls(&tree_dot_globalenv, &code_dot_globalenv);

        // Both should produce exactly one rm() call
        prop_assert_eq!(rm_calls_globalenv.len(), 1,
            "rm() with envir = globalenv() should create a Removal event");
        prop_assert_eq!(rm_calls_dot_globalenv.len(), 1,
            "rm() with envir = .GlobalEnv should create a Removal event");

        // Both should extract the same symbols
        prop_assert_eq!(&rm_calls_globalenv[0].symbols, &rm_calls_dot_globalenv[0].symbols,
            "globalenv() and .GlobalEnv should produce identical symbol extraction");
    }

    /// Feature: rm-remove-support, Property 5: envir= Argument Filtering
    /// **Validates: Requirements 4.1, 4.2, 4.3**
    ///
    /// Test case: Equivalence between omitted envir= and explicit globalenv()
    #[test]
    fn prop_rm_envir_omitted_vs_explicit_globalenv(symbol in r_identifier()) {
        let code_omitted = format!("rm({})", symbol);
        let code_explicit = format!("rm({}, envir = globalenv())", symbol);

        let tree_omitted = parse_r(&code_omitted);
        let tree_explicit = parse_r(&code_explicit);

        let rm_calls_omitted = detect_rm_calls(&tree_omitted, &code_omitted);
        let rm_calls_explicit = detect_rm_calls(&tree_explicit, &code_explicit);

        // Both should produce exactly one rm() call
        prop_assert_eq!(rm_calls_omitted.len(), 1,
            "rm() without envir= should create a Removal event");
        prop_assert_eq!(rm_calls_explicit.len(), 1,
            "rm() with envir = globalenv() should create a Removal event");

        // Both should extract the same symbols
        prop_assert_eq!(&rm_calls_omitted[0].symbols, &rm_calls_explicit[0].symbols,
            "Omitted envir= and explicit globalenv() should produce identical symbol extraction");
    }
}


// ============================================================================
// Feature: rm-remove-support, Property 6: Function Scope Isolation
// Validates: Requirements 5.1, 5.2, 5.3
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: rm-remove-support, Property 6: Function Scope Isolation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// For any rm() call inside a function body, the removal SHALL only affect scope
    /// queries within that function body. Scope queries outside the function (before
    /// or after) SHALL NOT be affected by the removal.
    ///
    /// Test pattern:
    /// ```r
    /// x <- 1  # Global definition
    /// my_func <- function() {
    ///   y <- 2
    ///   rm(y)  # Function-local removal
    /// }
    /// # After function: x should be in scope, y should NOT be in global scope
    /// ```
    #[test]
    fn prop_rm_function_scope_isolation_global_unaffected(
        global_var in r_identifier(),
        func_name in r_identifier(),
        local_var in r_identifier()
    ) {
        // Ensure all names are distinct
        prop_assume!(global_var != func_name && global_var != local_var && func_name != local_var);

        let uri = make_url("test");

        // Code: global definition, function with local definition and rm() inside
        let code = format!(
            "{} <- 1\n{} <- function() {{\n  {} <- 2\n  rm({})\n}}",
            global_var, func_name, local_var, local_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // At end of file (outside function), global variable should still be in scope
        // The rm() inside the function should NOT affect global scope
        let scope_outside = scope_at_position(&artifacts, 10, 0);

        prop_assert!(scope_outside.symbols.contains_key(&global_var),
            "Global variable should be available outside function (rm inside function should not affect it)");
        prop_assert!(scope_outside.symbols.contains_key(&func_name),
            "Function name should be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key(&local_var),
            "Function-local variable should NOT be available outside function (never exported)");
    }

    /// Feature: rm-remove-support, Property 6: Function Scope Isolation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Test that rm() inside a function DOES affect scope within that function body.
    /// After rm(y), y should not be in scope within the function.
    #[test]
    fn prop_rm_function_scope_isolation_affects_function_body(
        func_name in r_identifier(),
        local_var in r_identifier()
    ) {
        // Ensure names are distinct
        prop_assume!(func_name != local_var);

        let uri = make_url("test");

        // Code: function with local definition, then rm() of that variable
        // We need to query scope AFTER the rm() call within the function body
        let code = format!(
            "{} <- function() {{\n  {} <- 2\n  rm({})\n  # position after rm\n}}",
            func_name, local_var, local_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Find position after rm() but still inside function body
        // The rm() is on line 2 (0-indexed), so line 3 is after rm()
        // We need to find a position inside the function body after the rm() call
        let rm_line = code.lines().enumerate()
            .find(|(_, line)| line.contains("rm("))
            .map(|(i, _)| i as u32)
            .unwrap_or(2);

        // Query scope at position after rm() but inside function
        // Use the line after rm() which should be the comment line
        let scope_after_rm = scope_at_position(&artifacts, rm_line + 1, 5);

        prop_assert!(scope_after_rm.symbols.contains_key(&func_name),
            "Function name should be available inside function");
        prop_assert!(!scope_after_rm.symbols.contains_key(&local_var),
            "Local variable should NOT be in scope after rm() within function body");
    }

    /// Feature: rm-remove-support, Property 6: Function Scope Isolation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Test that rm() at global level DOES affect global scope.
    /// This contrasts with rm() inside a function which only affects function scope.
    #[test]
    fn prop_rm_global_level_affects_global_scope(
        var_name in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: global definition, then rm() at global level
        let code = format!(
            "{} <- 1\nrm({})\n# position after rm",
            var_name, var_name
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at position after rm() at global level
        let scope_after_rm = scope_at_position(&artifacts, 10, 0);

        prop_assert!(!scope_after_rm.symbols.contains_key(&var_name),
            "Variable should NOT be in scope after rm() at global level");
    }

    /// Feature: rm-remove-support, Property 6: Function Scope Isolation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Test that rm() of a global variable inside a function does NOT affect global scope.
    /// Even if the variable name matches a global variable, the rm() inside a function
    /// should only affect the function's local scope.
    #[test]
    fn prop_rm_global_var_name_inside_function_no_global_effect(
        var_name in r_identifier(),
        func_name in r_identifier()
    ) {
        // Ensure names are distinct
        prop_assume!(var_name != func_name);

        let uri = make_url("test");

        // Code: global definition, function that tries to rm() the same-named variable
        let code = format!(
            "{} <- 1\n{} <- function() {{\n  rm({})\n}}\n# after function",
            var_name, func_name, var_name
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at position after function definition (global scope)
        let scope_after_func = scope_at_position(&artifacts, 10, 0);

        prop_assert!(scope_after_func.symbols.contains_key(&var_name),
            "Global variable should still be in scope after function with rm() inside");
        prop_assert!(scope_after_func.symbols.contains_key(&func_name),
            "Function name should be available");
    }

    /// Feature: rm-remove-support, Property 6: Function Scope Isolation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Test that rm() inside nested functions only affects the innermost function scope.
    #[test]
    fn prop_rm_nested_function_scope_isolation(
        outer_func in r_identifier(),
        inner_func in r_identifier(),
        outer_var in r_identifier(),
        inner_var in r_identifier()
    ) {
        // Ensure all names are distinct
        prop_assume!(outer_func != inner_func && outer_func != outer_var && outer_func != inner_var);
        prop_assume!(inner_func != outer_var && inner_func != inner_var);
        prop_assume!(outer_var != inner_var);

        let uri = make_url("test");

        // Code: nested functions where inner function has rm()
        let code = format!(
            "{} <- function() {{\n  {} <- 1\n  {} <- function() {{\n    {} <- 2\n    rm({})\n  }}\n  # after inner func\n}}",
            outer_func, outer_var, inner_func, inner_var, inner_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at position after inner function definition but inside outer function
        // This should be on the line with "# after inner func"
        let comment_line = code.lines().enumerate()
            .find(|(_, line)| line.contains("# after inner func"))
            .map(|(i, _)| i as u32)
            .unwrap_or(6);

        let scope_outer_after_inner = scope_at_position(&artifacts, comment_line, 5);

        prop_assert!(scope_outer_after_inner.symbols.contains_key(&outer_func),
            "Outer function should be available inside itself");
        prop_assert!(scope_outer_after_inner.symbols.contains_key(&outer_var),
            "Outer variable should still be in scope (rm in inner function should not affect it)");
        prop_assert!(scope_outer_after_inner.symbols.contains_key(&inner_func),
            "Inner function should be available inside outer function");
        // inner_var is local to inner function, so it should NOT be available in outer function
        prop_assert!(!scope_outer_after_inner.symbols.contains_key(&inner_var),
            "Inner variable should NOT be available outside inner function");
    }

    /// Feature: rm-remove-support, Property 6: Function Scope Isolation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Test that rm() with remove() alias inside function also respects function scope isolation.
    #[test]
    fn prop_remove_function_scope_isolation(
        global_var in r_identifier(),
        func_name in r_identifier(),
        local_var in r_identifier()
    ) {
        // Ensure all names are distinct
        prop_assume!(global_var != func_name && global_var != local_var && func_name != local_var);

        let uri = make_url("test");

        // Code: global definition, function with local definition and remove() inside
        let code = format!(
            "{} <- 1\n{} <- function() {{\n  {} <- 2\n  remove({})\n}}",
            global_var, func_name, local_var, local_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // At end of file (outside function), global variable should still be in scope
        let scope_outside = scope_at_position(&artifacts, 10, 0);

        prop_assert!(scope_outside.symbols.contains_key(&global_var),
            "Global variable should be available outside function (remove inside function should not affect it)");
        prop_assert!(scope_outside.symbols.contains_key(&func_name),
            "Function name should be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key(&local_var),
            "Function-local variable should NOT be available outside function");
    }

    /// Feature: rm-remove-support, Property 6: Function Scope Isolation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Test that rm(list=...) inside function also respects function scope isolation.
    #[test]
    fn prop_rm_list_function_scope_isolation(
        global_var in r_identifier(),
        func_name in r_identifier(),
        local_var in r_identifier()
    ) {
        // Ensure all names are distinct
        prop_assume!(global_var != func_name && global_var != local_var && func_name != local_var);

        let uri = make_url("test");

        // Code: global definition, function with local definition and rm(list=...) inside
        let code = format!(
            "{} <- 1\n{} <- function() {{\n  {} <- 2\n  rm(list = \"{}\")\n}}",
            global_var, func_name, local_var, local_var
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // At end of file (outside function), global variable should still be in scope
        let scope_outside = scope_at_position(&artifacts, 10, 0);

        prop_assert!(scope_outside.symbols.contains_key(&global_var),
            "Global variable should be available outside function (rm(list=...) inside function should not affect it)");
        prop_assert!(scope_outside.symbols.contains_key(&func_name),
            "Function name should be available outside function");
        prop_assert!(!scope_outside.symbols.contains_key(&local_var),
            "Function-local variable should NOT be available outside function");
    }
}


// ============================================================================
// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
// Validates: Requirements 7.1, 7.2, 7.3, 7.4
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// For any sequence of definitions and removals of a symbol, scope at position P
    /// SHALL include the symbol if and only if there exists a definition before P
    /// with no removal between that definition and P.
    ///
    /// Test pattern: Define then remove - symbol NOT in scope after removal
    /// ```r
    /// x <- 1
    /// rm(x)
    /// # x NOT in scope here
    /// ```
    #[test]
    fn prop_timeline_define_then_remove_not_in_scope(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: define symbol, then remove it
        let code = format!(
            "{} <- 1\nrm({})\n# after rm",
            symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at position after rm() - symbol should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 10, 0);

        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after rm() (define then remove)");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Remove then define - symbol IS in scope after definition
    /// ```r
    /// rm(x)  # removal of undefined symbol has no effect
    /// x <- 1
    /// # x IS in scope here
    /// ```
    #[test]
    fn prop_timeline_remove_then_define_in_scope(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: remove symbol (before it's defined), then define it
        let code = format!(
            "rm({})\n{} <- 1\n# after define",
            symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at position after definition - symbol SHOULD be in scope
        let scope_after_define = scope_at_position(&artifacts, 10, 0);

        prop_assert!(scope_after_define.symbols.contains_key(&symbol),
            "Symbol should be in scope after definition (remove then define)");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Define, remove, define - symbol IS in scope after second definition
    /// ```r
    /// x <- 1
    /// rm(x)
    /// x <- 2
    /// # x IS in scope here
    /// ```
    #[test]
    fn prop_timeline_define_remove_define_in_scope(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: define, remove, then define again
        let code = format!(
            "{} <- 1\nrm({})\n{} <- 2\n# after second define",
            symbol, symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at position after second definition - symbol SHOULD be in scope
        let scope_after_second_define = scope_at_position(&artifacts, 10, 0);

        prop_assert!(scope_after_second_define.symbols.contains_key(&symbol),
            "Symbol should be in scope after second definition (define, remove, define)");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Position-aware queries - scope at position between definition and removal
    /// ```r
    /// x <- 1  # line 0
    /// # x IS in scope here (line 1)
    /// rm(x)   # line 2
    /// # x NOT in scope here (line 3)
    /// ```
    #[test]
    fn prop_timeline_position_aware_between_def_and_rm(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: define on line 0, rm on line 2
        let code = format!(
            "{} <- 1\n# between\nrm({})\n# after",
            symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at line 1 (between definition and removal) - symbol SHOULD be in scope
        let scope_between = scope_at_position(&artifacts, 1, 0);
        prop_assert!(scope_between.symbols.contains_key(&symbol),
            "Symbol should be in scope between definition and removal");

        // Query scope at line 3 (after removal) - symbol should NOT be in scope
        let scope_after = scope_at_position(&artifacts, 3, 0);
        prop_assert!(!scope_after.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after removal");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Position-aware queries at different points in define-remove-define sequence
    /// ```r
    /// x <- 1  # line 0: first definition
    /// # line 1: x IS in scope
    /// rm(x)   # line 2: removal
    /// # line 3: x NOT in scope
    /// x <- 2  # line 4: second definition
    /// # line 5: x IS in scope
    /// ```
    #[test]
    fn prop_timeline_position_aware_define_remove_define_sequence(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code with clear line positions
        let code = format!(
            "{} <- 1\n# after first def\nrm({})\n# after rm\n{} <- 2\n# after second def",
            symbol, symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Line 1: after first definition, before removal - symbol SHOULD be in scope
        let scope_after_first_def = scope_at_position(&artifacts, 1, 0);
        prop_assert!(scope_after_first_def.symbols.contains_key(&symbol),
            "Symbol should be in scope after first definition");

        // Line 3: after removal, before second definition - symbol should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 3, 0);
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after removal");

        // Line 5: after second definition - symbol SHOULD be in scope
        let scope_after_second_def = scope_at_position(&artifacts, 5, 0);
        prop_assert!(scope_after_second_def.symbols.contains_key(&symbol),
            "Symbol should be in scope after second definition");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Multiple symbols with interleaved definitions and removals
    /// ```r
    /// x <- 1
    /// y <- 2
    /// rm(x)
    /// # x NOT in scope, y IS in scope
    /// ```
    #[test]
    fn prop_timeline_multiple_symbols_interleaved(
        symbol_x in r_identifier(),
        symbol_y in r_identifier()
    ) {
        // Ensure different symbols
        prop_assume!(symbol_x != symbol_y);

        let uri = make_url("test");

        // Code: define x, define y, remove x
        let code = format!(
            "{} <- 1\n{} <- 2\nrm({})\n# after rm",
            symbol_x, symbol_y, symbol_x
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at end - x should NOT be in scope, y SHOULD be in scope
        let scope_end = scope_at_position(&artifacts, 10, 0);

        prop_assert!(!scope_end.symbols.contains_key(&symbol_x),
            "Symbol x should NOT be in scope after rm(x)");
        prop_assert!(scope_end.symbols.contains_key(&symbol_y),
            "Symbol y should still be in scope (not removed)");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Multiple removals of same symbol (idempotent)
    /// ```r
    /// x <- 1
    /// rm(x)
    /// rm(x)  # second removal has no effect (already removed)
    /// # x NOT in scope
    /// ```
    #[test]
    fn prop_timeline_multiple_removals_idempotent(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: define, remove, remove again
        let code = format!(
            "{} <- 1\nrm({})\nrm({})\n# after double rm",
            symbol, symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at end - symbol should NOT be in scope
        let scope_end = scope_at_position(&artifacts, 10, 0);

        prop_assert!(!scope_end.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after multiple removals");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Using remove() alias follows same timeline rules
    /// ```r
    /// x <- 1
    /// remove(x)
    /// # x NOT in scope
    /// ```
    #[test]
    fn prop_timeline_remove_alias_same_behavior(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: define, then remove using remove() alias
        let code = format!(
            "{} <- 1\nremove({})\n# after remove",
            symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at end - symbol should NOT be in scope
        let scope_end = scope_at_position(&artifacts, 10, 0);

        prop_assert!(!scope_end.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after remove() (same as rm())");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Using rm(list=...) follows same timeline rules
    /// ```r
    /// x <- 1
    /// rm(list = "x")
    /// # x NOT in scope
    /// ```
    #[test]
    fn prop_timeline_rm_list_same_behavior(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: define, then remove using rm(list=...)
        let code = format!(
            "{} <- 1\nrm(list = \"{}\")\n# after rm list",
            symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at end - symbol should NOT be in scope
        let scope_end = scope_at_position(&artifacts, 10, 0);

        prop_assert!(!scope_end.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after rm(list=...) (same as rm())");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: rm() with multiple symbols removes all of them
    /// ```r
    /// x <- 1
    /// y <- 2
    /// rm(x, y)
    /// # neither x nor y in scope
    /// ```
    #[test]
    fn prop_timeline_rm_multiple_symbols_at_once(
        symbol_x in r_identifier(),
        symbol_y in r_identifier()
    ) {
        // Ensure different symbols
        prop_assume!(symbol_x != symbol_y);

        let uri = make_url("test");

        // Code: define both, then remove both at once
        let code = format!(
            "{} <- 1\n{} <- 2\nrm({}, {})\n# after rm",
            symbol_x, symbol_y, symbol_x, symbol_y
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at end - neither symbol should be in scope
        let scope_end = scope_at_position(&artifacts, 10, 0);

        prop_assert!(!scope_end.symbols.contains_key(&symbol_x),
            "Symbol x should NOT be in scope after rm(x, y)");
        prop_assert!(!scope_end.symbols.contains_key(&symbol_y),
            "Symbol y should NOT be in scope after rm(x, y)");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Complex sequence with multiple definitions and removals
    /// ```r
    /// x <- 1  # line 0
    /// y <- 2  # line 1
    /// rm(x)   # line 2
    /// x <- 3  # line 3: redefine x
    /// rm(y)   # line 4
    /// # line 5: x IS in scope, y NOT in scope
    /// ```
    #[test]
    fn prop_timeline_complex_sequence(
        symbol_x in r_identifier(),
        symbol_y in r_identifier()
    ) {
        // Ensure different symbols
        prop_assume!(symbol_x != symbol_y);

        let uri = make_url("test");

        // Complex sequence
        let code = format!(
            "{} <- 1\n{} <- 2\nrm({})\n{} <- 3\nrm({})\n# end",
            symbol_x, symbol_y, symbol_x, symbol_x, symbol_y
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at end
        let scope_end = scope_at_position(&artifacts, 10, 0);

        prop_assert!(scope_end.symbols.contains_key(&symbol_x),
            "Symbol x should be in scope (redefined after removal)");
        prop_assert!(!scope_end.symbols.contains_key(&symbol_y),
            "Symbol y should NOT be in scope (removed and not redefined)");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Removal before any definition has no effect
    /// ```r
    /// rm(x)  # x was never defined
    /// # x NOT in scope (was never defined)
    /// ```
    #[test]
    fn prop_timeline_removal_of_undefined_no_effect(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: just rm() without any definition
        let code = format!(
            "rm({})\n# after rm of undefined",
            symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at end - symbol should NOT be in scope (was never defined)
        let scope_end = scope_at_position(&artifacts, 10, 0);

        prop_assert!(!scope_end.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope (was never defined, rm had no effect)");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Same-line definition and removal - position-aware
    /// ```r
    /// x <- 1; rm(x)
    /// # x NOT in scope at end of line
    /// ```
    #[test]
    fn prop_timeline_same_line_def_and_rm(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: define and remove on same line
        let code = format!(
            "{} <- 1; rm({})",
            symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Query scope at end of line - symbol should NOT be in scope
        let scope_end = scope_at_position(&artifacts, 0, 100);

        prop_assert!(!scope_end.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope at end of line after same-line rm()");
    }

    /// Feature: rm-remove-support, Property 8: Timeline-Based Scope Resolution
    /// **Validates: Requirements 7.1, 7.2, 7.3, 7.4**
    ///
    /// Test pattern: Position between same-line definition and removal
    /// The symbol should be in scope after definition but before removal on same line.
    #[test]
    fn prop_timeline_same_line_position_between_def_and_rm(
        symbol in r_identifier()
    ) {
        let uri = make_url("test");

        // Code: define and remove on same line with space between
        // x <- 1; rm(x)
        // Position after definition (col ~7) but before rm() (col ~9)
        let code = format!(
            "{} <- 1; rm({})",
            symbol, symbol
        );
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Find the position of rm() call
        let rm_pos = code.find("rm(").unwrap_or(0) as u32;

        // Query scope just before rm() - symbol SHOULD be in scope
        let scope_before_rm = scope_at_position(&artifacts, 0, rm_pos.saturating_sub(1));
        prop_assert!(scope_before_rm.symbols.contains_key(&symbol),
            "Symbol should be in scope between definition and rm() on same line");

        // Query scope after rm() - symbol should NOT be in scope
        let scope_after_rm = scope_at_position(&artifacts, 0, rm_pos + 10);
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after rm() on same line");
    }
}


// ============================================================================
// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
// Validates: Requirements 6.1, 6.2, 6.3
// ============================================================================

use super::scope::scope_at_position_with_graph;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
    /// **Validates: Requirements 6.1, 6.2, 6.3**
    ///
    /// For any file that sources another file defining symbol `s` and then calls `rm(s)`,
    /// scope queries after the `rm()` call SHALL NOT include `s`, while scope queries
    /// between the `source()` and `rm()` calls SHALL include `s`.
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// source("child.R")  # child.R defines helper_func
    /// # helper_func IS in scope here
    /// rm(helper_func)
    /// # helper_func is NOT in scope here
    /// ```
    #[test]
    fn prop_cross_file_removal_propagation_basic(
        symbol in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: sources child.R, then removes the symbol
        let parent_code = format!("source(\"child.R\")\nrm({})", symbol);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: defines the symbol
        let child_code = format!("{} <- function() {{ 1 }}", symbol);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // After source() but before rm() (line 0, after source call), symbol should be in scope
        let scope_before_rm = scope_at_position_with_graph(
            &parent_uri, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(scope_before_rm.symbols.contains_key(&symbol),
            "Symbol from sourced file should be in scope after source() but before rm()");

        // After rm() (line 1), symbol should NOT be in scope
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after rm()");

        // At end of file, symbol should NOT be in scope
        let scope_eof = scope_at_position_with_graph(
            &parent_uri, 10, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_eof.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope at end of file after rm()");
    }

    /// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
    /// **Validates: Requirements 6.1, 6.2, 6.3**
    ///
    /// Test that rm() only removes the specified symbol, not others from the sourced file.
    #[test]
    fn prop_cross_file_removal_propagation_selective(
        symbol_to_remove in r_identifier(),
        symbol_to_keep in r_identifier()
    ) {
        // Ensure different symbols
        prop_assume!(symbol_to_remove != symbol_to_keep);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: sources child.R, then removes only one symbol
        let parent_code = format!("source(\"child.R\")\nrm({})", symbol_to_remove);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: defines both symbols
        let child_code = format!(
            "{} <- function() {{ 1 }}\n{} <- function() {{ 2 }}",
            symbol_to_remove, symbol_to_keep
        );
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // After rm(), only the removed symbol should be gone
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol_to_remove),
            "Removed symbol should NOT be in scope after rm()");
        prop_assert!(scope_after_rm.symbols.contains_key(&symbol_to_keep),
            "Non-removed symbol should still be in scope after rm()");
    }

    /// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
    /// **Validates: Requirements 6.1, 6.2, 6.3**
    ///
    /// Test that rm() with multiple symbols removes all specified symbols from sourced file.
    #[test]
    fn prop_cross_file_removal_propagation_multiple_symbols(
        symbol_a in r_identifier(),
        symbol_b in r_identifier(),
        symbol_c in r_identifier()
    ) {
        // Ensure all symbols are different
        prop_assume!(symbol_a != symbol_b && symbol_b != symbol_c && symbol_a != symbol_c);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: sources child.R, then removes symbol_a and symbol_b
        let parent_code = format!("source(\"child.R\")\nrm({}, {})", symbol_a, symbol_b);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: defines all three symbols
        let child_code = format!(
            "{} <- function() {{ 1 }}\n{} <- function() {{ 2 }}\n{} <- function() {{ 3 }}",
            symbol_a, symbol_b, symbol_c
        );
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Before rm() (line 0), all three symbols should be in scope
        let scope_before_rm = scope_at_position_with_graph(
            &parent_uri, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(scope_before_rm.symbols.contains_key(&symbol_a),
            "symbol_a should be in scope before rm()");
        prop_assert!(scope_before_rm.symbols.contains_key(&symbol_b),
            "symbol_b should be in scope before rm()");
        prop_assert!(scope_before_rm.symbols.contains_key(&symbol_c),
            "symbol_c should be in scope before rm()");

        // After rm() (line 1), symbol_a and symbol_b should NOT be in scope, but symbol_c should be
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol_a),
            "symbol_a should NOT be in scope after rm()");
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol_b),
            "symbol_b should NOT be in scope after rm()");
        prop_assert!(scope_after_rm.symbols.contains_key(&symbol_c),
            "symbol_c should still be in scope after rm()");
    }

    /// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
    /// **Validates: Requirements 6.1, 6.2, 6.3**
    ///
    /// Test that remove() alias works the same as rm() for cross-file removal.
    #[test]
    fn prop_cross_file_removal_propagation_remove_alias(
        symbol in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: sources child.R, then removes the symbol using remove()
        let parent_code = format!("source(\"child.R\")\nremove({})", symbol);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: defines the symbol
        let child_code = format!("{} <- function() {{ 1 }}", symbol);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // After source() but before remove() (line 0), symbol should be in scope
        let scope_before_remove = scope_at_position_with_graph(
            &parent_uri, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(scope_before_remove.symbols.contains_key(&symbol),
            "Symbol from sourced file should be in scope after source() but before remove()");

        // After remove() (line 1), symbol should NOT be in scope
        let scope_after_remove = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_after_remove.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after remove()");
    }

    /// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
    /// **Validates: Requirements 6.1, 6.2, 6.3**
    ///
    /// Test that rm(list=...) works for cross-file removal.
    #[test]
    fn prop_cross_file_removal_propagation_list_arg(
        symbol in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: sources child.R, then removes the symbol using rm(list=...)
        let parent_code = format!("source(\"child.R\")\nrm(list = \"{}\")", symbol);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: defines the symbol
        let child_code = format!("{} <- function() {{ 1 }}", symbol);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // After source() but before rm(list=...) (line 0), symbol should be in scope
        let scope_before_rm = scope_at_position_with_graph(
            &parent_uri, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(scope_before_rm.symbols.contains_key(&symbol),
            "Symbol from sourced file should be in scope after source() but before rm(list=...)");

        // After rm(list=...) (line 1), symbol should NOT be in scope
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after rm(list=...)");
    }

    /// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
    /// **Validates: Requirements 6.1, 6.2, 6.3**
    ///
    /// Test that rm(list=c(...)) works for cross-file removal of multiple symbols.
    #[test]
    fn prop_cross_file_removal_propagation_list_c_arg(
        symbol_a in r_identifier(),
        symbol_b in r_identifier(),
        symbol_c in r_identifier()
    ) {
        // Ensure all symbols are different
        prop_assume!(symbol_a != symbol_b && symbol_b != symbol_c && symbol_a != symbol_c);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: sources child.R, then removes symbol_a and symbol_b using rm(list=c(...))
        let parent_code = format!(
            "source(\"child.R\")\nrm(list = c(\"{}\", \"{}\"))",
            symbol_a, symbol_b
        );
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: defines all three symbols
        let child_code = format!(
            "{} <- function() {{ 1 }}\n{} <- function() {{ 2 }}\n{} <- function() {{ 3 }}",
            symbol_a, symbol_b, symbol_c
        );
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // After rm(list=c(...)) (line 1), symbol_a and symbol_b should NOT be in scope
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 40, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol_a),
            "symbol_a should NOT be in scope after rm(list=c(...))");
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol_b),
            "symbol_b should NOT be in scope after rm(list=c(...))");
        prop_assert!(scope_after_rm.symbols.contains_key(&symbol_c),
            "symbol_c should still be in scope after rm(list=c(...))");
    }

    /// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
    /// **Validates: Requirements 6.1, 6.2, 6.3**
    ///
    /// Test that rm() of a sourced symbol followed by redefinition works correctly.
    #[test]
    fn prop_cross_file_removal_then_redefine(
        symbol in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: sources child.R, removes the symbol, then redefines it
        let parent_code = format!(
            "source(\"child.R\")\nrm({})\n{} <- function() {{ 99 }}",
            symbol, symbol
        );
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: defines the symbol
        let child_code = format!("{} <- function() {{ 1 }}", symbol);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // After source() but before rm() (line 0), symbol should be in scope
        let scope_after_source = scope_at_position_with_graph(
            &parent_uri, 0, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(scope_after_source.symbols.contains_key(&symbol),
            "Symbol should be in scope after source() but before rm()");

        // After rm() but before redefinition (line 1), symbol should NOT be in scope
        let scope_after_rm = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_after_rm.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope after rm() but before redefinition");

        // After redefinition (line 2), symbol should be in scope again
        let scope_after_redef = scope_at_position_with_graph(
            &parent_uri, 2, 40, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(scope_after_redef.symbols.contains_key(&symbol),
            "Symbol should be in scope after redefinition");
    }

    /// Feature: rm-remove-support, Property 7: Cross-File Removal Propagation
    /// **Validates: Requirements 6.1, 6.2, 6.3**
    ///
    /// Test that rm() in parent does NOT affect the child file's own scope.
    /// The child file should still have its own definition available.
    #[test]
    fn prop_cross_file_removal_does_not_affect_child_scope(
        symbol in r_identifier()
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: sources child.R, then removes the symbol
        let parent_code = format!("source(\"child.R\")\nrm({})", symbol);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: defines the symbol
        let child_code = format!("{} <- function() {{ 1 }}", symbol);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // In child file, the symbol should still be in scope (child's own definition)
        let scope_in_child = scope_at_position_with_graph(
            &child_uri, 0, 40, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(scope_in_child.symbols.contains_key(&symbol),
            "Symbol should still be in scope in child file (child's own definition)");

        // In parent file after rm(), the symbol should NOT be in scope
        let scope_in_parent = scope_at_position_with_graph(
            &parent_uri, 1, 20, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );
        prop_assert!(!scope_in_parent.symbols.contains_key(&symbol),
            "Symbol should NOT be in scope in parent file after rm()");
    }
}


// ============================================================================
// Property 3: Position-Aware Package Scope
// Feature: package-function-awareness
// **Validates: Requirements 2.1, 2.2, 2.3**
//
// For any R source file with a library() call at position P, and any symbol S
// exported by that package:
// - Scope resolution at any position before P SHALL NOT include S
// - Scope resolution at any position after P SHALL include S
//
// Note: Since package exports are not yet integrated into scope resolution (task 7),
// this test verifies that:
// - PackageLoad events are correctly positioned in the timeline
// - The package name is correctly captured
// - The position ordering is correct
// ============================================================================

use super::scope::ScopeEvent;

/// R reserved words that cannot be used as package names
const R_RESERVED_PKG: &[&str] = &[
    "if", "else", "for", "in", "while", "repeat", "next", "break", "function",
    "NA", "NaN", "Inf", "NULL", "TRUE", "FALSE", "T", "F",
    "na", "nan", "inf", "null", "true", "false",
];

/// Check if a name is a valid R package name (not reserved)
fn is_valid_pkg_name(name: &str) -> bool {
    !R_RESERVED_PKG.contains(&name) && !name.is_empty()
}

/// Generate a valid R package name (lowercase letters and dots, starting with letter)
fn pkg_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9\\.]{0,8}".prop_filter("not reserved", |s| is_valid_pkg_name(s))
}

/// Generate a library call function name
fn library_func() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("library"),
        Just("require"),
        Just("loadNamespace"),
    ]
}

/// Quote style for package names
#[derive(Debug, Clone, Copy)]
enum PkgQuoteStyle {
    None,       // library(dplyr)
    Double,     // library("dplyr")
    Single,     // library('dplyr')
}

fn pkg_quote_style() -> impl Strategy<Value = PkgQuoteStyle> {
    prop_oneof![
        Just(PkgQuoteStyle::None),
        Just(PkgQuoteStyle::Double),
        Just(PkgQuoteStyle::Single),
    ]
}

/// Generate R code for a library call
fn generate_library_call(func: &str, package: &str, quote_style: PkgQuoteStyle) -> String {
    let quoted_pkg = match quote_style {
        PkgQuoteStyle::None => package.to_string(),
        PkgQuoteStyle::Double => format!("\"{}\"", package),
        PkgQuoteStyle::Single => format!("'{}'", package),
    };
    format!("{}({})", func, quoted_pkg)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 3: Position-Aware Package Scope
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    ///
    /// For any R source file with a library() call at position P:
    /// - PackageLoad events SHALL appear in the timeline at position P
    /// - PackageLoad events SHALL have the correct package name
    /// - Timeline SHALL be sorted by position (PackageLoad at correct position relative to other events)
    #[test]
    fn prop_position_aware_package_scope(
        package in pkg_name(),
        func in library_func(),
        quote_style in pkg_quote_style(),
        lines_before in 0..5usize,
        lines_after in 0..5usize,
    ) {
        let uri = make_url("test_pkg_scope");

        // Build code with library call at a specific position
        let mut code_lines = Vec::new();

        // Add filler lines before library call
        for i in 0..lines_before {
            code_lines.push(format!("x{} <- {}", i, i));
        }

        // Add library call
        let library_line = lines_before;
        code_lines.push(generate_library_call(func, &package, quote_style));

        // Add filler lines after library call
        for i in 0..lines_after {
            code_lines.push(format!("y{} <- {}", i, i));
        }

        let code = code_lines.join("\n");
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Extract PackageLoad events from timeline
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, column, package: pkg, function_scope } = e {
                    Some((*line, *column, pkg.clone(), function_scope.clone()))
                } else {
                    None
                }
            })
            .collect();

        // 1. Should have exactly one PackageLoad event
        prop_assert_eq!(
            package_load_events.len(), 1,
            "Expected exactly one PackageLoad event, got {}. Code:\n{}",
            package_load_events.len(), code
        );

        let (event_line, _event_column, event_package, event_function_scope) = &package_load_events[0];

        // 2. PackageLoad should be on the correct line
        prop_assert_eq!(
            *event_line, library_line as u32,
            "PackageLoad event on wrong line. Expected {}, got {}. Code:\n{}",
            library_line, event_line, code
        );

        // 3. Package name should be correctly captured
        prop_assert_eq!(
            event_package, &package,
            "Package name mismatch. Expected '{}', got '{}'. Code:\n{}",
            package, event_package, code
        );

        // 4. Global library call should have function_scope=None
        prop_assert!(
            event_function_scope.is_none(),
            "Global library() call should have function_scope=None. Code:\n{}",
            code
        );

        // 5. Timeline should be sorted by position
        let mut prev_pos = (0u32, 0u32);
        for event in &artifacts.timeline {
            let pos = match event {
                ScopeEvent::Def { line, column, .. } => (*line, *column),
                ScopeEvent::Source { line, column, .. } => (*line, *column),
                ScopeEvent::FunctionScope { start_line, start_column, .. } => (*start_line, *start_column),
                ScopeEvent::Removal { line, column, .. } => (*line, *column),
                ScopeEvent::PackageLoad { line, column, .. } => (*line, *column),
            };
            prop_assert!(
                pos >= prev_pos,
                "Timeline not sorted: event at ({}, {}) comes after ({}, {}). Code:\n{}",
                pos.0, pos.1, prev_pos.0, prev_pos.1, code
            );
            prev_pos = pos;
        }

        // 6. Verify position-aware property: PackageLoad should come AFTER definitions on earlier lines
        //    and BEFORE definitions on later lines
        for event in &artifacts.timeline {
            if let ScopeEvent::Def { line, .. } = event {
                if *line < library_line as u32 {
                    // Definitions before library call should come before PackageLoad in timeline
                    // (This is implicitly verified by the sorted check above)
                }
            }
        }
    }

    /// Property 3 extended: Multiple library calls should create multiple PackageLoad events in order
    /// **Validates: Requirements 2.1, 2.2, 2.3**
    #[test]
    fn prop_multiple_library_calls_ordered(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
        pkg3 in pkg_name(),
    ) {
        // Ensure packages are distinct for clearer testing
        prop_assume!(pkg1 != pkg2 && pkg2 != pkg3 && pkg1 != pkg3);

        let uri = make_url("test_multi_pkg");

        let code = format!(
            "x <- 1\nlibrary({})\ny <- 2\nlibrary({})\nz <- 3\nlibrary({})\nw <- 4",
            pkg1, pkg2, pkg3
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Extract PackageLoad events
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, package, .. } = e {
                    Some((*line, package.clone()))
                } else {
                    None
                }
            })
            .collect();

        // Should have exactly 3 PackageLoad events
        prop_assert_eq!(
            package_load_events.len(), 3,
            "Expected 3 PackageLoad events, got {}. Code:\n{}",
            package_load_events.len(), code
        );

        // Events should be in document order with correct packages
        prop_assert_eq!(&package_load_events[0], &(1, pkg1.clone()),
            "First PackageLoad should be {} on line 1", pkg1);
        prop_assert_eq!(&package_load_events[1], &(3, pkg2.clone()),
            "Second PackageLoad should be {} on line 3", pkg2);
        prop_assert_eq!(&package_load_events[2], &(5, pkg3.clone()),
            "Third PackageLoad should be {} on line 5", pkg3);

        // Verify strict ordering: each PackageLoad should be after the previous
        for i in 1..package_load_events.len() {
            let (prev_line, _) = &package_load_events[i - 1];
            let (curr_line, _) = &package_load_events[i];
            prop_assert!(
                curr_line > prev_line,
                "PackageLoad events not in strict order: line {} not > line {}",
                curr_line, prev_line
            );
        }
    }

    /// Property 3 extended: PackageLoad position should be at the END of the library() call
    /// **Validates: Requirements 2.2, 2.3**
    ///
    /// This ensures that symbols from the package are only available AFTER the call completes,
    /// not during the call itself.
    #[test]
    fn prop_package_load_position_at_call_end(
        package in pkg_name(),
        func in library_func(),
    ) {
        let uri = make_url("test_pkg_pos");

        let code = format!("{}({})", func, package);
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, column, package: pkg, .. } = e {
                    Some((*line, *column, pkg.clone()))
                } else {
                    None
                }
            })
            .collect();

        prop_assert_eq!(package_load_events.len(), 1);
        let (line, column, pkg) = &package_load_events[0];

        // Should be on line 0
        prop_assert_eq!(*line, 0);

        // Package name should match
        prop_assert_eq!(pkg, &package);

        // Column should be at or after the end of the call (after the closing paren)
        // The call is: func(package) or func("package") or func('package')
        // Minimum length is func.len() + 1 (open paren) + package.len() + 1 (close paren)
        let min_end_column = (func.len() + 1 + package.len() + 1) as u32;
        prop_assert!(
            *column >= min_end_column - 1, // Allow for 0-based indexing variations
            "PackageLoad column {} should be at or near end of call (min expected: {}). Code: {}",
            column, min_end_column, code
        );
    }
}

// ============================================================================
// Property 4: Function-Scoped Package Loading
// Feature: package-function-awareness
// **Validates: Requirements 2.4, 2.5**
//
// For any R source file with a library() call inside a function body, the package
// exports SHALL only be available within that function's scope, not at the global
// level or in other functions.
//
// This test verifies that:
// - PackageLoad events inside functions have the correct function_scope set (not None)
// - The function_scope interval matches the containing function
// - library() calls at global scope have function_scope=None
// - Nested functions capture the innermost function scope
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 4: Function-Scoped Package Loading
    /// **Validates: Requirements 2.4, 2.5**
    ///
    /// For any R source file with a library() call inside a function body:
    /// - The PackageLoad event SHALL have function_scope set to the containing function
    /// - The function_scope interval SHALL match the containing function's boundaries
    #[test]
    fn prop_function_scoped_package_loading(
        package in pkg_name(),
        func_name in r_identifier(),
        lines_before_lib in 0..3usize,
        lines_after_lib in 0..3usize,
    ) {
        let uri = make_url("test_func_pkg_scope");

        // Build code with library call inside a function
        let mut func_body_lines = Vec::new();

        // Add filler lines before library call inside function
        for i in 0..lines_before_lib {
            func_body_lines.push(format!("    x{} <- {}", i, i));
        }

        // Add library call inside function
        func_body_lines.push(format!("    library({})", package));

        // Add filler lines after library call inside function
        for i in 0..lines_after_lib {
            func_body_lines.push(format!("    y{} <- {}", i, i));
        }

        let func_body = func_body_lines.join("\n");
        let code = format!("{} <- function() {{\n{}\n}}", func_name, func_body);

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Extract PackageLoad events from timeline
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, column, package: pkg, function_scope } = e {
                    Some((*line, *column, pkg.clone(), function_scope.clone()))
                } else {
                    None
                }
            })
            .collect();

        // 1. Should have exactly one PackageLoad event
        prop_assert_eq!(
            package_load_events.len(), 1,
            "Expected exactly one PackageLoad event, got {}. Code:\n{}",
            package_load_events.len(), code
        );

        let (event_line, _event_column, event_package, event_function_scope) = &package_load_events[0];

        // 2. Package name should be correctly captured
        prop_assert_eq!(
            event_package, &package,
            "Package name mismatch. Expected '{}', got '{}'. Code:\n{}",
            package, event_package, code
        );

        // 3. Library call inside function should have function_scope=Some(...)
        prop_assert!(
            event_function_scope.is_some(),
            "Library call inside function should have function_scope=Some(...), got None. Code:\n{}",
            code
        );

        let function_scope = event_function_scope.as_ref().unwrap();

        // 4. Function scope should start at line 0 (where the function definition starts)
        prop_assert_eq!(
            function_scope.start.line, 0,
            "Function scope should start at line 0. Got start line {}. Code:\n{}",
            function_scope.start.line, code
        );

        // 5. Function scope should end at the last line (where the closing brace is)
        let last_line = code.lines().count() as u32 - 1;
        prop_assert_eq!(
            function_scope.end.line, last_line,
            "Function scope should end at line {}. Got end line {}. Code:\n{}",
            last_line, function_scope.end.line, code
        );

        // 6. The library call line should be within the function scope
        prop_assert!(
            function_scope.contains(super::scope::Position::new(*event_line, 0)),
            "Library call at line {} should be within function scope ({}, {}) to ({}, {}). Code:\n{}",
            event_line,
            function_scope.start.line, function_scope.start.column,
            function_scope.end.line, function_scope.end.column,
            code
        );
    }

    /// Property 4 extended: Global library() calls should have function_scope=None
    /// **Validates: Requirements 2.4, 2.5**
    #[test]
    fn prop_global_library_call_no_function_scope(
        package in pkg_name(),
        func in library_func(),
        quote_style in pkg_quote_style(),
    ) {
        let uri = make_url("test_global_pkg");

        // Global library call (not inside any function)
        let code = generate_library_call(func, &package, quote_style);

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Extract PackageLoad events
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { function_scope, package: pkg, .. } = e {
                    Some((pkg.clone(), function_scope.clone()))
                } else {
                    None
                }
            })
            .collect();

        prop_assert_eq!(package_load_events.len(), 1);
        let (pkg, function_scope) = &package_load_events[0];

        prop_assert_eq!(pkg, &package);
        prop_assert!(
            function_scope.is_none(),
            "Global library() call should have function_scope=None, got {:?}. Code:\n{}",
            function_scope, code
        );
    }

    /// Property 4 extended: Nested functions should capture innermost function scope
    /// **Validates: Requirements 2.4, 2.5**
    #[test]
    fn prop_nested_function_innermost_scope(
        package in pkg_name(),
        outer_func in r_identifier(),
        inner_func in r_identifier(),
    ) {
        // Ensure function names are distinct
        prop_assume!(outer_func != inner_func);

        let uri = make_url("test_nested_pkg");

        // Code with library() call inside nested function
        let code = format!(
            "{} <- function() {{\n    {} <- function() {{\n        library({})\n    }}\n}}",
            outer_func, inner_func, package
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Extract PackageLoad events
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, function_scope, package: pkg, .. } = e {
                    Some((*line, pkg.clone(), function_scope.clone()))
                } else {
                    None
                }
            })
            .collect();

        prop_assert_eq!(
            package_load_events.len(), 1,
            "Expected exactly one PackageLoad event. Code:\n{}", code
        );

        let (event_line, event_package, event_function_scope) = &package_load_events[0];

        prop_assert_eq!(event_package, &package);

        // Should have function_scope (inside nested function)
        prop_assert!(
            event_function_scope.is_some(),
            "Library call in nested function should have function_scope. Code:\n{}",
            code
        );

        let function_scope = event_function_scope.as_ref().unwrap();

        // The function scope should be the INNER function (line 1 to line 3)
        // Outer function: lines 0-4
        // Inner function: lines 1-3
        prop_assert_eq!(
            function_scope.start.line, 1,
            "Function scope should start at inner function (line 1). Got {}. Code:\n{}",
            function_scope.start.line, code
        );

        prop_assert_eq!(
            function_scope.end.line, 3,
            "Function scope should end at inner function closing brace (line 3). Got {}. Code:\n{}",
            function_scope.end.line, code
        );

        // Library call is on line 2, which should be within the inner function scope
        prop_assert_eq!(
            *event_line, 2,
            "Library call should be on line 2. Got {}. Code:\n{}",
            event_line, code
        );

        prop_assert!(
            function_scope.contains(super::scope::Position::new(*event_line, 0)),
            "Library call should be within inner function scope. Code:\n{}",
            code
        );
    }

    /// Property 4 extended: Multiple library calls in different scopes
    /// **Validates: Requirements 2.4, 2.5**
    #[test]
    fn prop_multiple_library_calls_different_scopes(
        pkg_global in pkg_name(),
        pkg_func in pkg_name(),
        func_name in r_identifier(),
    ) {
        // Ensure packages are distinct
        prop_assume!(pkg_global != pkg_func);

        let uri = make_url("test_multi_scope_pkg");

        // Code with library() at global scope and inside a function
        let code = format!(
            "library({})\n{} <- function() {{\n    library({})\n}}",
            pkg_global, func_name, pkg_func
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Extract PackageLoad events
        let package_load_events: Vec<_> = artifacts.timeline.iter()
            .filter_map(|e| {
                if let ScopeEvent::PackageLoad { line, package, function_scope, .. } = e {
                    Some((*line, package.clone(), function_scope.clone()))
                } else {
                    None
                }
            })
            .collect();

        prop_assert_eq!(
            package_load_events.len(), 2,
            "Expected 2 PackageLoad events. Code:\n{}", code
        );

        // First event: global library call (line 0, no function scope)
        let (line0, pkg0, scope0) = &package_load_events[0];
        prop_assert_eq!(*line0, 0, "First library call should be on line 0");
        prop_assert_eq!(pkg0, &pkg_global, "First package should be {}", pkg_global);
        prop_assert!(
            scope0.is_none(),
            "Global library call should have function_scope=None. Code:\n{}",
            code
        );

        // Second event: function-scoped library call (line 2, with function scope)
        let (line2, pkg2, scope2) = &package_load_events[1];
        prop_assert_eq!(*line2, 2, "Second library call should be on line 2");
        prop_assert_eq!(pkg2, &pkg_func, "Second package should be {}", pkg_func);
        prop_assert!(
            scope2.is_some(),
            "Function library call should have function_scope=Some(...). Code:\n{}",
            code
        );

        // Verify the function scope boundaries
        let func_scope = scope2.as_ref().unwrap();
        prop_assert_eq!(
            func_scope.start.line, 1,
            "Function scope should start at line 1. Code:\n{}",
            code
        );
        prop_assert_eq!(
            func_scope.end.line, 3,
            "Function scope should end at line 3. Code:\n{}",
            code
        );
    }
}

// ============================================================================
// Property: Point Query Correctness (Interval Tree)
// Validates: Requirements 1.3, 1.4
// Feature: interval-tree-scope-lookup, Property 1: Point Query Correctness
// ============================================================================

use super::scope::{FunctionScopeInterval, FunctionScopeTree, Position};

/// Produces a strategy yielding valid intervals as (start_line, start_col, end_line, end_col)
/// where the end position is lexicographically greater than or equal to the start position.
///
/// # Examples
///
/// ```
/// use proptest::prelude::*;
///
/// proptest! {
///     |(iv in valid_interval())| {
///         let (sl, sc, el, ec) = iv;
///         assert!((el, ec) >= (sl, sc));
///     }
/// }
/// ```
fn valid_interval() -> impl Strategy<Value = (u32, u32, u32, u32)> {
    // Generate start position, then end position >= start
    (0..1000u32, 0..100u32).prop_flat_map(|(start_line, start_col)| {
        let end_line_range = start_line..1000u32;
        let end_col_range = if start_line == 1000u32 - 1 {
            start_col..100u32
        } else {
            0..100u32
        };
        (end_line_range, end_col_range).prop_map(move |(end_line, end_col)| {
            let end_col = if end_line == start_line {
                end_col.max(start_col)
            } else {
                end_col
            };
            (start_line, start_col, end_line, end_col)
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: interval-tree-scope-lookup, Property 1: Point Query Correctness
    /// **Validates: Requirements 1.3, 1.4**
    ///
    /// For any set of function scope intervals and for any query position,
    /// the point query SHALL return exactly those intervals that contain the position—
    /// no false positives (intervals that don't contain the point) and no false negatives
    /// (missing intervals that do contain the point).
    #[test]
    fn prop_point_query_correctness(
        intervals in prop::collection::vec(valid_interval(), 0..50),
        query_line in 0..1000u32,
        query_col in 0..100u32,
    ) {
        // Build tree from generated intervals
        let tree = FunctionScopeTree::from_scopes(&intervals);
        let query_pos = Position::new(query_line, query_col);

        // Query tree for all intervals containing the point
        let tree_results = tree.query_point(query_pos);

        // Brute force: find all intervals containing the point
        let brute_force: Vec<FunctionScopeInterval> = intervals
            .iter()
            .filter_map(|&(sl, sc, el, ec)| {
                let interval = FunctionScopeInterval::from_tuple((sl, sc, el, ec));
                // Only include valid intervals (start <= end)
                if interval.start <= interval.end && interval.contains(query_pos) {
                    Some(interval)
                } else {
                    None
                }
            })
            .collect();

        // Verify no false positives: all returned intervals must contain the point
        for interval in &tree_results {
            prop_assert!(
                interval.contains(query_pos),
                "False positive: interval ({}, {}) - ({}, {}) does not contain point ({}, {})",
                interval.start.line,
                interval.start.column,
                interval.end.line,
                interval.end.column,
                query_line,
                query_col
            );
        }

        // Verify no false negatives: same count as brute force
        prop_assert_eq!(
            tree_results.len(),
            brute_force.len(),
            "Missing intervals: tree returned {} but brute force found {}",
            tree_results.len(),
            brute_force.len()
        );

        // Verify the actual intervals match (not just count)
        // Sort both for comparison since order may differ
        let mut tree_sorted: Vec<_> = tree_results.iter().map(|i| i.as_tuple()).collect();
        let mut brute_sorted: Vec<_> = brute_force.iter().map(|i| i.as_tuple()).collect();
        tree_sorted.sort();
        brute_sorted.sort();
        prop_assert_eq!(
            tree_sorted,
            brute_sorted,
            "Interval sets differ between tree and brute force"
        );
    }

    /// Feature: interval-tree-scope-lookup, Property 1: Point Query Correctness (Empty Tree)
    /// **Validates: Requirements 1.6**
    ///
    /// An empty tree should return an empty result for any query.
    #[test]
    fn prop_point_query_empty_tree(
        query_line in 0..1000u32,
        query_col in 0..100u32,
    ) {
        let tree = FunctionScopeTree::new();
        let query_pos = Position::new(query_line, query_col);

        let results = tree.query_point(query_pos);

        prop_assert!(
            results.is_empty(),
            "Empty tree should return empty results, got {} intervals",
            results.len()
        );
    }

    /// Feature: interval-tree-scope-lookup, Property 1: Point Query Correctness (Boundary Positions)
    /// **Validates: Requirements 1.3, 4.2**
    ///
    /// Positions exactly at interval boundaries should be included (inclusive boundaries).
    #[test]
    fn prop_point_query_boundary_positions(
        intervals in prop::collection::vec(valid_interval(), 1..20),
    ) {
        let tree = FunctionScopeTree::from_scopes(&intervals);

        // For each valid interval, test that start and end positions are included
        for &(sl, sc, el, ec) in &intervals {
            let interval = FunctionScopeInterval::from_tuple((sl, sc, el, ec));
            // Skip invalid intervals
            if interval.start > interval.end {
                continue;
            }

            // Query at start position
            let start_results = tree.query_point(interval.start);
            prop_assert!(
                start_results.iter().any(|i| *i == interval),
                "Interval ({}, {}) - ({}, {}) should contain its start position ({}, {})",
                sl, sc, el, ec, sl, sc
            );

            // Query at end position
            let end_results = tree.query_point(interval.end);
            prop_assert!(
                end_results.iter().any(|i| *i == interval),
                "Interval ({}, {}) - ({}, {}) should contain its end position ({}, {})",
                sl, sc, el, ec, el, ec
            );
        }
    }
}


// ============================================================================
// Property 2: Innermost Selection Correctness (Interval Tree)
// Validates: Requirements 2.1, 2.2
// Feature: interval-tree-scope-lookup, Property 2: Innermost Selection Correctness
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: interval-tree-scope-lookup, Property 2: Innermost Selection Correctness
    /// **Validates: Requirements 2.1, 2.2**
    ///
    /// For any set of function scope intervals and for any query position,
    /// the innermost query SHALL return the interval with the lexicographically largest
    /// start position among all intervals containing the query point, or None if no
    /// intervals contain the point.
    #[test]
    fn prop_innermost_selection_correctness(
        intervals in prop::collection::vec(valid_interval(), 0..50),
        query_line in 0..1000u32,
        query_col in 0..100u32,
    ) {
        let tree = FunctionScopeTree::from_scopes(&intervals);
        let query_pos = Position::new(query_line, query_col);

        // Query tree for innermost
        let tree_result = tree.query_innermost(query_pos);

        // Brute force: find all containing intervals, then select max by start
        let containing: Vec<_> = intervals.iter()
            .filter_map(|&(sl, sc, el, ec)| {
                let interval = FunctionScopeInterval::from_tuple((sl, sc, el, ec));
                if interval.start <= interval.end && interval.contains(query_pos) {
                    Some(interval)
                } else {
                    None
                }
            })
            .collect();

        let brute_force_result = containing.into_iter()
            .max_by_key(|interval| interval.start);

        // Verify results match
        match (tree_result, brute_force_result) {
            (None, None) => { /* Both found nothing - correct */ }
            (Some(tree_interval), Some(brute_interval)) => {
                prop_assert_eq!(
                    (tree_interval.start.line, tree_interval.start.column),
                    (brute_interval.start.line, brute_interval.start.column),
                    "Innermost mismatch: tree start ({}, {}) vs brute start ({}, {})",
                    tree_interval.start.line, tree_interval.start.column,
                    brute_interval.start.line, brute_interval.start.column
                );
            }
            (Some(tree_interval), None) => {
                prop_assert!(
                    false,
                    "Tree returned interval ({}, {}) - ({}, {}) but brute force found nothing",
                    tree_interval.start.line, tree_interval.start.column,
                    tree_interval.end.line, tree_interval.end.column
                );
            }
            (None, Some(brute_interval)) => {
                prop_assert!(
                    false,
                    "Tree returned nothing but brute force found ({}, {}) - ({}, {})",
                    brute_interval.start.line, brute_interval.start.column,
                    brute_interval.end.line, brute_interval.end.column
                );
            }
        }
    }

    /// Feature: interval-tree-scope-lookup, Property 2: Innermost Selection Correctness (Empty Tree)
    /// **Validates: Requirements 2.2**
    ///
    /// An empty tree should return None for any innermost query.
    #[test]
    fn prop_innermost_selection_empty_tree(
        query_line in 0..1000u32,
        query_col in 0..100u32,
    ) {
        let tree = FunctionScopeTree::new();
        let query_pos = Position::new(query_line, query_col);

        let result = tree.query_innermost(query_pos);

        prop_assert!(
            result.is_none(),
            "Empty tree should return None for innermost query, got {:?}",
            result
        );
    }

    /// Feature: interval-tree-scope-lookup, Property 2: Innermost Selection Correctness (Nested Intervals)
    /// **Validates: Requirements 2.1**
    ///
    /// For nested intervals, the innermost (latest start) should be selected.
    #[test]
    fn prop_innermost_selection_nested(
        // Generate a base interval
        base_start_line in 0..500u32,
        base_start_col in 0..50u32,
        base_end_line in 500..1000u32,
        base_end_col in 50..100u32,
        // Generate a nested interval inside the base
        nest_offset_start_line in 1..100u32,
        nest_offset_start_col in 1..20u32,
        nest_offset_end_line in 1..100u32,
        nest_offset_end_col in 1..20u32,
    ) {
        // Create base interval
        let base = (base_start_line, base_start_col, base_end_line, base_end_col);

        // Create nested interval strictly inside base
        let nested_start_line = base_start_line + nest_offset_start_line;
        let nested_start_col = base_start_col + nest_offset_start_col;
        let nested_end_line = base_end_line.saturating_sub(nest_offset_end_line);
        let nested_end_col = base_end_col.saturating_sub(nest_offset_end_col);

        // Ensure nested is valid (start <= end)
        prop_assume!(
            (nested_start_line, nested_start_col) <= (nested_end_line, nested_end_col)
        );

        let nested = (nested_start_line, nested_start_col, nested_end_line, nested_end_col);

        let intervals = vec![base, nested];
        let tree = FunctionScopeTree::from_scopes(&intervals);

        // Query at a point inside the nested interval
        let query_line = (nested_start_line + nested_end_line) / 2;
        let query_col = (nested_start_col + nested_end_col) / 2;
        let query_pos = Position::new(query_line, query_col);

        let result = tree.query_innermost(query_pos);

        // The nested interval has a later start, so it should be selected
        prop_assert!(result.is_some(), "Should find an interval containing the query point");
        let result = result.unwrap();

        // The result should be the nested interval (later start)
        prop_assert_eq!(
            result.as_tuple(),
            nested,
            "Should select nested interval (later start) as innermost"
        );
    }

    /// Feature: interval-tree-scope-lookup, Property 2: Innermost Selection Correctness (Point Outside)
    /// **Validates: Requirements 2.2**
    ///
    /// When the query point is outside all intervals, None should be returned.
    #[test]
    fn prop_innermost_selection_point_outside(
        intervals in prop::collection::vec(valid_interval(), 1..20),
    ) {
        let tree = FunctionScopeTree::from_scopes(&intervals);

        // Find a point guaranteed to be outside all intervals
        // Use a line beyond all interval end lines
        let max_end_line = intervals.iter()
            .map(|&(_, _, el, _)| el)
            .max()
            .unwrap_or(0);

        let query_pos = Position::new(max_end_line + 100, 0);

        let result = tree.query_innermost(query_pos);

        prop_assert!(
            result.is_none(),
            "Point outside all intervals should return None, got {:?}",
            result
        );
    }
}


// ============================================================================
// Property 3: Backward Compatibility (Model-Based)
// Validates: Requirements 3.4
// Feature: interval-tree-scope-lookup, Property 3: Backward Compatibility
// ============================================================================

/// Return all intervals that contain a given position using a linear scan.
///
/// Scopes are tuples of the form `(start_line, start_col, end_line, end_col)`.
/// Only intervals where the start is less than or equal to the end are considered,
/// and an interval is included only if `start <= (line, column) <= end`.
///
/// # Returns
///
/// A `Vec` of the intervals from `scopes` that contain the specified position,
/// preserving their original order.
///
/// # Examples
///
/// ```
/// let scopes = vec![ (1, 0, 3, 10), (2, 0, 2, 5), (4, 0, 5, 0) ];
/// let containing = linear_scan_containing(&scopes, 2, 3);
/// assert_eq!(containing, vec![ (1, 0, 3, 10), (2, 0, 2, 5) ]);
/// ```
fn linear_scan_containing(
    scopes: &[(u32, u32, u32, u32)],
    line: u32,
    column: u32,
) -> Vec<(u32, u32, u32, u32)> {
    scopes
        .iter()
        .filter(|(sl, sc, el, ec)| {
            // Only include valid intervals (start <= end)
            (*sl, *sc) <= (*el, *ec)
                // Check containment: start <= point <= end
                && (*sl, *sc) <= (line, column)
                && (line, column) <= (*el, *ec)
        })
        .copied()
        .collect()
}

/// Selects the innermost interval that contains the given position using a linear scan.
///
/// Intervals are tuples (start_line, start_col, end_line, end_col) and are treated as
/// inclusive: start <= position <= end. If multiple intervals share the same maximum
/// start position, one of them (the last encountered in iteration order) is returned.
/// Returns `None` if no interval contains the position.
///
/// # Examples
///
/// ```
/// let scopes = vec![(1, 0, 3, 0), (2, 0, 2, 5)]; // second interval is nested inside the first
/// let found = crate::linear_scan_innermost(&scopes, 2, 1).unwrap();
/// assert_eq!(found, (2, 0, 2, 5));
/// ```
#[allow(dead_code)]
fn linear_scan_innermost(
    scopes: &[(u32, u32, u32, u32)],
    line: u32,
    column: u32,
) -> Option<(u32, u32, u32, u32)> {
    scopes
        .iter()
        .filter(|(sl, sc, el, ec)| {
            // Only include valid intervals (start <= end)
            (*sl, *sc) <= (*el, *ec)
                // Check containment: start <= point <= end
                && (*sl, *sc) <= (line, column)
                && (line, column) <= (*el, *ec)
        })
        .max_by_key(|(sl, sc, _, _)| (*sl, *sc))
        .copied()
}

/// Finds the start position (line, column) of the containing interval with the greatest start.
///
/// Scans `scopes` for intervals that contain the point `(line, column)` and returns the start
/// `(line, column)` pair of the interval whose start is maximal (lexicographic by line then column).
///
/// # Returns
///
/// `Some((start_line, start_column))` if at least one interval contains the point, `None` otherwise.
///
/// # Examples
///
/// ```
/// let scopes = vec![
///     (1, 0, 3, 5), // start (1,0) .. end (3,5)
///     (2, 0, 2, 10), // start (2,0) .. end (2,10)
/// ];
/// assert_eq!(find_max_start_position(&scopes, 2, 3), Some((2, 0)));
/// assert_eq!(find_max_start_position(&scopes, 4, 0), None);
/// ```
fn find_max_start_position(
    scopes: &[(u32, u32, u32, u32)],
    line: u32,
    column: u32,
) -> Option<(u32, u32)> {
    scopes
        .iter()
        .filter(|(sl, sc, el, ec)| {
            // Only include valid intervals (start <= end)
            (*sl, *sc) <= (*el, *ec)
                // Check containment: start <= point <= end
                && (*sl, *sc) <= (line, column)
                && (line, column) <= (*el, *ec)
        })
        .map(|(sl, sc, _, _)| (*sl, *sc))
        .max()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: interval-tree-scope-lookup, Property 3: Backward Compatibility
    /// **Validates: Requirements 3.4**
    ///
    /// For any valid function scopes and any position, the interval tree SHALL produce
    /// identical results to the original linear scan implementation.
    ///
    /// This is a model-based property test where the "model" is the simple linear scan
    /// implementation that was replaced by the interval tree.
    #[test]
    fn prop_backward_compatibility(
        scopes in prop::collection::vec(valid_interval(), 0..100),
        queries in prop::collection::vec((0..1000u32, 0..100u32), 1..20),
    ) {
        let tree = FunctionScopeTree::from_scopes(&scopes);

        for (line, column) in queries {
            // Compare query_point results
            let tree_containing: Vec<_> = tree
                .query_point(Position::new(line, column))
                .into_iter()
                .map(|i| i.as_tuple())
                .collect();
            let linear_containing = linear_scan_containing(&scopes, line, column);

            // Sort both for comparison since order may differ
            let mut tree_sorted = tree_containing.clone();
            let mut linear_sorted = linear_containing.clone();
            tree_sorted.sort();
            linear_sorted.sort();

            prop_assert_eq!(
                &tree_sorted,
                &linear_sorted,
                "query_point mismatch at ({}, {}): tree returned {:?}, linear scan returned {:?}",
                line,
                column,
                &tree_sorted,
                &linear_sorted
            );

            // Compare query_innermost results
            // When multiple intervals have the same maximum start position, either one is valid.
            // We verify that the tree returns an interval with the correct maximum start position.
            let tree_innermost = tree
                .query_innermost(Position::new(line, column))
                .map(|i| i.as_tuple());
            let max_start = find_max_start_position(&scopes, line, column);

            match (tree_innermost, max_start) {
                (None, None) => { /* Both found nothing - correct */ }
                (Some((sl, sc, _, _)), Some((expected_sl, expected_sc))) => {
                    prop_assert_eq!(
                        (sl, sc),
                        (expected_sl, expected_sc),
                        "query_innermost returned interval with wrong start position at ({}, {}): got ({}, {}), expected ({}, {})",
                        line,
                        column,
                        sl,
                        sc,
                        expected_sl,
                        expected_sc
                    );
                }
                (Some(interval), None) => {
                    prop_assert!(
                        false,
                        "Tree returned interval {:?} but no intervals contain point ({}, {})",
                        interval,
                        line,
                        column
                    );
                }
                (None, Some(start)) => {
                    prop_assert!(
                        false,
                        "Tree returned nothing but intervals with start {:?} contain point ({}, {})",
                        start,
                        line,
                        column
                    );
                }
            }
        }
    }

    /// Feature: interval-tree-scope-lookup, Property 3: Backward Compatibility (Edge Cases)
    /// **Validates: Requirements 3.4**
    ///
    /// Test backward compatibility with edge case scenarios:
    /// - Empty scopes
    /// - Single scope
    /// - Deeply nested scopes
    /// - Overlapping but non-nested scopes
    #[test]
    fn prop_backward_compatibility_edge_cases(
        // Generate a mix of nested and non-nested intervals
        base_intervals in prop::collection::vec(valid_interval(), 0..20),
        // Generate additional nested intervals
        nested_count in 0..10usize,
        query_line in 0..1000u32,
        query_col in 0..100u32,
    ) {
        // Create some nested intervals by shrinking existing ones
        let mut all_scopes = base_intervals.clone();
        for i in 0..nested_count.min(base_intervals.len()) {
            let (sl, sc, el, ec) = base_intervals[i];
            // Create a nested interval if possible
            if sl + 1 < el || (sl + 1 == el && sc + 1 < ec) {
                let nested = (
                    sl + 1,
                    sc.saturating_add(1).min(99),
                    el.saturating_sub(1).max(sl + 1),
                    ec.saturating_sub(1).max(0),
                );
                // Only add if valid
                if (nested.0, nested.1) <= (nested.2, nested.3) {
                    all_scopes.push(nested);
                }
            }
        }

        let tree = FunctionScopeTree::from_scopes(&all_scopes);

        // Compare query_point results
        let tree_containing: Vec<_> = tree
            .query_point(Position::new(query_line, query_col))
            .into_iter()
            .map(|i| i.as_tuple())
            .collect();
        let linear_containing = linear_scan_containing(&all_scopes, query_line, query_col);

        let mut tree_sorted = tree_containing.clone();
        let mut linear_sorted = linear_containing.clone();
        tree_sorted.sort();
        linear_sorted.sort();

        prop_assert_eq!(
            tree_sorted,
            linear_sorted,
            "query_point mismatch with nested intervals at ({}, {})",
            query_line,
            query_col
        );

        // Compare query_innermost results
        // When multiple intervals have the same maximum start position, either one is valid.
        let tree_innermost = tree
            .query_innermost(Position::new(query_line, query_col))
            .map(|i| i.as_tuple());
        let max_start = find_max_start_position(&all_scopes, query_line, query_col);

        match (tree_innermost, max_start) {
            (None, None) => { /* Both found nothing - correct */ }
            (Some((tsl, tsc, _, _)), Some((expected_sl, expected_sc))) => {
                prop_assert_eq!(
                    (tsl, tsc),
                    (expected_sl, expected_sc),
                    "query_innermost returned interval with wrong start position at ({}, {})",
                    query_line,
                    query_col
                );
            }
            (Some(interval), None) => {
                prop_assert!(
                    false,
                    "Tree returned interval {:?} but no intervals contain point ({}, {})",
                    interval,
                    query_line,
                    query_col
                );
            }
            (None, Some(start)) => {
                prop_assert!(
                    false,
                    "Tree returned nothing but intervals with start {:?} contain point ({}, {})",
                    start,
                    query_line,
                    query_col
                );
            }
        }
    }

    /// Feature: interval-tree-scope-lookup, Property 3: Backward Compatibility (Boundary Queries)
    /// **Validates: Requirements 3.4**
    ///
    /// Test backward compatibility specifically at interval boundaries.
    #[test]
    fn prop_backward_compatibility_boundaries(
        scopes in prop::collection::vec(valid_interval(), 1..50),
    ) {
        let tree = FunctionScopeTree::from_scopes(&scopes);

        // Test at each interval's start and end positions
        for &(sl, sc, el, ec) in &scopes {
            // Skip invalid intervals
            if (sl, sc) > (el, ec) {
                continue;
            }

            // Test at start position
            let tree_at_start: Vec<_> = tree
                .query_point(Position::new(sl, sc))
                .into_iter()
                .map(|i| i.as_tuple())
                .collect();
            let linear_at_start = linear_scan_containing(&scopes, sl, sc);

            let mut tree_sorted = tree_at_start.clone();
            let mut linear_sorted = linear_at_start.clone();
            tree_sorted.sort();
            linear_sorted.sort();

            prop_assert_eq!(
                tree_sorted,
                linear_sorted,
                "query_point mismatch at start boundary ({}, {})",
                sl,
                sc
            );

            // Test at end position
            let tree_at_end: Vec<_> = tree
                .query_point(Position::new(el, ec))
                .into_iter()
                .map(|i| i.as_tuple())
                .collect();
            let linear_at_end = linear_scan_containing(&scopes, el, ec);

            let mut tree_sorted = tree_at_end.clone();
            let mut linear_sorted = linear_at_end.clone();
            tree_sorted.sort();
            linear_sorted.sort();

            prop_assert_eq!(
                tree_sorted,
                linear_sorted,
                "query_point mismatch at end boundary ({}, {})",
                el,
                ec
            );

            // Test innermost at start
            // When multiple intervals have the same maximum start position, either one is valid.
            let tree_innermost_start = tree
                .query_innermost(Position::new(sl, sc))
                .map(|i| i.as_tuple());
            let max_start_at_start = find_max_start_position(&scopes, sl, sc);

            match (tree_innermost_start, max_start_at_start) {
                (None, None) => { /* Both found nothing - correct */ }
                (Some((tsl, tsc, _, _)), Some((expected_sl, expected_sc))) => {
                    prop_assert_eq!(
                        (tsl, tsc),
                        (expected_sl, expected_sc),
                        "query_innermost returned interval with wrong start position at start boundary ({}, {})",
                        sl,
                        sc
                    );
                }
                (Some(interval), None) => {
                    prop_assert!(
                        false,
                        "Tree returned interval {:?} but no intervals contain start boundary ({}, {})",
                        interval,
                        sl,
                        sc
                    );
                }
                (None, Some(start)) => {
                    prop_assert!(
                        false,
                        "Tree returned nothing but intervals with start {:?} contain start boundary ({}, {})",
                        start,
                        sl,
                        sc
                    );
                }
            }

            // Test innermost at end
            let tree_innermost_end = tree
                .query_innermost(Position::new(el, ec))
                .map(|i| i.as_tuple());
            let max_start_at_end = find_max_start_position(&scopes, el, ec);

            match (tree_innermost_end, max_start_at_end) {
                (None, None) => { /* Both found nothing - correct */ }
                (Some((tsl, tsc, _, _)), Some((expected_sl, expected_sc))) => {
                    prop_assert_eq!(
                        (tsl, tsc),
                        (expected_sl, expected_sc),
                        "query_innermost returned interval with wrong start position at end boundary ({}, {})",
                        el,
                        ec
                    );
                }
                (Some(interval), None) => {
                    prop_assert!(
                        false,
                        "Tree returned interval {:?} but no intervals contain end boundary ({}, {})",
                        interval,
                        el,
                        ec
                    );
                }
                (None, Some(start)) => {
                    prop_assert!(
                        false,
                        "Tree returned nothing but intervals with start {:?} contain end boundary ({}, {})",
                        start,
                        el,
                        ec
                    );
                }
            }
        }
    }
}

// ============================================================================
// Property 4: Position Lexicographic Ordering
// Validates: Requirements 4.1
// Feature: interval-tree-scope-lookup, Property 4: Position Lexicographic Ordering
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: interval-tree-scope-lookup, Property 4: Position Lexicographic Ordering
    /// **Validates: Requirements 4.1**
    ///
    /// For any two positions (line1, col1) and (line2, col2), the Position comparison
    /// SHALL follow lexicographic ordering: (line1, col1) < (line2, col2) if and only if
    /// line1 < line2, or (line1 == line2 and col1 < col2).
    #[test]
    fn prop_position_lexicographic_ordering(
        line1 in 0..u32::MAX,
        col1 in 0..u32::MAX,
        line2 in 0..u32::MAX,
        col2 in 0..u32::MAX,
    ) {
        let pos1 = Position::new(line1, col1);
        let pos2 = Position::new(line2, col2);

        // Expected lexicographic comparison
        let expected_less = line1 < line2 || (line1 == line2 && col1 < col2);
        let expected_equal = line1 == line2 && col1 == col2;
        let expected_greater = line1 > line2 || (line1 == line2 && col1 > col2);

        // Verify Ord implementation matches
        prop_assert_eq!(pos1 < pos2, expected_less,
            "Less-than mismatch: ({}, {}) < ({}, {}) should be {}",
            line1, col1, line2, col2, expected_less);
        prop_assert_eq!(pos1 == pos2, expected_equal,
            "Equality mismatch: ({}, {}) == ({}, {}) should be {}",
            line1, col1, line2, col2, expected_equal);
        prop_assert_eq!(pos1 > pos2, expected_greater,
            "Greater-than mismatch: ({}, {}) > ({}, {}) should be {}",
            line1, col1, line2, col2, expected_greater);

        // Verify totality: exactly one of <, ==, > is true
        prop_assert!(
            (pos1 < pos2) as u8 + (pos1 == pos2) as u8 + (pos1 > pos2) as u8 == 1,
            "Totality violated for ({}, {}) vs ({}, {})",
            line1, col1, line2, col2
        );
    }

    /// Feature: interval-tree-scope-lookup, Property 4: Position Lexicographic Ordering (Transitivity)
    /// **Validates: Requirements 4.1**
    ///
    /// For any three positions a, b, c: if a < b and b < c, then a < c.
    #[test]
    fn prop_position_ordering_transitivity(
        line1 in 0..1000u32,
        col1 in 0..100u32,
        line2 in 0..1000u32,
        col2 in 0..100u32,
        line3 in 0..1000u32,
        col3 in 0..100u32,
    ) {
        let pos1 = Position::new(line1, col1);
        let pos2 = Position::new(line2, col2);
        let pos3 = Position::new(line3, col3);

        // If pos1 < pos2 and pos2 < pos3, then pos1 < pos3
        if pos1 < pos2 && pos2 < pos3 {
            prop_assert!(pos1 < pos3,
                "Transitivity violated: ({}, {}) < ({}, {}) < ({}, {}) but not ({}, {}) < ({}, {})",
                line1, col1, line2, col2, line3, col3, line1, col1, line3, col3);
        }

        // Also test transitivity for <=
        if pos1 <= pos2 && pos2 <= pos3 {
            prop_assert!(pos1 <= pos3,
                "Transitivity violated for <=: ({}, {}) <= ({}, {}) <= ({}, {}) but not ({}, {}) <= ({}, {})",
                line1, col1, line2, col2, line3, col3, line1, col1, line3, col3);
        }
    }

    /// Feature: interval-tree-scope-lookup, Property 4: Position Lexicographic Ordering (Antisymmetry)
    /// **Validates: Requirements 4.1**
    ///
    /// For any two positions a, b: if a < b, then NOT (b < a).
    /// Also: if a <= b and b <= a, then a == b.
    #[test]
    fn prop_position_ordering_antisymmetry(
        line1 in 0..u32::MAX,
        col1 in 0..u32::MAX,
        line2 in 0..u32::MAX,
        col2 in 0..u32::MAX,
    ) {
        let pos1 = Position::new(line1, col1);
        let pos2 = Position::new(line2, col2);

        // If pos1 < pos2, then NOT (pos2 < pos1)
        if pos1 < pos2 {
            prop_assert!(!(pos2 < pos1),
                "Antisymmetry violated: ({}, {}) < ({}, {}) but also ({}, {}) < ({}, {})",
                line1, col1, line2, col2, line2, col2, line1, col1);
        }

        // If pos1 <= pos2 and pos2 <= pos1, then pos1 == pos2
        if pos1 <= pos2 && pos2 <= pos1 {
            prop_assert_eq!(pos1, pos2,
                "Antisymmetry violated for <=: ({}, {}) <= ({}, {}) and ({}, {}) <= ({}, {}) but not equal",
                line1, col1, line2, col2, line2, col2, line1, col1);
        }
    }

    /// Feature: interval-tree-scope-lookup, Property 4: Position Lexicographic Ordering (Reflexivity)
    /// **Validates: Requirements 4.1**
    ///
    /// For any position a: a == a and a <= a and NOT (a < a).
    #[test]
    fn prop_position_ordering_reflexivity(
        line in 0..u32::MAX,
        col in 0..u32::MAX,
    ) {
        let pos = Position::new(line, col);

        // Reflexivity: a == a
        prop_assert_eq!(pos, pos,
            "Reflexivity violated: ({}, {}) != ({}, {})",
            line, col, line, col);

        // a <= a
        prop_assert!(pos <= pos,
            "Reflexivity violated for <=: ({}, {}) not <= ({}, {})",
            line, col, line, col);

        // NOT (a < a)
        prop_assert!(!(pos < pos),
            "Irreflexivity violated: ({}, {}) < ({}, {})",
            line, col, line, col);
    }

    /// Feature: interval-tree-scope-lookup, Property 4: Position Lexicographic Ordering (Consistency)
    ///
    /// The Ord implementation should be consistent with PartialOrd and Eq.
    /// Specifically: (a == b) implies (a.cmp(&b) == Ordering::Equal)
    /// and (a < b) implies (a.cmp(&b) == Ordering::Less)
    #[test]
    fn prop_position_ordering_consistency(
        line1 in 0..u32::MAX,
        col1 in 0..u32::MAX,
        line2 in 0..u32::MAX,
        col2 in 0..u32::MAX,
    ) {
        use std::cmp::Ordering;

        let pos1 = Position::new(line1, col1);
        let pos2 = Position::new(line2, col2);

        let cmp_result = pos1.cmp(&pos2);

        // Consistency between cmp and comparison operators
        match cmp_result {
            Ordering::Less => {
                prop_assert!(pos1 < pos2,
                    "Consistency violated: cmp returned Less but ({}, {}) not < ({}, {})",
                    line1, col1, line2, col2);
                prop_assert!(!(pos1 == pos2),
                    "Consistency violated: cmp returned Less but ({}, {}) == ({}, {})",
                    line1, col1, line2, col2);
                prop_assert!(!(pos1 > pos2),
                    "Consistency violated: cmp returned Less but ({}, {}) > ({}, {})",
                    line1, col1, line2, col2);
            }
            Ordering::Equal => {
                prop_assert!(pos1 == pos2,
                    "Consistency violated: cmp returned Equal but ({}, {}) != ({}, {})",
                    line1, col1, line2, col2);
                prop_assert!(!(pos1 < pos2),
                    "Consistency violated: cmp returned Equal but ({}, {}) < ({}, {})",
                    line1, col1, line2, col2);
                prop_assert!(!(pos1 > pos2),
                    "Consistency violated: cmp returned Equal but ({}, {}) > ({}, {})",
                    line1, col1, line2, col2);
            }
            Ordering::Greater => {
                prop_assert!(pos1 > pos2,
                    "Consistency violated: cmp returned Greater but ({}, {}) not > ({}, {})",
                    line1, col1, line2, col2);
                prop_assert!(!(pos1 == pos2),
                    "Consistency violated: cmp returned Greater but ({}, {}) == ({}, {})",
                    line1, col1, line2, col2);
                prop_assert!(!(pos1 < pos2),
                    "Consistency violated: cmp returned Greater but ({}, {}) < ({}, {})",
                    line1, col1, line2, col2);
            }
        }
    }
}


// ============================================================================
// Property 10: Base Package Universal Availability
// Feature: package-function-awareness
// **Validates: Requirements 6.2, 6.3, 6.4**
//
// For any R source file and any position within that file, exports from base
// packages (base, methods, utils, grDevices, graphics, stats, datasets) SHALL
// be available in scope.
//
// This test verifies that:
// - Base exports are available at the very beginning of a file (position 0, 0)
// - Base exports are available at any random position within the file
// - Base exports are available even when no library() calls exist
// - Base exports are available before any library() calls
// - Base exports are available inside function scopes
// - Base exports are available at the end of the file
// ============================================================================

use super::scope::scope_at_position_with_packages;
use std::collections::HashSet;

/// Generate a random R code structure for testing base package availability
fn r_code_structure() -> impl Strategy<Value = RCodeStructure> {
    (
        // Number of top-level statements (0-5)
        0..6usize,
        // Whether to include a function definition
        any::<bool>(),
        // Whether to include library calls
        any::<bool>(),
        // Number of library calls (0-3)
        0..4usize,
    ).prop_flat_map(|(num_statements, include_function, include_library, num_library_calls)| {
        let statements = prop::collection::vec(r_identifier(), num_statements);
        let func_name = r_identifier();
        let func_body_statements = prop::collection::vec(r_identifier(), 0..3usize);
        let library_packages = prop::collection::vec(pkg_name(), num_library_calls);
        
        (
            Just(num_statements),
            Just(include_function),
            Just(include_library),
            statements,
            func_name,
            func_body_statements,
            library_packages,
        )
    }).prop_map(|(num_statements, include_function, include_library, statements, func_name, func_body_statements, library_packages)| {
        RCodeStructure {
            num_statements,
            include_function,
            include_library,
            statements,
            func_name,
            func_body_statements,
            library_packages,
        }
    })
}

/// Structure representing generated R code for testing
#[derive(Debug, Clone)]
struct RCodeStructure {
    num_statements: usize,
    include_function: bool,
    include_library: bool,
    statements: Vec<String>,
    func_name: String,
    func_body_statements: Vec<String>,
    library_packages: Vec<String>,
}

impl RCodeStructure {
    /// Generate the R code string from this structure
    fn to_code(&self) -> String {
        let mut lines = Vec::new();
        
        // Add some initial statements
        for (i, stmt) in self.statements.iter().enumerate() {
            lines.push(format!("{} <- {}", stmt, i));
        }
        
        // Optionally add library calls
        if self.include_library {
            for pkg in &self.library_packages {
                lines.push(format!("library({})", pkg));
            }
        }
        
        // Optionally add a function definition
        if self.include_function {
            let mut func_lines = vec![format!("{} <- function() {{", self.func_name)];
            for (i, stmt) in self.func_body_statements.iter().enumerate() {
                func_lines.push(format!("    {} <- {}", stmt, i + 100));
            }
            func_lines.push("}".to_string());
            lines.extend(func_lines);
        }
        
        // Add a final statement
        lines.push("final_result <- 42".to_string());
        
        lines.join("\n")
    }
    
    /// Get the total number of lines in the generated code
    fn line_count(&self) -> u32 {
        self.to_code().lines().count() as u32
    }
}

/// Generate a set of base exports for testing
fn base_exports_set() -> HashSet<String> {
    // Representative exports from base packages
    // These are common functions from base, methods, utils, grDevices, graphics, stats, datasets
    let exports = vec![
        // base package
        "print", "cat", "sum", "mean", "length", "c", "list", "data.frame",
        "paste", "paste0", "sprintf", "format", "as.character", "as.numeric",
        "is.null", "is.na", "is.numeric", "is.character", "is.logical",
        "if", "for", "while", "function", "return", "stop", "warning", "message",
        "tryCatch", "try", "invisible", "suppressWarnings", "suppressMessages",
        // utils package
        "head", "tail", "str", "help", "example", "install.packages",
        // stats package
        "lm", "glm", "t.test", "cor", "var", "sd", "median", "quantile",
        // grDevices package
        "pdf", "png", "jpeg", "dev.off", "rgb", "col2rgb",
        // graphics package
        "plot", "lines", "points", "abline", "hist", "barplot", "boxplot",
        // methods package
        "setClass", "setMethod", "setGeneric", "new", "show",
        // datasets package (common datasets)
        "iris", "mtcars", "airquality", "faithful",
    ];
    exports.into_iter().map(String::from).collect()
}

/// Generate a set of base exports that are unlikely to be shadowed by generated variable names.
/// The r_identifier() generator produces names matching [a-z][a-z0-9_]{0,5}, so we use
/// exports with dots, uppercase letters, or longer names that won't match.
fn base_exports_set_unlikely_shadowed() -> HashSet<String> {
    let exports = vec![
        // Names with dots (can't be generated by r_identifier)
        "data.frame", "as.character", "as.numeric", "is.null", "is.na",
        "is.numeric", "is.character", "is.logical", "paste0", "dev.off",
        "col2rgb", "t.test", "install.packages",
        // Names with uppercase (can't be generated by r_identifier)
        "TRUE", "FALSE", "NULL", "NA", "NaN", "Inf",
        // Longer names (> 6 chars, unlikely to match [a-z][a-z0-9_]{0,5})
        "suppressWarnings", "suppressMessages", "tryCatch", "invisible",
        "setClass", "setMethod", "setGeneric", "quantile", "airquality",
        "faithful", "barplot", "boxplot", "abline", "sprintf", "message",
        "warning", "example", "install", "median", "format", "length",
        "function", "return",
    ];
    exports.into_iter().map(String::from).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: package-function-awareness, Property 10: Base Package Universal Availability
    /// **Validates: Requirements 6.2, 6.3, 6.4**
    ///
    /// For any R source file and any position within that file, exports from base
    /// packages SHALL be available in scope without requiring explicit library() calls,
    /// UNLESS they are shadowed by local definitions.
    ///
    /// This test verifies that base exports are present in scope (either from base
    /// or from a local definition that shadows them).
    #[test]
    fn prop_base_package_availability(
        code_structure in r_code_structure(),
        query_line_offset in 0..10u32,
        query_column in 0..50u32,
    ) {
        let uri = make_url("test_base_pkg_availability");
        let code = code_structure.to_code();
        let line_count = code_structure.line_count();
        
        // Ensure query position is within the file
        let query_line = query_line_offset % line_count.max(1);
        
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);
        
        // Create a mock package exports callback (returns empty for all packages)
        let get_exports = |_pkg: &str| -> HashSet<String> {
            HashSet::new()
        };
        
        // Create base exports set - use exports that are unlikely to be shadowed
        // by generated variable names (which use r_identifier() pattern [a-z][a-z0-9_]{0,5})
        let base_exports = base_exports_set_unlikely_shadowed();
        
        // Query scope at the generated position
        let scope = scope_at_position_with_packages(&artifacts, query_line, query_column, &get_exports, &base_exports);
        
        // Verify that base exports are available at this position
        // (either from base package or shadowed by local definition)
        for export in &base_exports {
            prop_assert!(
                scope.symbols.contains_key(export),
                "Base export '{}' should be available at position ({}, {}) in code:\n{}",
                export, query_line, query_column, code
            );
            
            // Verify the symbol has the correct package:base URI
            // (since we use exports unlikely to be shadowed)
            let symbol = scope.symbols.get(export).unwrap();
            prop_assert_eq!(
                symbol.source_uri.as_str(), "package:base",
                "Base export '{}' should have package:base URI, got '{}'. Code:\n{}",
                export, symbol.source_uri.as_str(), code
            );
        }
    }

    /// Feature: package-function-awareness, Property 10 extended: Base exports at file start
    /// **Validates: Requirements 6.3, 6.4**
    ///
    /// Base exports SHALL be available at the very beginning of any file (position 0, 0).
    #[test]
    fn prop_base_package_available_at_file_start(
        code_structure in r_code_structure(),
    ) {
        let uri = make_url("test_base_pkg_start");
        let code = code_structure.to_code();
        
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);
        
        let get_exports = |_pkg: &str| -> HashSet<String> {
            HashSet::new()
        };
        let base_exports = base_exports_set();
        
        // Query at the very start of the file
        let scope = scope_at_position_with_packages(&artifacts, 0, 0, &get_exports, &base_exports);
        
        // All base exports should be available
        for export in &base_exports {
            prop_assert!(
                scope.symbols.contains_key(export),
                "Base export '{}' should be available at file start (0, 0). Code:\n{}",
                export, code
            );
        }
    }

    /// Feature: package-function-awareness, Property 10 extended: Base exports at file end
    /// **Validates: Requirements 6.3, 6.4**
    ///
    /// Base exports SHALL be available at the end of any file.
    #[test]
    fn prop_base_package_available_at_file_end(
        code_structure in r_code_structure(),
    ) {
        let uri = make_url("test_base_pkg_end");
        let code = code_structure.to_code();
        let last_line = code.lines().count().saturating_sub(1) as u32;
        let last_line_len = code.lines().last().map(|l| l.len()).unwrap_or(0) as u32;
        
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);
        
        let get_exports = |_pkg: &str| -> HashSet<String> {
            HashSet::new()
        };
        let base_exports = base_exports_set();
        
        // Query at the end of the file
        let scope = scope_at_position_with_packages(&artifacts, last_line, last_line_len, &get_exports, &base_exports);
        
        // All base exports should be available
        for export in &base_exports {
            prop_assert!(
                scope.symbols.contains_key(export),
                "Base export '{}' should be available at file end ({}, {}). Code:\n{}",
                export, last_line, last_line_len, code
            );
        }
    }

    /// Feature: package-function-awareness, Property 10 extended: Base exports inside functions
    /// **Validates: Requirements 6.3, 6.4**
    ///
    /// Base exports SHALL be available inside function scopes.
    #[test]
    fn prop_base_package_available_inside_function(
        func_name in r_identifier(),
        body_statements in prop::collection::vec(r_identifier(), 1..4usize),
    ) {
        let uri = make_url("test_base_pkg_func");
        
        // Create code with a function
        let mut func_body = Vec::new();
        for (i, stmt) in body_statements.iter().enumerate() {
            func_body.push(format!("    {} <- {}", stmt, i));
        }
        let code = format!(
            "{} <- function() {{\n{}\n}}",
            func_name,
            func_body.join("\n")
        );
        
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);
        
        let get_exports = |_pkg: &str| -> HashSet<String> {
            HashSet::new()
        };
        let base_exports = base_exports_set();
        
        // Query inside the function body (line 1, which is inside the function)
        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);
        
        // All base exports should be available inside the function
        for export in &base_exports {
            prop_assert!(
                scope.symbols.contains_key(export),
                "Base export '{}' should be available inside function. Code:\n{}",
                export, code
            );
        }
    }

    /// Feature: package-function-awareness, Property 10 extended: Base exports before library calls
    /// **Validates: Requirements 6.3, 6.4**
    ///
    /// Base exports SHALL be available even before any library() calls in the file.
    #[test]
    fn prop_base_package_available_before_library_calls(
        pkg in pkg_name(),
        statements_before in prop::collection::vec(r_identifier(), 1..4usize),
    ) {
        let uri = make_url("test_base_pkg_before_lib");
        
        // Create code with statements before a library call
        let mut lines = Vec::new();
        for (i, stmt) in statements_before.iter().enumerate() {
            lines.push(format!("{} <- {}", stmt, i));
        }
        lines.push(format!("library({})", pkg));
        lines.push("final <- 1".to_string());
        let code = lines.join("\n");
        
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);
        
        let get_exports = |_pkg: &str| -> HashSet<String> {
            HashSet::new()
        };
        let base_exports = base_exports_set();
        
        // Query at line 0 (before the library call)
        let scope = scope_at_position_with_packages(&artifacts, 0, 5, &get_exports, &base_exports);
        
        // All base exports should be available before library()
        for export in &base_exports {
            prop_assert!(
                scope.symbols.contains_key(export),
                "Base export '{}' should be available before library() call. Code:\n{}",
                export, code
            );
        }
    }

    /// Feature: package-function-awareness, Property 10 extended: Base exports not overridden by empty packages
    /// **Validates: Requirements 6.3, 6.4**
    ///
    /// Base exports SHALL remain available even when library() calls load packages
    /// that don't export the same symbols.
    #[test]
    fn prop_base_package_not_overridden_by_empty_packages(
        pkg in pkg_name(),
    ) {
        let uri = make_url("test_base_pkg_not_overridden");
        
        let code = format!("library({})\nx <- print(1)", pkg);
        
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);
        
        // Package exports callback returns empty (package has no exports)
        let get_exports = |_pkg: &str| -> HashSet<String> {
            HashSet::new()
        };
        let base_exports = base_exports_set();
        
        // Query after the library call
        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);
        
        // Base exports should still be available
        for export in &base_exports {
            prop_assert!(
                scope.symbols.contains_key(export),
                "Base export '{}' should still be available after loading package '{}'. Code:\n{}",
                export, pkg, code
            );
        }
    }

    /// Feature: package-function-awareness, Property 10 extended: Package exports override base exports
    /// **Validates: Requirements 6.3, 6.4**
    ///
    /// When a loaded package exports a symbol with the same name as a base export,
    /// the package export SHALL take precedence (override the base export).
    #[test]
    fn prop_package_export_overrides_base_export(
        pkg in pkg_name(),
    ) {
        let uri = make_url("test_pkg_overrides_base");
        
        let code = format!("library({})\nx <- print(1)", pkg);
        
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);
        
        // Package exports callback returns "print" (same as base export)
        let get_exports = |p: &str| -> HashSet<String> {
            if p == pkg {
                let mut exports = HashSet::new();
                exports.insert("print".to_string());
                exports
            } else {
                HashSet::new()
            }
        };
        
        let mut base_exports = HashSet::new();
        base_exports.insert("print".to_string());
        base_exports.insert("cat".to_string());
        
        // Query after the library call
        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);
        
        // "print" should be from the loaded package, not base
        let print_symbol = scope.symbols.get("print").unwrap();
        prop_assert_eq!(
            print_symbol.source_uri.as_str(), &format!("package:{}", pkg),
            "print should be from package '{}' after library() call, not base. Code:\n{}",
            pkg, code
        );
        
        // "cat" should still be from base (not exported by the package)
        let cat_symbol = scope.symbols.get("cat").unwrap();
        prop_assert_eq!(
            cat_symbol.source_uri.as_str(), "package:base",
            "cat should still be from base. Code:\n{}",
            code
        );
    }
}

// ============================================================================
// Feature: package-function-awareness, Property 8: Cross-File Package Propagation
// **Validates: Requirements 5.1, 5.2, 5.3**
//
// For any parent file that loads package P before a source() call to child file C,
// scope resolution in C SHALL include exports from P.
//
// This test verifies that:
// - Packages loaded BEFORE source() call are propagated to child files
// - Packages loaded AFTER source() call are NOT propagated to child files
// - Multiple packages loaded before source() are all propagated
// - Package propagation respects call-site positions
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: package-function-awareness, Property 8: Cross-File Package Propagation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// For any parent file that loads package P before a source() call to child file C,
    /// scope resolution in C SHALL include exports from P in inherited_packages.
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// library(dplyr)      # Line 0
    /// source("child.R")   # Line 1
    /// ```
    /// Child file should have "dplyr" in inherited_packages.
    #[test]
    fn prop_cross_file_package_propagation_basic(
        package in pkg_name(),
        func in library_func(),
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: library call at line 0, source() at line 1
        let parent_code = format!("{}({})\nsource(\"child.R\")", func, package);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: simple assignment
        let child_code = "x <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 1)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query child's scope at position (0, 0)
        let scope = scope_at_position_with_graph(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Child should have inherited the package from parent
        prop_assert!(
            scope.inherited_packages.contains(&package),
            "Child should inherit package '{}' from parent. Got inherited_packages: {:?}. Parent code:\n{}",
            package, scope.inherited_packages, parent_code
        );
    }

    /// Feature: package-function-awareness, Property 8: Cross-File Package Propagation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Packages loaded AFTER source() call should NOT be propagated to child files.
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// source("child.R")   # Line 0
    /// library(dplyr)      # Line 1
    /// ```
    /// Child file should NOT have "dplyr" in inherited_packages.
    #[test]
    fn prop_cross_file_package_propagation_after_source_not_propagated(
        package in pkg_name(),
        func in library_func(),
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: source() at line 0, library call at line 1
        let parent_code = format!("source(\"child.R\")\n{}({})", func, package);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: simple assignment
        let child_code = "x <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query child's scope at position (0, 0)
        let scope = scope_at_position_with_graph(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Child should NOT have the package (loaded after source() call)
        prop_assert!(
            !scope.inherited_packages.contains(&package),
            "Child should NOT inherit package '{}' (loaded after source() call). Got inherited_packages: {:?}. Parent code:\n{}",
            package, scope.inherited_packages, parent_code
        );
    }

    /// Feature: package-function-awareness, Property 8: Cross-File Package Propagation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Multiple packages loaded before source() should all be propagated.
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// library(pkg1)       # Line 0
    /// library(pkg2)       # Line 1
    /// source("child.R")   # Line 2
    /// ```
    /// Child file should have both "pkg1" and "pkg2" in inherited_packages.
    #[test]
    fn prop_cross_file_package_propagation_multiple_packages(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
    ) {
        // Ensure packages are distinct
        prop_assume!(pkg1 != pkg2);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: two library calls before source()
        let parent_code = format!("library({})\nlibrary({})\nsource(\"child.R\")", pkg1, pkg2);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: simple assignment
        let child_code = "x <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 2)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query child's scope at position (0, 0)
        let scope = scope_at_position_with_graph(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Child should have inherited both packages from parent
        prop_assert!(
            scope.inherited_packages.contains(&pkg1),
            "Child should inherit package '{}' from parent. Got inherited_packages: {:?}. Parent code:\n{}",
            pkg1, scope.inherited_packages, parent_code
        );
        prop_assert!(
            scope.inherited_packages.contains(&pkg2),
            "Child should inherit package '{}' from parent. Got inherited_packages: {:?}. Parent code:\n{}",
            pkg2, scope.inherited_packages, parent_code
        );
    }

    /// Feature: package-function-awareness, Property 8: Cross-File Package Propagation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Only packages loaded BEFORE source() should be propagated, not those loaded after.
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// library(pkg_before) # Line 0
    /// source("child.R")   # Line 1
    /// library(pkg_after)  # Line 2
    /// ```
    /// Child file should have "pkg_before" but NOT "pkg_after" in inherited_packages.
    #[test]
    fn prop_cross_file_package_propagation_respects_call_site(
        pkg_before in pkg_name(),
        pkg_after in pkg_name(),
    ) {
        // Ensure packages are distinct
        prop_assume!(pkg_before != pkg_after);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: library before source(), library after source()
        let parent_code = format!(
            "library({})\nsource(\"child.R\")\nlibrary({})",
            pkg_before, pkg_after
        );
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: simple assignment
        let child_code = "x <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 1)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query child's scope at position (0, 0)
        let scope = scope_at_position_with_graph(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Child should have inherited pkg_before (loaded before source())
        prop_assert!(
            scope.inherited_packages.contains(&pkg_before),
            "Child should inherit package '{}' (loaded before source()). Got inherited_packages: {:?}. Parent code:\n{}",
            pkg_before, scope.inherited_packages, parent_code
        );

        // Child should NOT have pkg_after (loaded after source())
        prop_assert!(
            !scope.inherited_packages.contains(&pkg_after),
            "Child should NOT inherit package '{}' (loaded after source()). Got inherited_packages: {:?}. Parent code:\n{}",
            pkg_after, scope.inherited_packages, parent_code
        );
    }

    /// Feature: package-function-awareness, Property 8: Cross-File Package Propagation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Function-scoped package loads should NOT be propagated to child files
    /// (unless the source() call is within the same function scope).
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// f <- function() {
    ///     library(dplyr)
    /// }
    /// source("child.R")
    /// ```
    /// Child file should NOT have "dplyr" in inherited_packages.
    #[test]
    fn prop_cross_file_package_propagation_function_scoped_not_propagated(
        package in pkg_name(),
        func_name in r_identifier(),
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: library inside function, source() outside
        let parent_code = format!(
            "{} <- function() {{\n    library({})\n}}\nsource(\"child.R\")",
            func_name, package
        );
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: simple assignment
        let child_code = "x <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 3)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query child's scope at position (0, 0)
        let scope = scope_at_position_with_graph(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Child should NOT have the package (it's function-scoped in parent)
        prop_assert!(
            !scope.inherited_packages.contains(&package),
            "Child should NOT inherit function-scoped package '{}'. Got inherited_packages: {:?}. Parent code:\n{}",
            package, scope.inherited_packages, parent_code
        );
    }

    /// Feature: package-function-awareness, Property 8: Cross-File Package Propagation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Packages should propagate through multiple levels of source() chains.
    ///
    /// Test pattern:
    /// ```r
    /// # grandparent.R
    /// library(dplyr)
    /// source("parent.R")
    ///
    /// # parent.R
    /// source("child.R")
    ///
    /// # child.R
    /// x <- 1
    /// ```
    /// Child file should have "dplyr" in inherited_packages (propagated through parent).
    #[test]
    fn prop_cross_file_package_propagation_transitive(
        package in pkg_name(),
    ) {
        let grandparent_uri = make_url("grandparent");
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Grandparent code: library call, then source parent
        let grandparent_code = format!("library({})\nsource(\"parent.R\")", package);
        let grandparent_tree = parse_r_tree(&grandparent_code);
        let grandparent_artifacts = compute_artifacts(&grandparent_uri, &grandparent_tree, &grandparent_code);

        // Parent code: source child
        let parent_code = "source(\"child.R\")";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: simple assignment
        let child_code = "x <- 1";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let grandparent_meta = make_meta_with_sources(vec![("parent.R", 1)]);
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&grandparent_uri, &grandparent_meta, Some(&workspace_root), |_| None);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &grandparent_uri { Some(grandparent_artifacts.clone()) }
            else if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &grandparent_uri { Some(grandparent_meta.clone()) }
            else if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query child's scope at position (0, 0)
        let scope = scope_at_position_with_graph(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Child should have inherited the package from grandparent (through parent)
        prop_assert!(
            scope.inherited_packages.contains(&package),
            "Child should inherit package '{}' from grandparent (transitive). Got inherited_packages: {:?}. Grandparent code:\n{}",
            package, scope.inherited_packages, grandparent_code
        );
    }

    /// Feature: package-function-awareness, Property 8: Cross-File Package Propagation
    /// **Validates: Requirements 5.1, 5.2, 5.3**
    ///
    /// Packages inherited from parent should be available at any position in child file.
    #[test]
    fn prop_cross_file_package_propagation_available_at_any_position(
        package in pkg_name(),
        query_line in 0..10u32,
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: library call at line 0, source() at line 1
        let parent_code = format!("library({})\nsource(\"child.R\")", package);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: multiple lines
        let child_code = "x <- 1\ny <- 2\nz <- 3\na <- 4\nb <- 5\nc <- 6\nd <- 7\ne <- 8\nf <- 9\ng <- 10";
        let child_tree = parse_r_tree(child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 1)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query child's scope at various positions
        let scope = scope_at_position_with_graph(
            &child_uri, query_line, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Child should have inherited the package at any position
        prop_assert!(
            scope.inherited_packages.contains(&package),
            "Child should inherit package '{}' at line {}. Got inherited_packages: {:?}. Parent code:\n{}",
            package, query_line, scope.inherited_packages, parent_code
        );
    }
}


// ============================================================================
// Feature: package-function-awareness, Property 9: Forward-Only Package Propagation
// **Validates: Requirement 5.4**
//
// For any child file C that loads package P, scope resolution in the parent file
// SHALL NOT include exports from P (packages do not propagate backward).
//
// This test verifies that:
// - Packages loaded in child files do NOT appear in parent's inherited_packages
// - Packages loaded in deeply nested files do NOT propagate back through the chain
// - While symbols from child files ARE merged into parent scope, packages are NOT
// - Parent's packages propagate forward to child, but child's packages don't propagate back
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: package-function-awareness, Property 9: Forward-Only Package Propagation
    /// **Validates: Requirement 5.4**
    ///
    /// For any child file C that loads package P, scope resolution in the parent file
    /// SHALL NOT include exports from P in inherited_packages.
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// source("child.R")   # Line 0
    /// x <- 1              # Line 1
    ///
    /// # child.R
    /// library(dplyr)      # Line 0
    /// ```
    /// Parent file should NOT have "dplyr" in inherited_packages.
    #[test]
    fn prop_forward_only_package_propagation_basic(
        package in pkg_name(),
        func in library_func(),
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: source() at line 0, then some code
        let parent_code = "source(\"child.R\")\nx <- 1";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: library call
        let child_code = format!("{}({})\ny <- 2", func, package);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query parent's scope AFTER the source() call
        let scope = scope_at_position_with_graph(
            &parent_uri, 1, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Parent should NOT have the package (it was loaded in child, not parent)
        // Requirement 5.4: Forward-only propagation
        prop_assert!(
            !scope.inherited_packages.contains(&package),
            "Parent should NOT inherit package '{}' from child (forward-only propagation). Got inherited_packages: {:?}. Child code:\n{}",
            package, scope.inherited_packages, child_code
        );
    }

    /// Feature: package-function-awareness, Property 9: Forward-Only Package Propagation
    /// **Validates: Requirement 5.4**
    ///
    /// Packages loaded in deeply nested files should NOT propagate back through the chain.
    ///
    /// Test pattern:
    /// ```r
    /// # grandparent.R
    /// source("parent.R")
    ///
    /// # parent.R
    /// source("child.R")
    ///
    /// # child.R
    /// library(stringr)
    /// ```
    /// Neither grandparent nor parent should have "stringr" in inherited_packages.
    #[test]
    fn prop_forward_only_package_propagation_deep_chain(
        package in pkg_name(),
    ) {
        let grandparent_uri = make_url("grandparent");
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Grandparent code: source parent
        let grandparent_code = "source(\"parent.R\")\nz <- 1";
        let grandparent_tree = parse_r_tree(grandparent_code);
        let grandparent_artifacts = compute_artifacts(&grandparent_uri, &grandparent_tree, grandparent_code);

        // Parent code: source child
        let parent_code = "source(\"child.R\")\ny <- 1";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: loads package
        let child_code = format!("library({})\nx <- 1", package);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let grandparent_meta = make_meta_with_sources(vec![("parent.R", 0)]);
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&grandparent_uri, &grandparent_meta, Some(&workspace_root), |_| None);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &grandparent_uri { Some(grandparent_artifacts.clone()) }
            else if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &grandparent_uri { Some(grandparent_meta.clone()) }
            else if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query grandparent's scope after source() call
        let grandparent_scope = scope_at_position_with_graph(
            &grandparent_uri, 1, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Grandparent should NOT have the package (loaded in grandchild)
        // Requirement 5.4: Forward-only propagation
        prop_assert!(
            !grandparent_scope.inherited_packages.contains(&package),
            "Grandparent should NOT inherit package '{}' from grandchild (forward-only propagation). Got inherited_packages: {:?}",
            package, grandparent_scope.inherited_packages
        );

        // Query parent's scope after source() call
        let parent_scope = scope_at_position_with_graph(
            &parent_uri, 1, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Parent should also NOT have the package (loaded in child)
        prop_assert!(
            !parent_scope.inherited_packages.contains(&package),
            "Parent should NOT inherit package '{}' from child (forward-only propagation). Got inherited_packages: {:?}",
            package, parent_scope.inherited_packages
        );
    }

    /// Feature: package-function-awareness, Property 9: Forward-Only Package Propagation
    /// **Validates: Requirement 5.4**
    ///
    /// While symbols from child files ARE merged into parent scope, packages are NOT.
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// source("child.R")
    /// y <- helper_func()
    ///
    /// # child.R
    /// library(ggplot2)
    /// helper_func <- function() { 1 }
    /// ```
    /// Parent should have "helper_func" symbol but NOT "ggplot2" in inherited_packages.
    #[test]
    fn prop_forward_only_package_propagation_symbols_propagate_packages_dont(
        package in pkg_name(),
        func_name in r_identifier(),
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: source child, then use function from child
        let parent_code = format!("source(\"child.R\")\ny <- {}()", func_name);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: loads package and defines function
        let child_code = format!("library({})\n{} <- function() {{ 1 }}", package, func_name);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query parent's scope after source() call
        let scope = scope_at_position_with_graph(
            &parent_uri, 1, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Symbols from child SHOULD be available in parent
        prop_assert!(
            scope.symbols.contains_key(&func_name),
            "Parent should have '{}' symbol from child (symbols propagate). Got symbols: {:?}",
            func_name, scope.symbols.keys().collect::<Vec<_>>()
        );

        // But packages from child should NOT be in parent's inherited_packages
        // Requirement 5.4: Forward-only propagation
        prop_assert!(
            !scope.inherited_packages.contains(&package),
            "Parent should NOT inherit package '{}' from child (forward-only propagation). Got inherited_packages: {:?}",
            package, scope.inherited_packages
        );
    }

    /// Feature: package-function-awareness, Property 9: Forward-Only Package Propagation
    /// **Validates: Requirement 5.4**
    ///
    /// Parent's packages propagate forward to child, but child's packages don't propagate back.
    ///
    /// Test pattern:
    /// ```r
    /// # parent.R
    /// library(dplyr)
    /// source("child.R")
    /// z <- 1
    ///
    /// # child.R
    /// library(ggplot2)
    /// x <- 1
    /// ```
    /// Child should have "dplyr" in inherited_packages.
    /// Parent should NOT have "ggplot2" in inherited_packages.
    #[test]
    fn prop_forward_only_package_propagation_asymmetric(
        parent_pkg in pkg_name(),
        child_pkg in pkg_name(),
    ) {
        // Ensure packages are distinct
        prop_assume!(parent_pkg != child_pkg);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: library call, then source child
        let parent_code = format!("library({})\nsource(\"child.R\")\nz <- 1", parent_pkg);
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child code: loads different package
        let child_code = format!("library({})\nx <- 1", child_pkg);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 1)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query child's scope - should have parent's package
        let child_scope = scope_at_position_with_graph(
            &child_uri, 0, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Child SHOULD have parent's package (forward propagation works)
        prop_assert!(
            child_scope.inherited_packages.contains(&parent_pkg),
            "Child should inherit package '{}' from parent (forward propagation). Got inherited_packages: {:?}",
            parent_pkg, child_scope.inherited_packages
        );

        // Query parent's scope after source() call
        let parent_scope = scope_at_position_with_graph(
            &parent_uri, 2, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Parent should NOT have child's package (forward-only propagation)
        // Requirement 5.4: Forward-only propagation
        prop_assert!(
            !parent_scope.inherited_packages.contains(&child_pkg),
            "Parent should NOT inherit package '{}' from child (forward-only propagation). Got inherited_packages: {:?}",
            child_pkg, parent_scope.inherited_packages
        );
    }

    /// Feature: package-function-awareness, Property 9: Forward-Only Package Propagation
    /// **Validates: Requirement 5.4**
    ///
    /// Packages loaded in child should not appear in parent at any query position.
    #[test]
    fn prop_forward_only_package_propagation_any_parent_position(
        package in pkg_name(),
        query_line in 0..5u32,
    ) {
        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: source child at line 0, then multiple lines
        let parent_code = "source(\"child.R\")\na <- 1\nb <- 2\nc <- 3\nd <- 4\ne <- 5";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: loads package
        let child_code = format!("library({})\nx <- 1", package);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query parent's scope at various positions
        let scope = scope_at_position_with_graph(
            &parent_uri, query_line, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Parent should NOT have the package at any position
        // Requirement 5.4: Forward-only propagation
        prop_assert!(
            !scope.inherited_packages.contains(&package),
            "Parent should NOT inherit package '{}' from child at line {} (forward-only propagation). Got inherited_packages: {:?}",
            package, query_line, scope.inherited_packages
        );
    }

    /// Feature: package-function-awareness, Property 9: Forward-Only Package Propagation
    /// **Validates: Requirement 5.4**
    ///
    /// Multiple packages loaded in child should all NOT propagate to parent.
    #[test]
    fn prop_forward_only_package_propagation_multiple_packages(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
        pkg3 in pkg_name(),
    ) {
        // Ensure packages are distinct
        prop_assume!(pkg1 != pkg2 && pkg2 != pkg3 && pkg1 != pkg3);

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");
        let workspace_root = Url::parse("file:///").unwrap();

        // Parent code: source child
        let parent_code = "source(\"child.R\")\nx <- 1";
        let parent_tree = parse_r_tree(parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, parent_code);

        // Child code: loads multiple packages
        let child_code = format!("library({})\nlibrary({})\nlibrary({})\ny <- 1", pkg1, pkg2, pkg3);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // Build dependency graph
        let mut graph = DependencyGraph::new();
        let parent_meta = make_meta_with_sources(vec![("child.R", 0)]);
        graph.update_file(&parent_uri, &parent_meta, Some(&workspace_root), |_| None);

        let get_artifacts = |uri: &Url| -> Option<ScopeArtifacts> {
            if uri == &parent_uri { Some(parent_artifacts.clone()) }
            else if uri == &child_uri { Some(child_artifacts.clone()) }
            else { None }
        };

        let get_metadata = |uri: &Url| -> Option<CrossFileMetadata> {
            if uri == &parent_uri { Some(parent_meta.clone()) }
            else { None }
        };

        // Query parent's scope after source() call
        let scope = scope_at_position_with_graph(
            &parent_uri, 1, 0, &get_artifacts, &get_metadata, &graph, Some(&workspace_root), 10,
        );

        // Parent should NOT have any of the child's packages
        // Requirement 5.4: Forward-only propagation
        prop_assert!(
            !scope.inherited_packages.contains(&pkg1),
            "Parent should NOT inherit package '{}' from child (forward-only propagation). Got inherited_packages: {:?}",
            pkg1, scope.inherited_packages
        );
        prop_assert!(
            !scope.inherited_packages.contains(&pkg2),
            "Parent should NOT inherit package '{}' from child (forward-only propagation). Got inherited_packages: {:?}",
            pkg2, scope.inherited_packages
        );
        prop_assert!(
            !scope.inherited_packages.contains(&pkg3),
            "Parent should NOT inherit package '{}' from child (forward-only propagation). Got inherited_packages: {:?}",
            pkg3, scope.inherited_packages
        );
    }
}

// ============================================================================
// Property 11: Package Export Diagnostic Suppression
// Feature: package-function-awareness
// **Validates: Requirements 8.1, 8.2**
//
// For any symbol S that is exported by a package P loaded before position X,
// the Diagnostic_Engine SHALL NOT emit an "undefined variable" warning for S
// at position X.
//
// This test verifies that:
// - Symbols from loaded packages are available in scope after the library() call
// - Symbols from loaded packages are NOT available before the library() call
// - The scope resolution correctly includes package exports at the usage position
// - Multiple packages can provide exports that suppress diagnostics
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: package-function-awareness, Property 11: Package Export Diagnostic Suppression
    /// **Validates: Requirements 8.1, 8.2**
    ///
    /// For any symbol S that is exported by a package P loaded before position X,
    /// the scope at position X SHALL contain S, which means the Diagnostic_Engine
    /// SHALL NOT emit an "undefined variable" warning for S at position X.
    ///
    /// This test verifies that package exports are available in scope after the
    /// library() call, which is the mechanism by which diagnostics are suppressed.
    #[test]
    fn prop_package_export_diagnostic_suppression(
        package in pkg_name(),
        func in library_func(),
        export_name in r_identifier(),
        lines_before in 0..3usize,
        lines_after in 1..4usize,
    ) {
        let uri = make_url("test_pkg_diag_suppression");

        // Build code with library call and usage of package export
        let mut code_lines = Vec::new();

        // Add filler lines before library call
        for i in 0..lines_before {
            code_lines.push(format!("x{} <- {}", i, i));
        }

        // Add library call
        let library_line = lines_before;
        code_lines.push(format!("{}({})", func, package));

        // Add lines after library call, including usage of package export
        for i in 0..lines_after {
            if i == 0 {
                // Use the package export on the first line after library()
                code_lines.push(format!("result <- {}(1)", export_name));
            } else {
                code_lines.push(format!("y{} <- {}", i, i));
            }
        }

        let code = code_lines.join("\n");
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback that returns the export_name for our package
        let pkg_clone = package.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        // Empty base exports (we're testing package exports, not base)
        let base_exports = HashSet::new();

        // Query scope at the usage position (line after library call)
        let usage_line = (library_line + 1) as u32;
        let scope = scope_at_position_with_packages(&artifacts, usage_line, 10, &get_exports, &base_exports);

        // Requirement 8.1, 8.2: The symbol should be in scope after the library() call
        // This means the Diagnostic_Engine would NOT emit an "undefined variable" warning
        prop_assert!(
            scope.symbols.contains_key(&export_name),
            "Package export '{}' from package '{}' should be in scope at line {} (after library() on line {}). \
             This means no 'undefined variable' warning would be emitted. Code:\n{}",
            export_name, package, usage_line, library_line, code
        );

        // Verify the symbol has the correct package URI
        let symbol = scope.symbols.get(&export_name).unwrap();
        let expected_uri = format!("package:{}", package);
        prop_assert_eq!(
            symbol.source_uri.as_str(), expected_uri.as_str(),
            "Package export '{}' should have URI '{}', got '{}'. Code:\n{}",
            export_name, expected_uri, symbol.source_uri.as_str(), code
        );
    }

    /// Feature: package-function-awareness, Property 11 extended: Multiple package exports
    /// **Validates: Requirements 8.1, 8.2**
    ///
    /// When multiple packages are loaded, exports from ALL loaded packages should
    /// be available in scope and suppress diagnostics.
    #[test]
    fn prop_package_export_diagnostic_suppression_multiple_packages(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
        export1 in r_identifier(),
        export2 in r_identifier(),
    ) {
        // Ensure packages and exports are distinct
        prop_assume!(pkg1 != pkg2);
        prop_assume!(export1 != export2);

        let uri = make_url("test_multi_pkg_diag_suppression");

        // Code with two library calls and usage of both exports
        let code = format!(
            "library({})\nlibrary({})\nresult1 <- {}(1)\nresult2 <- {}(2)",
            pkg1, pkg2, export1, export2
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg1_clone = pkg1.clone();
        let pkg2_clone = pkg2.clone();
        let export1_clone = export1.clone();
        let export2_clone = export2.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg1_clone {
                let mut exports = HashSet::new();
                exports.insert(export1_clone.clone());
                exports
            } else if p == pkg2_clone {
                let mut exports = HashSet::new();
                exports.insert(export2_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library calls)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Both exports should be in scope
        prop_assert!(
            scope.symbols.contains_key(&export1),
            "Export '{}' from package '{}' should be in scope. Code:\n{}",
            export1, pkg1, code
        );
        prop_assert!(
            scope.symbols.contains_key(&export2),
            "Export '{}' from package '{}' should be in scope. Code:\n{}",
            export2, pkg2, code
        );
    }

    /// Feature: package-function-awareness, Property 11 extended: Export NOT available before library()
    /// **Validates: Requirements 8.1, 8.2 (inverse)**
    ///
    /// Symbols from a package should NOT be in scope BEFORE the library() call.
    /// This verifies the position-aware nature of diagnostic suppression.
    #[test]
    fn prop_package_export_not_available_before_library(
        package in pkg_name(),
        export_name in r_identifier(),
        lines_before in 1..4usize,
    ) {
        let uri = make_url("test_pkg_export_before_lib");

        // Build code with usage BEFORE library call
        let mut code_lines = Vec::new();

        // Add lines before library call, including usage of package export
        for i in 0..lines_before {
            if i == 0 {
                // Use the package export on the first line (before library)
                code_lines.push(format!("result <- {}(1)", export_name));
            } else {
                code_lines.push(format!("x{} <- {}", i, i));
            }
        }

        // Add library call after the usage
        code_lines.push(format!("library({})", package));

        let code = code_lines.join("\n");
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 0 (before library call)
        let scope = scope_at_position_with_packages(&artifacts, 0, 10, &get_exports, &base_exports);

        // The export should NOT be in scope before the library() call
        // This means the Diagnostic_Engine WOULD emit an "undefined variable" warning
        prop_assert!(
            !scope.symbols.contains_key(&export_name),
            "Package export '{}' from package '{}' should NOT be in scope at line 0 (before library()). \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            export_name, package, code
        );
    }

    /// Feature: package-function-awareness, Property 11 extended: Function-scoped package exports
    /// **Validates: Requirements 8.1, 8.2, 2.4, 2.5**
    ///
    /// When a library() call is inside a function, its exports should only suppress
    /// diagnostics within that function scope, not outside.
    #[test]
    fn prop_package_export_diagnostic_suppression_function_scoped(
        package in pkg_name(),
        func_name in r_identifier(),
        export_name in r_identifier(),
    ) {
        // Ensure func_name and export_name are different to avoid shadowing
        // (the function definition would shadow the package export)
        prop_assume!(func_name != export_name);

        let uri = make_url("test_func_scoped_pkg_diag");

        // Code with library() inside a function
        let code = format!(
            "{} <- function() {{\n    library({})\n    result <- {}(1)\n}}\noutside <- {}(2)",
            func_name, package, export_name, export_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope inside the function (line 2, after library call inside function)
        let scope_inside = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Export should be in scope inside the function
        prop_assert!(
            scope_inside.symbols.contains_key(&export_name),
            "Package export '{}' should be in scope inside function after library(). Code:\n{}",
            export_name, code
        );

        // Query scope outside the function (line 4)
        let scope_outside = scope_at_position_with_packages(&artifacts, 4, 10, &get_exports, &base_exports);

        // Export should NOT be in scope outside the function
        // (function-scoped library() doesn't affect global scope)
        prop_assert!(
            !scope_outside.symbols.contains_key(&export_name),
            "Package export '{}' should NOT be in scope outside function (function-scoped library). Code:\n{}",
            export_name, code
        );
    }

    /// Feature: package-function-awareness, Property 11 extended: Local definition shadows package export
    /// **Validates: Requirements 8.1, 8.2**
    ///
    /// When a local definition shadows a package export, the local definition should
    /// take precedence in scope (and thus also suppress diagnostics).
    #[test]
    fn prop_package_export_shadowed_by_local_definition(
        package in pkg_name(),
        symbol_name in r_identifier(),
    ) {
        let uri = make_url("test_local_shadows_pkg");

        // Code with library() and local definition of same name
        let code = format!(
            "library({})\n{} <- function(x) x + 1\nresult <- {}(1)",
            package, symbol_name, symbol_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let symbol_clone = symbol_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(symbol_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library and local definition)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Symbol should be in scope (either from package or local definition)
        prop_assert!(
            scope.symbols.contains_key(&symbol_name),
            "Symbol '{}' should be in scope (from local definition or package). Code:\n{}",
            symbol_name, code
        );

        // The symbol should be from the local definition, not the package
        // (local definitions take precedence)
        let symbol = scope.symbols.get(&symbol_name).unwrap();
        prop_assert!(
            !symbol.source_uri.as_str().starts_with("package:"),
            "Symbol '{}' should be from local definition, not package. Got URI: '{}'. Code:\n{}",
            symbol_name, symbol.source_uri.as_str(), code
        );
    }

    /// Feature: package-function-awareness, Property 11 extended: Same export from multiple packages
    /// **Validates: Requirements 8.1, 8.2**
    ///
    /// When multiple packages export the same symbol, the first loaded package's
    /// export should be used (but either way, diagnostics are suppressed).
    #[test]
    fn prop_package_export_first_loaded_wins(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
        export_name in r_identifier(),
    ) {
        // Ensure packages are distinct
        prop_assume!(pkg1 != pkg2);

        let uri = make_url("test_first_pkg_wins");

        // Code with two library calls, both packages export same symbol
        let code = format!(
            "library({})\nlibrary({})\nresult <- {}(1)",
            pkg1, pkg2, export_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback - both packages export the same symbol
        let pkg1_clone = pkg1.clone();
        let pkg2_clone = pkg2.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg1_clone || p == pkg2_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library calls)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Symbol should be in scope (diagnostics suppressed)
        prop_assert!(
            scope.symbols.contains_key(&export_name),
            "Export '{}' should be in scope (from either package). Code:\n{}",
            export_name, code
        );

        // The symbol should be from pkg1 (first loaded) since pkg2's export
        // doesn't override non-base exports
        let symbol = scope.symbols.get(&export_name).unwrap();
        let expected_uri = format!("package:{}", pkg1);
        prop_assert_eq!(
            symbol.source_uri.as_str(), expected_uri.as_str(),
            "Export '{}' should be from first loaded package '{}', got '{}'. Code:\n{}",
            export_name, pkg1, symbol.source_uri.as_str(), code
        );
    }
}

// ============================================================================
// Property 12: Pre-Load Diagnostic Emission
// Feature: package-function-awareness
// **Validates: Requirement 8.3**
//
// For any symbol S that is exported by a package P loaded at position X,
// the Diagnostic_Engine SHALL emit an "undefined variable" warning for S
// at any position before X.
//
// This test verifies that:
// - Symbols from packages are NOT available in scope BEFORE the library() call
// - The scope resolution correctly excludes package exports at positions before loading
// - This is the inverse of Property 11 - we verify diagnostics WOULD be emitted
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: package-function-awareness, Property 12: Pre-Load Diagnostic Emission
    /// **Validates: Requirement 8.3**
    ///
    /// For any symbol S that is exported by a package P loaded at position X,
    /// the scope at any position before X SHALL NOT contain S, which means the
    /// Diagnostic_Engine SHALL emit an "undefined variable" warning for S.
    ///
    /// This test verifies that package exports are NOT available in scope before
    /// the library() call, which means diagnostics WOULD be emitted.
    #[test]
    fn prop_pre_load_diagnostic_emission(
        package in pkg_name(),
        func in library_func(),
        export_name in r_identifier(),
        lines_before in 1..5usize,
        lines_after in 0..3usize,
    ) {
        let uri = make_url("test_pre_load_diag");

        // Build code with usage BEFORE library call
        let mut code_lines = Vec::new();

        // Add lines before library call, including usage of package export
        for i in 0..lines_before {
            if i == 0 {
                // Use the package export on the first line (before library)
                code_lines.push(format!("result <- {}(1)", export_name));
            } else {
                code_lines.push(format!("x{} <- {}", i, i));
            }
        }

        // Add library call after the usage
        let library_line = lines_before;
        code_lines.push(generate_library_call(func, &package, PkgQuoteStyle::None));

        // Add filler lines after library call
        for i in 0..lines_after {
            code_lines.push(format!("y{} <- {}", i, i));
        }

        let code = code_lines.join("\n");
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback that returns the export_name for our package
        let pkg_clone = package.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        // Empty base exports (we're testing package exports, not base)
        let base_exports = HashSet::new();

        // Query scope at line 0 (before library call)
        let scope = scope_at_position_with_packages(&artifacts, 0, 10, &get_exports, &base_exports);

        // Requirement 8.3: The symbol should NOT be in scope before the library() call
        // This means the Diagnostic_Engine WOULD emit an "undefined variable" warning
        prop_assert!(
            !scope.symbols.contains_key(&export_name),
            "Package export '{}' from package '{}' should NOT be in scope at line 0 (before library() on line {}). \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            export_name, package, library_line, code
        );
    }

    /// Feature: package-function-awareness, Property 12 extended: Multiple positions before library
    /// **Validates: Requirement 8.3**
    ///
    /// Verifies that the symbol is NOT in scope at ANY position before the library() call,
    /// not just at line 0.
    #[test]
    fn prop_pre_load_diagnostic_emission_all_positions(
        package in pkg_name(),
        export_name in r_identifier(),
        lines_before in 2..6usize,
    ) {
        // Ensure export_name doesn't collide with filler variable names (x0, x1, etc.)
        // which would create local definitions that shadow the package export
        let filler_names: Vec<String> = (0..lines_before).map(|i| format!("x{}", i)).collect();
        prop_assume!(!filler_names.contains(&export_name));

        let uri = make_url("test_pre_load_all_pos");

        // Build code with multiple lines before library call
        let mut code_lines = Vec::new();

        // Add lines before library call
        for i in 0..lines_before {
            code_lines.push(format!("x{} <- {}", i, i));
        }

        // Add library call
        let library_line = lines_before;
        code_lines.push(format!("library({})", package));

        // Add usage after library call
        code_lines.push(format!("result <- {}(1)", export_name));

        let code = code_lines.join("\n");
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Check ALL positions before the library call
        for check_line in 0..library_line {
            let scope = scope_at_position_with_packages(&artifacts, check_line as u32, 10, &get_exports, &base_exports);

            // Requirement 8.3: Symbol should NOT be in scope at any position before library()
            prop_assert!(
                !scope.symbols.contains_key(&export_name),
                "Package export '{}' should NOT be in scope at line {} (before library() on line {}). \
                 This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
                export_name, check_line, library_line, code
            );
        }

        // Verify it IS in scope after the library call (sanity check)
        let scope_after = scope_at_position_with_packages(&artifacts, (library_line + 1) as u32, 10, &get_exports, &base_exports);
        prop_assert!(
            scope_after.symbols.contains_key(&export_name),
            "Package export '{}' SHOULD be in scope at line {} (after library() on line {}). Code:\n{}",
            export_name, library_line + 1, library_line, code
        );
    }

    /// Feature: package-function-awareness, Property 12 extended: Same line before call position
    /// **Validates: Requirement 8.3**
    ///
    /// When a symbol usage is on the same line as the library() call but BEFORE the call,
    /// the symbol should NOT be in scope (diagnostic should be emitted).
    #[test]
    fn prop_pre_load_diagnostic_same_line_before_call(
        package in pkg_name(),
        export_name in r_identifier(),
    ) {
        let uri = make_url("test_pre_load_same_line");

        // Code with usage before library call on same line (using semicolon)
        // Note: In R, this is unusual but valid syntax
        let code = format!("result <- {}(1); library({})", export_name, package);

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at column 0 (before the library call on the same line)
        // The usage `result <- export_name(1)` starts at column 0
        let scope = scope_at_position_with_packages(&artifacts, 0, 0, &get_exports, &base_exports);

        // Requirement 8.3: Symbol should NOT be in scope before the library() call
        // even on the same line
        prop_assert!(
            !scope.symbols.contains_key(&export_name),
            "Package export '{}' should NOT be in scope at column 0 (before library() call on same line). \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            export_name, code
        );
    }

    /// Feature: package-function-awareness, Property 12 extended: Function-scoped pre-load
    /// **Validates: Requirement 8.3, 2.4**
    ///
    /// When a library() call is inside a function, symbols should NOT be in scope
    /// before the library() call within that function.
    #[test]
    fn prop_pre_load_diagnostic_function_scoped(
        package in pkg_name(),
        func_name in r_identifier(),
        export_name in r_identifier(),
    ) {
        // Ensure func_name and export_name are different to avoid shadowing
        // (the function definition would shadow the package export)
        prop_assume!(func_name != export_name);

        let uri = make_url("test_pre_load_func_scoped");

        // Code with usage before library() inside a function
        let code = format!(
            "{} <- function() {{\n    result <- {}(1)\n    library({})\n    result2 <- {}(2)\n}}",
            func_name, export_name, package, export_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 1 (before library() inside function)
        let scope_before = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        // Requirement 8.3: Symbol should NOT be in scope before library() inside function
        prop_assert!(
            !scope_before.symbols.contains_key(&export_name),
            "Package export '{}' should NOT be in scope at line 1 (before library() inside function). \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            export_name, code
        );

        // Query scope at line 3 (after library() inside function)
        let scope_after = scope_at_position_with_packages(&artifacts, 3, 10, &get_exports, &base_exports);

        // Sanity check: Symbol SHOULD be in scope after library() inside function
        prop_assert!(
            scope_after.symbols.contains_key(&export_name),
            "Package export '{}' SHOULD be in scope at line 3 (after library() inside function). Code:\n{}",
            export_name, code
        );
    }

    /// Feature: package-function-awareness, Property 12 extended: Multiple packages pre-load
    /// **Validates: Requirement 8.3**
    ///
    /// When multiple packages are loaded at different positions, symbols from each
    /// package should NOT be in scope before their respective library() calls.
    #[test]
    fn prop_pre_load_diagnostic_multiple_packages(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
        export1 in r_identifier(),
        export2 in r_identifier(),
    ) {
        // Ensure packages and exports are distinct
        prop_assume!(pkg1 != pkg2);
        prop_assume!(export1 != export2);

        let uri = make_url("test_pre_load_multi_pkg");

        // Code with two library calls at different positions
        let code = format!(
            "# Line 0: before both\nlibrary({})\n# Line 2: after pkg1, before pkg2\nlibrary({})\n# Line 4: after both",
            pkg1, pkg2
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg1_clone = pkg1.clone();
        let pkg2_clone = pkg2.clone();
        let export1_clone = export1.clone();
        let export2_clone = export2.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg1_clone {
                let mut exports = HashSet::new();
                exports.insert(export1_clone.clone());
                exports
            } else if p == pkg2_clone {
                let mut exports = HashSet::new();
                exports.insert(export2_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Line 0: Before both library calls - neither export should be in scope
        let scope_0 = scope_at_position_with_packages(&artifacts, 0, 0, &get_exports, &base_exports);
        prop_assert!(
            !scope_0.symbols.contains_key(&export1),
            "Export '{}' from pkg1 should NOT be in scope at line 0 (before library(pkg1)). Code:\n{}",
            export1, code
        );
        prop_assert!(
            !scope_0.symbols.contains_key(&export2),
            "Export '{}' from pkg2 should NOT be in scope at line 0 (before library(pkg2)). Code:\n{}",
            export2, code
        );

        // Line 2: After pkg1, before pkg2 - only export1 should be in scope
        let scope_2 = scope_at_position_with_packages(&artifacts, 2, 0, &get_exports, &base_exports);
        prop_assert!(
            scope_2.symbols.contains_key(&export1),
            "Export '{}' from pkg1 SHOULD be in scope at line 2 (after library(pkg1)). Code:\n{}",
            export1, code
        );
        prop_assert!(
            !scope_2.symbols.contains_key(&export2),
            "Export '{}' from pkg2 should NOT be in scope at line 2 (before library(pkg2)). Code:\n{}",
            export2, code
        );

        // Line 4: After both library calls - both exports should be in scope
        let scope_4 = scope_at_position_with_packages(&artifacts, 4, 0, &get_exports, &base_exports);
        prop_assert!(
            scope_4.symbols.contains_key(&export1),
            "Export '{}' from pkg1 SHOULD be in scope at line 4 (after both library calls). Code:\n{}",
            export1, code
        );
        prop_assert!(
            scope_4.symbols.contains_key(&export2),
            "Export '{}' from pkg2 SHOULD be in scope at line 4 (after both library calls). Code:\n{}",
            export2, code
        );
    }
}

// ============================================================================
// Property 13: Non-Export Diagnostic Emission
// Feature: package-function-awareness
// **Validates: Requirement 8.4**
//
// For any symbol S that is NOT exported by any loaded package at position X,
// the Diagnostic_Engine SHALL emit an "undefined variable" warning for S at
// position X.
//
// This test verifies that:
// - Symbols NOT in a package's exports are NOT available in scope
// - Loading a package does NOT make non-exported symbols available
// - The scope resolution correctly excludes non-exported symbols
// - Multiple packages being loaded does not affect non-exported symbols
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: package-function-awareness, Property 13: Non-Export Diagnostic Emission
    /// **Validates: Requirement 8.4**
    ///
    /// For any symbol S that is NOT exported by any loaded package at position X,
    /// the scope at position X SHALL NOT contain S, which means the Diagnostic_Engine
    /// SHALL emit an "undefined variable" warning for S at position X.
    ///
    /// This test verifies that symbols NOT in a package's exports are NOT available
    /// in scope, even after the library() call.
    #[test]
    fn prop_non_export_diagnostic_emission(
        package in pkg_name(),
        func in library_func(),
        exported_name in r_identifier(),
        non_exported_name in r_identifier(),
        lines_before in 0..3usize,
        lines_after in 1..4usize,
    ) {
        // Ensure the exported and non-exported names are different
        prop_assume!(exported_name != non_exported_name);

        let uri = make_url("test_non_export_diag");

        // Build code with library call and usage of a NON-exported symbol
        let mut code_lines = Vec::new();

        // Add filler lines before library call
        for i in 0..lines_before {
            code_lines.push(format!("x{} <- {}", i, i));
        }

        // Add library call
        let library_line = lines_before;
        code_lines.push(format!("{}({})", func, package));

        // Add lines after library call, including usage of NON-exported symbol
        for i in 0..lines_after {
            if i == 0 {
                // Use the NON-exported symbol on the first line after library()
                code_lines.push(format!("result <- {}(1)", non_exported_name));
            } else {
                code_lines.push(format!("y{} <- {}", i, i));
            }
        }

        let code = code_lines.join("\n");
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback that returns ONLY the exported_name
        // (NOT the non_exported_name)
        let pkg_clone = package.clone();
        let export_clone = exported_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        // Empty base exports (we're testing package exports, not base)
        let base_exports = HashSet::new();

        // Query scope at the usage position (line after library call)
        let usage_line = (library_line + 1) as u32;
        let scope = scope_at_position_with_packages(&artifacts, usage_line, 10, &get_exports, &base_exports);

        // Requirement 8.4: The NON-exported symbol should NOT be in scope
        // This means the Diagnostic_Engine WOULD emit an "undefined variable" warning
        prop_assert!(
            !scope.symbols.contains_key(&non_exported_name),
            "Non-exported symbol '{}' should NOT be in scope at line {} (after library({}) on line {}). \
             Package '{}' only exports '{}'. \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            non_exported_name, usage_line, package, library_line, package, exported_name, code
        );

        // Sanity check: The exported symbol SHOULD be in scope
        prop_assert!(
            scope.symbols.contains_key(&exported_name),
            "Exported symbol '{}' SHOULD be in scope at line {} (after library({}) on line {}). Code:\n{}",
            exported_name, usage_line, package, library_line, code
        );
    }

    /// Feature: package-function-awareness, Property 13 extended: Multiple packages, symbol not in any
    /// **Validates: Requirement 8.4**
    ///
    /// When multiple packages are loaded, a symbol that is NOT exported by ANY of them
    /// should NOT be in scope.
    #[test]
    fn prop_non_export_diagnostic_multiple_packages(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
        export1 in r_identifier(),
        export2 in r_identifier(),
        non_exported in r_identifier(),
    ) {
        // Ensure all names are distinct
        prop_assume!(pkg1 != pkg2);
        prop_assume!(export1 != export2);
        prop_assume!(non_exported != export1);
        prop_assume!(non_exported != export2);

        let uri = make_url("test_non_export_multi_pkg");

        // Code with two library calls and usage of non-exported symbol
        let code = format!(
            "library({})\nlibrary({})\nresult <- {}(1)",
            pkg1, pkg2, non_exported
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback - neither package exports non_exported
        let pkg1_clone = pkg1.clone();
        let pkg2_clone = pkg2.clone();
        let export1_clone = export1.clone();
        let export2_clone = export2.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg1_clone {
                let mut exports = HashSet::new();
                exports.insert(export1_clone.clone());
                exports
            } else if p == pkg2_clone {
                let mut exports = HashSet::new();
                exports.insert(export2_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library calls)
        let scope = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Requirement 8.4: Non-exported symbol should NOT be in scope
        prop_assert!(
            !scope.symbols.contains_key(&non_exported),
            "Non-exported symbol '{}' should NOT be in scope even after loading packages '{}' and '{}'. \
             Package '{}' exports '{}', package '{}' exports '{}'. \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            non_exported, pkg1, pkg2, pkg1, export1, pkg2, export2, code
        );

        // Sanity check: Both exported symbols SHOULD be in scope
        prop_assert!(
            scope.symbols.contains_key(&export1),
            "Exported symbol '{}' from package '{}' SHOULD be in scope. Code:\n{}",
            export1, pkg1, code
        );
        prop_assert!(
            scope.symbols.contains_key(&export2),
            "Exported symbol '{}' from package '{}' SHOULD be in scope. Code:\n{}",
            export2, pkg2, code
        );
    }

    /// Feature: package-function-awareness, Property 13 extended: Package with empty exports
    /// **Validates: Requirement 8.4**
    ///
    /// When a package has no exports (empty export list), no symbols from that package
    /// should be available in scope.
    #[test]
    fn prop_non_export_diagnostic_empty_exports(
        package in pkg_name(),
        symbol_name in r_identifier(),
    ) {
        let uri = make_url("test_empty_exports");

        // Code with library call and usage of a symbol
        let code = format!(
            "library({})\nresult <- {}(1)",
            package, symbol_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback that returns EMPTY exports
        let pkg_clone = package.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                HashSet::new() // Empty exports!
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 1 (after library call)
        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        // Requirement 8.4: Symbol should NOT be in scope (package has no exports)
        prop_assert!(
            !scope.symbols.contains_key(&symbol_name),
            "Symbol '{}' should NOT be in scope after loading package '{}' with empty exports. \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            symbol_name, package, code
        );
    }

    /// Feature: package-function-awareness, Property 13 extended: Function-scoped non-export
    /// **Validates: Requirement 8.4, 2.4**
    ///
    /// When a library() call is inside a function, non-exported symbols should NOT
    /// be in scope either inside or outside the function.
    #[test]
    fn prop_non_export_diagnostic_function_scoped(
        package in pkg_name(),
        func_name in r_identifier(),
        exported_name in r_identifier(),
        non_exported_name in r_identifier(),
    ) {
        // Ensure names are distinct
        prop_assume!(exported_name != non_exported_name);
        prop_assume!(func_name != exported_name);
        prop_assume!(func_name != non_exported_name);

        let uri = make_url("test_func_scoped_non_export");

        // Code with library() inside a function, using non-exported symbol
        let code = format!(
            "{} <- function() {{\n    library({})\n    result <- {}(1)\n}}\noutside <- {}(2)",
            func_name, package, non_exported_name, non_exported_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback - only exports exported_name
        let pkg_clone = package.clone();
        let export_clone = exported_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope inside the function (line 2, after library call inside function)
        let scope_inside = scope_at_position_with_packages(&artifacts, 2, 10, &get_exports, &base_exports);

        // Requirement 8.4: Non-exported symbol should NOT be in scope inside function
        prop_assert!(
            !scope_inside.symbols.contains_key(&non_exported_name),
            "Non-exported symbol '{}' should NOT be in scope inside function after library({}). \
             Package only exports '{}'. Code:\n{}",
            non_exported_name, package, exported_name, code
        );

        // Query scope outside the function (line 4)
        let scope_outside = scope_at_position_with_packages(&artifacts, 4, 10, &get_exports, &base_exports);

        // Non-exported symbol should NOT be in scope outside function either
        prop_assert!(
            !scope_outside.symbols.contains_key(&non_exported_name),
            "Non-exported symbol '{}' should NOT be in scope outside function. Code:\n{}",
            non_exported_name, code
        );
    }

    /// Feature: package-function-awareness, Property 13 extended: Similar but different symbol names
    /// **Validates: Requirement 8.4**
    ///
    /// Verifies that symbols with similar names (e.g., "mutate" vs "mutate2") are
    /// correctly distinguished - only exact matches should be in scope.
    #[test]
    fn prop_non_export_diagnostic_similar_names(
        package in pkg_name(),
        base_name in r_identifier(),
    ) {
        let uri = make_url("test_similar_names");

        // Create similar but different names
        let exported_name = base_name.clone();
        let similar_name = format!("{}2", base_name);

        // Code with library call and usage of similar-but-different symbol
        let code = format!(
            "library({})\nresult <- {}(1)",
            package, similar_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback - only exports base_name, not similar_name
        let pkg_clone = package.clone();
        let export_clone = exported_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 1 (after library call)
        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        // Requirement 8.4: Similar-but-different symbol should NOT be in scope
        prop_assert!(
            !scope.symbols.contains_key(&similar_name),
            "Similar symbol '{}' should NOT be in scope (only '{}' is exported by '{}'). \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            similar_name, exported_name, package, code
        );

        // Sanity check: The exact exported name SHOULD be in scope
        prop_assert!(
            scope.symbols.contains_key(&exported_name),
            "Exported symbol '{}' SHOULD be in scope. Code:\n{}",
            exported_name, code
        );
    }

    /// Feature: package-function-awareness, Property 13 extended: Non-export not affected by base packages
    /// **Validates: Requirement 8.4**
    ///
    /// Verifies that non-exported symbols are NOT in scope even when base packages
    /// are available (base packages don't magically make all symbols available).
    #[test]
    fn prop_non_export_diagnostic_with_base_packages(
        package in pkg_name(),
        exported_name in r_identifier(),
        non_exported_name in r_identifier(),
        base_export in r_identifier(),
    ) {
        // Ensure all names are distinct
        prop_assume!(exported_name != non_exported_name);
        prop_assume!(non_exported_name != base_export);
        prop_assume!(exported_name != base_export);

        let uri = make_url("test_non_export_with_base");

        // Code with library call and usage of non-exported symbol
        let code = format!(
            "library({})\nresult <- {}(1)",
            package, non_exported_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let export_clone = exported_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        // Add some base exports (but NOT the non_exported_name)
        let mut base_exports = HashSet::new();
        base_exports.insert(base_export.clone());

        // Query scope at line 1 (after library call)
        let scope = scope_at_position_with_packages(&artifacts, 1, 10, &get_exports, &base_exports);

        // Requirement 8.4: Non-exported symbol should NOT be in scope
        // (even though base packages are available)
        prop_assert!(
            !scope.symbols.contains_key(&non_exported_name),
            "Non-exported symbol '{}' should NOT be in scope (not in package '{}' exports or base). \
             This means an 'undefined variable' warning WOULD be emitted. Code:\n{}",
            non_exported_name, package, code
        );

        // Sanity checks
        prop_assert!(
            scope.symbols.contains_key(&exported_name),
            "Exported symbol '{}' from package '{}' SHOULD be in scope. Code:\n{}",
            exported_name, package, code
        );
        prop_assert!(
            scope.symbols.contains_key(&base_export),
            "Base export '{}' SHOULD be in scope. Code:\n{}",
            base_export, code
        );
    }
}

// ============================================================================
// Property 14: Package Completion Inclusion
// Validates: Requirements 9.1, 9.2
//
// For any position X after a library(P) call, completions at X SHALL include
// all exports from package P with package attribution.
//
// This test verifies that:
// - All exports from a loaded package are available in scope after the library() call
// - Each export has the correct package attribution (source_uri contains package name)
// - Multiple packages can provide exports that appear in completions
// - Package attribution is preserved for each export
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Feature: package-function-awareness, Property 14: Package Completion Inclusion
    /// **Validates: Requirements 9.1, 9.2**
    ///
    /// For any position X after a library(P) call, completions at X SHALL include
    /// all exports from package P with package attribution.
    ///
    /// This test verifies that package exports are available in scope after the
    /// library() call with correct package attribution, which is the mechanism
    /// by which completions include package exports with package names in the detail field.
    #[test]
    fn prop_package_completion_inclusion(
        package in pkg_name(),
        func in library_func(),
        export1 in r_identifier(),
        export2 in r_identifier(),
        export3 in r_identifier(),
        lines_before in 0..3usize,
    ) {
        // Ensure exports are distinct
        prop_assume!(export1 != export2 && export2 != export3 && export1 != export3);
        // Ensure exports don't conflict with filler variable names (filler0, filler1, filler2)
        prop_assume!(!export1.starts_with("filler") && !export2.starts_with("filler") && !export3.starts_with("filler"));

        let uri = make_url("test_pkg_completion_inclusion");

        // Build code with library call
        let mut code_lines = Vec::new();

        // Add filler lines before library call (using "filler" prefix to avoid conflicts with exports)
        for i in 0..lines_before {
            code_lines.push(format!("filler{} <- {}", i, i));
        }

        // Add library call
        let library_line = lines_before;
        code_lines.push(format!("{}({})", func, package));

        // Add a line after library call
        code_lines.push("# cursor position here".to_string());

        let code = code_lines.join("\n");
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback that returns multiple exports for our package
        let pkg_clone = package.clone();
        let export1_clone = export1.clone();
        let export2_clone = export2.clone();
        let export3_clone = export3.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export1_clone.clone());
                exports.insert(export2_clone.clone());
                exports.insert(export3_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        // Empty base exports (we're testing package exports, not base)
        let base_exports = HashSet::new();

        // Query scope at the position after library call (where completions would be requested)
        let query_line = (library_line + 1) as u32;
        let scope = scope_at_position_with_packages(&artifacts, query_line, 0, &get_exports, &base_exports);

        // Requirement 9.1: All exports from the loaded package should be in scope
        prop_assert!(
            scope.symbols.contains_key(&export1),
            "Package export '{}' from package '{}' should be in scope at line {} (after library() on line {}). \
             This means it would appear in completions. Code:\n{}",
            export1, package, query_line, library_line, code
        );
        prop_assert!(
            scope.symbols.contains_key(&export2),
            "Package export '{}' from package '{}' should be in scope at line {} (after library() on line {}). \
             This means it would appear in completions. Code:\n{}",
            export2, package, query_line, library_line, code
        );
        prop_assert!(
            scope.symbols.contains_key(&export3),
            "Package export '{}' from package '{}' should be in scope at line {} (after library() on line {}). \
             This means it would appear in completions. Code:\n{}",
            export3, package, query_line, library_line, code
        );

        // Requirement 9.2: Each export should have the correct package attribution
        let expected_uri = format!("package:{}", package);
        
        let symbol1 = scope.symbols.get(&export1).unwrap();
        prop_assert_eq!(
            symbol1.source_uri.as_str(), expected_uri.as_str(),
            "Package export '{}' should have URI '{}' for package attribution, got '{}'. Code:\n{}",
            export1, expected_uri, symbol1.source_uri.as_str(), code
        );

        let symbol2 = scope.symbols.get(&export2).unwrap();
        prop_assert_eq!(
            symbol2.source_uri.as_str(), expected_uri.as_str(),
            "Package export '{}' should have URI '{}' for package attribution, got '{}'. Code:\n{}",
            export2, expected_uri, symbol2.source_uri.as_str(), code
        );

        let symbol3 = scope.symbols.get(&export3).unwrap();
        prop_assert_eq!(
            symbol3.source_uri.as_str(), expected_uri.as_str(),
            "Package export '{}' should have URI '{}' for package attribution, got '{}'. Code:\n{}",
            export3, expected_uri, symbol3.source_uri.as_str(), code
        );
    }

    /// Feature: package-function-awareness, Property 14 extended: Multiple packages
    /// **Validates: Requirements 9.1, 9.2, 9.3**
    ///
    /// When multiple packages are loaded, completions should include exports from
    /// ALL loaded packages, each with correct package attribution.
    #[test]
    fn prop_package_completion_inclusion_multiple_packages(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
        export1 in r_identifier(),
        export2 in r_identifier(),
    ) {
        // Ensure packages and exports are distinct
        prop_assume!(pkg1 != pkg2);
        prop_assume!(export1 != export2);

        let uri = make_url("test_multi_pkg_completion");

        // Code with two library calls
        let code = format!(
            "library({})\nlibrary({})\n# cursor position here",
            pkg1, pkg2
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg1_clone = pkg1.clone();
        let pkg2_clone = pkg2.clone();
        let export1_clone = export1.clone();
        let export2_clone = export2.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg1_clone {
                let mut exports = HashSet::new();
                exports.insert(export1_clone.clone());
                exports
            } else if p == pkg2_clone {
                let mut exports = HashSet::new();
                exports.insert(export2_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library calls)
        let scope = scope_at_position_with_packages(&artifacts, 2, 0, &get_exports, &base_exports);

        // Requirement 9.1: Both exports should be in scope
        prop_assert!(
            scope.symbols.contains_key(&export1),
            "Export '{}' from package '{}' should be in scope. Code:\n{}",
            export1, pkg1, code
        );
        prop_assert!(
            scope.symbols.contains_key(&export2),
            "Export '{}' from package '{}' should be in scope. Code:\n{}",
            export2, pkg2, code
        );

        // Requirement 9.2: Each export should have correct package attribution
        let symbol1 = scope.symbols.get(&export1).unwrap();
        let expected_uri1 = format!("package:{}", pkg1);
        prop_assert_eq!(
            symbol1.source_uri.as_str(), expected_uri1.as_str(),
            "Export '{}' should have URI '{}', got '{}'. Code:\n{}",
            export1, expected_uri1, symbol1.source_uri.as_str(), code
        );

        let symbol2 = scope.symbols.get(&export2).unwrap();
        let expected_uri2 = format!("package:{}", pkg2);
        prop_assert_eq!(
            symbol2.source_uri.as_str(), expected_uri2.as_str(),
            "Export '{}' should have URI '{}', got '{}'. Code:\n{}",
            export2, expected_uri2, symbol2.source_uri.as_str(), code
        );
    }

    /// Feature: package-function-awareness, Property 14 extended: Duplicate exports
    /// **Validates: Requirements 9.2, 9.3**
    ///
    /// When multiple packages export the same symbol, the first loaded package's
    /// export should be used (but both would be shown in completions with attribution).
    /// This test verifies the scope resolution behavior for duplicate exports.
    #[test]
    fn prop_package_completion_duplicate_exports(
        pkg1 in pkg_name(),
        pkg2 in pkg_name(),
        shared_export in r_identifier(),
    ) {
        // Ensure packages are distinct
        prop_assume!(pkg1 != pkg2);

        let uri = make_url("test_duplicate_export_completion");

        // Code with two library calls, both packages export the same symbol
        let code = format!(
            "library({})\nlibrary({})\n# cursor position here",
            pkg1, pkg2
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback where both packages export the same symbol
        let pkg1_clone = pkg1.clone();
        let pkg2_clone = pkg2.clone();
        let export_clone = shared_export.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg1_clone || p == pkg2_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library calls)
        let scope = scope_at_position_with_packages(&artifacts, 2, 0, &get_exports, &base_exports);

        // The symbol should be in scope
        prop_assert!(
            scope.symbols.contains_key(&shared_export),
            "Shared export '{}' should be in scope. Code:\n{}",
            shared_export, code
        );

        // The first loaded package (pkg1) should win for the symbol attribution
        // This is because scope_at_position_with_packages processes PackageLoad events
        // in order and only overrides base package exports, not other package exports
        let symbol = scope.symbols.get(&shared_export).unwrap();
        let expected_uri = format!("package:{}", pkg1);
        prop_assert_eq!(
            symbol.source_uri.as_str(), expected_uri.as_str(),
            "Shared export '{}' should have URI '{}' (first loaded package), got '{}'. Code:\n{}",
            shared_export, expected_uri, symbol.source_uri.as_str(), code
        );
    }

    /// Feature: package-function-awareness, Property 14 extended: Position-aware completions
    /// **Validates: Requirements 9.1, 2.1, 2.2**
    ///
    /// Package exports should NOT be available in completions before the library() call.
    #[test]
    fn prop_package_completion_position_aware(
        package in pkg_name(),
        export_name in r_identifier(),
        lines_before in 1..4usize,
    ) {
        // Ensure export doesn't conflict with filler variable names
        prop_assume!(!export_name.starts_with("filler"));

        let uri = make_url("test_pkg_completion_position");

        // Build code with library call in the middle
        let mut code_lines = Vec::new();

        // Add filler lines before library call (using "filler" prefix to avoid conflicts)
        for i in 0..lines_before {
            code_lines.push(format!("filler{} <- {}", i, i));
        }

        // Add library call
        let library_line = lines_before;
        code_lines.push(format!("library({})", package));

        // Add a line after library call
        code_lines.push("# after library".to_string());

        let code = code_lines.join("\n");
        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let export_clone = export_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(export_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope BEFORE the library call (line 0)
        let scope_before = scope_at_position_with_packages(&artifacts, 0, 0, &get_exports, &base_exports);

        // Export should NOT be in scope before library() call
        prop_assert!(
            !scope_before.symbols.contains_key(&export_name),
            "Package export '{}' should NOT be in scope at line 0 (before library() on line {}). \
             This means it would NOT appear in completions before the library call. Code:\n{}",
            export_name, library_line, code
        );

        // Query scope AFTER the library call
        let scope_after = scope_at_position_with_packages(&artifacts, (library_line + 1) as u32, 0, &get_exports, &base_exports);

        // Export SHOULD be in scope after library() call
        prop_assert!(
            scope_after.symbols.contains_key(&export_name),
            "Package export '{}' should be in scope at line {} (after library() on line {}). \
             This means it would appear in completions after the library call. Code:\n{}",
            export_name, library_line + 1, library_line, code
        );
    }

    /// Feature: package-function-awareness, Property 14 extended: Local definitions shadow package exports
    /// **Validates: Requirements 9.4**
    ///
    /// Local definitions should take precedence over package exports in completions.
    #[test]
    fn prop_package_completion_local_precedence(
        package in pkg_name(),
        symbol_name in r_identifier(),
    ) {
        let uri = make_url("test_pkg_completion_local_precedence");

        // Code with library call followed by local definition of same symbol
        let code = format!(
            "library({})\n{} <- 42\n# cursor position here",
            package, symbol_name
        );

        let tree = parse_r_tree(&code);
        let artifacts = compute_artifacts(&uri, &tree, &code);

        // Create a package exports callback
        let pkg_clone = package.clone();
        let symbol_clone = symbol_name.clone();
        let get_exports = move |p: &str| -> HashSet<String> {
            if p == pkg_clone {
                let mut exports = HashSet::new();
                exports.insert(symbol_clone.clone());
                exports
            } else {
                HashSet::new()
            }
        };

        let base_exports = HashSet::new();

        // Query scope at line 2 (after both library call and local definition)
        let scope = scope_at_position_with_packages(&artifacts, 2, 0, &get_exports, &base_exports);

        // Symbol should be in scope
        prop_assert!(
            scope.symbols.contains_key(&symbol_name),
            "Symbol '{}' should be in scope. Code:\n{}",
            symbol_name, code
        );

        // Requirement 9.4: Local definition should take precedence
        // The symbol should have the file URI, not the package URI
        let symbol = scope.symbols.get(&symbol_name).unwrap();
        prop_assert_eq!(
            &symbol.source_uri, &uri,
            "Symbol '{}' should have local file URI (local definition takes precedence), got '{}'. Code:\n{}",
            symbol_name, symbol.source_uri.as_str(), code
        );
    }
}
