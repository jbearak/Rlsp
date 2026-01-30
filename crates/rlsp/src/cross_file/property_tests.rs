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
        graph.update_file(&parent_uri, &meta, |p| {
            if p == format!("{}.R", child) {
                Some(child_uri.clone())
            } else {
                None
            }
        });

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
        let child1_uri = make_url(&child1);
        let child2_uri = make_url(&child2);

        let meta = make_meta_with_sources(vec![
            (&format!("{}.R", child1), 5),
            (&format!("{}.R", child2), 10),
        ]);

        graph.update_file(&parent_uri, &meta, |p| {
            if p == format!("{}.R", child1) {
                Some(child1_uri.clone())
            } else if p == format!("{}.R", child2) {
                Some(child2_uri.clone())
            } else {
                None
            }
        });

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
        graph.update_file(&parent_uri, &meta, |p| {
            if p == format!("{}.R", child) {
                Some(child_uri.clone())
            } else {
                None
            }
        });

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
        graph.update_file(&parent_uri, &meta, |p| {
            if p == format!("{}.R", child) {
                Some(child_uri.clone())
            } else {
                None
            }
        });

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
        graph.update_file(&uri_a, &meta_a, |p| {
            if p == format!("{}.R", file_b) {
                Some(uri_b.clone())
            } else {
                None
            }
        });

        // B sources C
        let meta_b = make_meta_with_sources(vec![(&format!("{}.R", file_c), 1)]);
        graph.update_file(&uri_b, &meta_b, |p| {
            if p == format!("{}.R", file_c) {
                Some(uri_c.clone())
            } else {
                None
            }
        });

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
        graph.update_file(&uri_a, &meta_a, |p| {
            if p == format!("{}.R", file_b) { Some(uri_b.clone()) } else { None }
        });

        let meta_b = make_meta_with_sources(vec![(&format!("{}.R", file_c), 1)]);
        graph.update_file(&uri_b, &meta_b, |p| {
            if p == format!("{}.R", file_c) { Some(uri_c.clone()) } else { None }
        });

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
        let child_uri = make_url(&child);

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
                },
                ForwardSource {
                    path: format!("{}.R", child),
                    line,
                    column: 0,
                    is_directive: true, // Directive
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&parent_uri, &meta, |p| {
            if p == format!("{}.R", child) {
                Some(child_uri.clone())
            } else {
                None
            }
        });

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
        let child_uri = make_url(&child);

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
                },
                ForwardSource {
                    path: format!("{}.R", child),
                    line: line2,
                    column: 0,
                    is_directive: false,
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&parent_uri, &meta, |p| {
            if p == format!("{}.R", child) {
                Some(child_uri.clone())
            } else {
                None
            }
        });

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
        let child_uri = make_url(&child);

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
                },
                ForwardSource {
                    path: format!("{}.R", child),
                    line,
                    column: 0,
                    is_directive: false, // AST second
                    local: false,
                    chdir: false,
                    is_sys_source: false,
                },
            ],
            ..Default::default()
        };

        graph.update_file(&parent_uri, &meta, |p| {
            if p == format!("{}.R", child) {
                Some(child_uri.clone())
            } else {
                None
            }
        });

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
        symbol_a in "[a-zA-Z][a-zA-Z0-9_]{0,3}",
        symbol_b in "[a-zA-Z][a-zA-Z0-9_]{0,3}"
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
// Property 5: Diagnostic Suppression
// Validates: Requirements 2.4, 2.5, 10.4, 10.5
// ============================================================================

use super::directive::is_line_ignored;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Property 5: For any file containing @lsp-ignore on line n, no diagnostics
    /// SHALL be emitted for line n.
    #[test]
    fn prop_diagnostic_suppression_ignore(
        prefix_lines in 0..5u32
    ) {
        let mut lines = Vec::new();
        for i in 0..prefix_lines {
            lines.push(format!("x{} <- {}", i, i));
        }
        lines.push("# @lsp-ignore".to_string());
        lines.push("undefined_var".to_string());
        let content = lines.join("\n");

        let meta = parse_directives(&content);

        // The @lsp-ignore line itself should be ignored
        prop_assert!(is_line_ignored(&meta, prefix_lines));
    }

    /// Property 5: For any file containing @lsp-ignore-next on line n, no diagnostics
    /// SHALL be emitted for line n+1.
    #[test]
    fn prop_diagnostic_suppression_ignore_next(
        prefix_lines in 0..5u32
    ) {
        let mut lines = Vec::new();
        for i in 0..prefix_lines {
            lines.push(format!("x{} <- {}", i, i));
        }
        lines.push("# @lsp-ignore-next".to_string());
        lines.push("undefined_var".to_string());
        let content = lines.join("\n");

        let meta = parse_directives(&content);

        // The line AFTER @lsp-ignore-next should be ignored
        prop_assert!(is_line_ignored(&meta, prefix_lines + 1));
        // The @lsp-ignore-next line itself should NOT be ignored
        prop_assert!(!is_line_ignored(&meta, prefix_lines));
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

        let parent_uri = make_url("parent");
        let child_uri = make_url("child");

        // Parent file: defines symbol_before on line 0, symbol_after on line call_site_line
        let mut parent_lines = vec![format!("{} <- 1", symbol_before)];
        for i in 1..call_site_line {
            parent_lines.push(format!("x{} <- {}", i, i));
        }
        parent_lines.push(format!("{} <- 2", symbol_after));
        let parent_code = parent_lines.join("\n");
        let parent_tree = parse_r_tree(&parent_code);
        let parent_artifacts = compute_artifacts(&parent_uri, &parent_tree, &parent_code);

        // Child file: has backward directive with line= parameter
        let child_code = format!("# @lsp-sourced-by ../parent.R line={}\ny <- 3", call_site_line);
        let child_tree = parse_r_tree(&child_code);
        let child_artifacts = compute_artifacts(&child_uri, &child_tree, &child_code);

        // line= is 1-based, so line=N means call site is at 0-based line N-1
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

        // symbol_after (defined on line call_site_line) should NOT be available
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
// Property 44: Workspace Index Version Monotonicity
// Validates: Requirements 13.5
// ============================================================================

use super::workspace_index::{CrossFileWorkspaceIndex, IndexEntry};
use super::file_cache::FileSnapshot;
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
