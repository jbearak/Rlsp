// Tests for workspace scanning functionality

#[cfg(test)]
mod workspace_scan_tests {
    use super::super::*;
    use std::path::Path;
    use std::fs;
    use tempfile::TempDir;
    use tower_lsp::lsp_types::Url;

    // Use default max_chain_depth for tests
    const TEST_MAX_CHAIN_DEPTH: usize = 20;

    /// Issue #476 (bug A): the dependency graph must be built in a deterministic
    /// order. `build_dependency_graph_from_workspace` feeds files to
    /// `update_file`, which appends each file's incoming edge to
    /// `backward[child]`; if the feed order followed the non-deterministic
    /// rayon-scan / HashMap / LRU iteration order, the backward-edge `Vec` order
    /// — which scope resolution's parent-prefix walk follows — varied run to run,
    /// so `raven check` dropped a different subset of symbols each run. The fix
    /// sorts by URI before feeding `update_file`. This asserts the resulting
    /// backward-edge order is the canonical (URI-sorted) order, independent of
    /// scan order.
    #[test]
    fn test_dependency_graph_build_order_is_deterministic_476() {
        let temp_dir = TempDir::new().unwrap();
        // Several parents all source the same child. Names chosen so a plausible
        // non-sorted processing order would NOT coincide with sorted order.
        let child = "shared_child.r";
        fs::write(temp_dir.path().join(child), "helper <- function() {}").unwrap();
        for p in ["zeta.r", "alpha.r", "mike.r", "bravo.r"] {
            fs::write(
                temp_dir.path().join(p),
                format!("source(\"{child}\")\nhelper()\n"),
            )
            .unwrap();
        }

        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let child_uri = Url::from_file_path(temp_dir.path().join(child)).unwrap();

        // Build twice; the resulting backward-edge order must be byte-identical
        // AND in URI-sorted order (the canonical, scan-order-independent result).
        let order_of = |state: &WorldState| -> Vec<String> {
            state
                .cross_file_graph
                .get_dependents(&child_uri)
                .iter()
                .map(|e| e.from.as_str().to_string())
                .collect()
        };

        let build = || {
            let entries = scan_workspace(std::slice::from_ref(&workspace_url), TEST_MAX_CHAIN_DEPTH);
            let mut state = WorldState::new();
            state.workspace_folders = vec![workspace_url.clone()];
            state.apply_workspace_index(entries);
            state
        };

        let first = order_of(&build());
        let second = order_of(&build());
        assert_eq!(first, second, "graph build order must be deterministic");
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(
            first, sorted,
            "backward edges must be in canonical URI-sorted order, got {first:?}"
        );
        assert_eq!(first.len(), 4, "all four parents should be backward edges");
    }

    #[test]
    fn test_scan_workspace_finds_uppercase_r_files() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.R");
        fs::write(&test_file, "x <- 1").unwrap();

        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        assert_eq!(entries.len(), 1, "Should find 1 .R file");
        assert_eq!(entries.len(), 1, "Should have 1 cross-file entry");
        assert_eq!(entries.len(), 1, "Should have 1 new index entry");
    }

    #[test]
    fn test_scan_workspace_finds_lowercase_r_files() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.r");
        fs::write(&test_file, "x <- 1").unwrap();

        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        assert_eq!(entries.len(), 1, "Should find 1 .r file");
        assert_eq!(entries.len(), 1, "Should have 1 cross-file entry");
        assert_eq!(entries.len(), 1, "Should have 1 new index entry");
    }

    #[test]
    fn test_scan_workspace_finds_mixed_case_r_files() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create files with different case extensions
        fs::write(temp_dir.path().join("uppercase.R"), "x <- 1").unwrap();
        fs::write(temp_dir.path().join("lowercase.r"), "y <- 2").unwrap();
        
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        assert_eq!(entries.len(), 2, "Should find both .R and .r files");
        assert_eq!(entries.len(), 2, "Should have 2 cross-file entries");
        assert_eq!(entries.len(), 2, "Should have 2 new index entries");
        
        // Verify both files are indexed
        let uris: Vec<String> = entries.keys().map(|u| u.to_string()).collect();
        assert!(uris.iter().any(|u| u.contains("uppercase.R")), "Should find uppercase.R");
        assert!(uris.iter().any(|u| u.contains("lowercase.r")), "Should find lowercase.r");
    }

    #[test]
    fn test_scan_workspace_computes_cross_file_metadata() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create a file with a function definition
        let test_file = temp_dir.path().join("functions.r");
        fs::write(&test_file, "my_func <- function() { 42 }").unwrap();
        
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries.len(), 1);
        
        // Verify the entry has artifacts with exported symbols
        let entry = entries.values().next().unwrap();
        assert!(entry.artifacts.exported_interface.contains_key("my_func"), 
            "Should export my_func symbol");
        
        // Verify new index entry also has the same artifacts
        let new_entry = entries.values().next().unwrap();
        assert!(new_entry.artifacts.exported_interface.contains_key("my_func"), 
            "New index entry should export my_func symbol");
    }

    #[test]
    fn test_scan_workspace_recursive() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create nested directory structure
        let subdir = temp_dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        
        fs::write(temp_dir.path().join("root.r"), "x <- 1").unwrap();
        fs::write(subdir.join("nested.r"), "y <- 2").unwrap();
        
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        assert_eq!(entries.len(), 2, "Should find files in root and subdirectory");
        assert_eq!(entries.len(), 2, "Should have 2 new index entries");
    }

    #[test]
    fn test_scan_workspace_new_index_entry_has_all_fields() {
        let temp_dir = TempDir::new().unwrap();
        
        // Create a file with library call and function definition
        let test_file = temp_dir.path().join("complete.r");
        fs::write(&test_file, r#"
library(dplyr)
data("flights", package = "nycflights13")
my_func <- function(x) { x + 1 }
"#).unwrap();
        
        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        assert_eq!(entries.len(), 1);
        
        let entry = entries.values().next().unwrap();
        
        // Verify all fields are populated
        assert!(!entry.contents.to_string().is_empty(), "Should have contents");
        assert!(entry.tree.is_some(), "Should have parsed tree");
        assert!(entry.loaded_packages.contains(&"dplyr".to_string()), "Should have loaded packages");
        assert_eq!(
            entry.data_packages,
            vec!["nycflights13"],
            "Should preserve data() package hints for CLI warming"
        );
        assert!(entry.snapshot.size > 0, "Should have valid snapshot");
        assert!(entry.artifacts.exported_interface.contains_key("my_func"), 
            "Should have exported symbols in artifacts");
    }

    #[test]
    fn test_is_stat_model_extension_matches_supported_extensions_case_insensitively() {
        assert!(is_stat_model_extension(Path::new("script.R")));
        assert!(is_stat_model_extension(Path::new("script.r")));
        assert!(is_stat_model_extension(Path::new("model.JAGS")));
        assert!(is_stat_model_extension(Path::new("model.Bugs")));
        assert!(is_stat_model_extension(Path::new("model.STAN")));
        assert!(!is_stat_model_extension(Path::new("notes.txt")));
        assert!(!is_stat_model_extension(Path::new("README")));
    }

    #[test]
    fn test_scan_workspace_excludes_github_directory() {
        let temp_dir = TempDir::new().unwrap();
        let github_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&github_dir).unwrap();
        fs::write(github_dir.join("action.R"), "source('versions-matrix.R')").unwrap();
        // A normal R file at root should still be found
        fs::write(temp_dir.path().join("main.R"), "x <- 1").unwrap();

        let workspace_url = Url::from_file_path(temp_dir.path()).unwrap();
        let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        assert_eq!(entries.len(), 1, "Only root main.R should be indexed, not .github/");
        let uris: Vec<String> = entries.keys().map(|u| u.to_string()).collect();
        assert!(uris.iter().any(|u| u.contains("main.R")));
        assert!(!uris.iter().any(|u| u.contains(".github")),
            ".github/ R files must not be scanned");
        // The exclusion must hold for every map the same traversal produces.
        assert!(
            !entries.keys().any(|u| u.as_str().contains(".github")),
            ".github/ R files must not produce cross-file entries"
        );
        assert!(
            !entries.keys().any(|u| u.as_str().contains(".github")),
            ".github/ R files must not produce new index entries"
        );
    }
}


// ============================================================================
// JAGS/Stan Workspace Indexing Tests
// **Validates: Requirements 11.1** (Property 11)
// ============================================================================

#[cfg(test)]
mod jags_stan_indexing_tests {
    use super::super::*;
    use std::fs;
    use tempfile::TempDir;
    use tower_lsp::lsp_types::Url;

    const TEST_MAX_CHAIN_DEPTH: usize = 20;

    #[test]
    fn test_scan_workspace_includes_jags_stan_files() {
        // **Validates: Requirements 11.1**
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        // Create test files with various extensions
        fs::write(dir.join("model.jags"), "model { x ~ dnorm(0, 1) }").unwrap();
        fs::write(dir.join("model.bugs"), "model { y ~ dgamma(1, 1) }").unwrap();
        fs::write(dir.join("model.stan"), "data { int N; }").unwrap();
        fs::write(dir.join("script.R"), "x <- 1").unwrap();
        fs::write(dir.join("readme.txt"), "not indexed").unwrap();

        let workspace_url = Url::from_file_path(dir).unwrap();
        let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        // All R, JAGS, and Stan files should be indexed (4 total), .txt excluded
        assert_eq!(entries.len(), 4, "Should index .jags, .bugs, .stan, and .R files");
        assert_eq!(entries.len(), 4, "Should have 4 cross-file entries");
        assert_eq!(entries.len(), 4, "Should have 4 new index entries");

        // Verify specific files are indexed by checking URIs
        let uris: Vec<String> = entries.keys().map(|u| u.to_string()).collect();
        assert!(uris.iter().any(|u| u.contains("model.jags")), "Should index .jags files");
        assert!(uris.iter().any(|u| u.contains("model.bugs")), "Should index .bugs files");
        assert!(uris.iter().any(|u| u.contains("model.stan")), "Should index .stan files");
        assert!(uris.iter().any(|u| u.contains("script.R")), "Should index .R files");
        assert!(!uris.iter().any(|u| u.contains("readme.txt")), "Should NOT index .txt files");
    }

    #[test]
    fn test_scan_workspace_jags_stan_case_insensitive() {
        // **Validates: Requirements 11.1**
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();

        fs::write(dir.join("upper.JAGS"), "model { x ~ dnorm(0, 1) }").unwrap();
        fs::write(dir.join("upper.BUGS"), "model { y ~ dgamma(1, 1) }").unwrap();
        fs::write(dir.join("upper.STAN"), "data { int N; }").unwrap();

        let workspace_url = Url::from_file_path(dir).unwrap();
        let index = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

        assert_eq!(index.len(), 3, "Should index uppercase JAGS/BUGS/STAN extensions");
    }
}

#[cfg(test)]
mod jags_stan_indexing_property_tests {
    use super::super::*;
    use proptest::prelude::*;
    use std::fs;
    use tempfile::TempDir;
    use tower_lsp::lsp_types::Url;

    const TEST_MAX_CHAIN_DEPTH: usize = 20;

    /// Generate a valid filename stem (alphanumeric, non-empty)
    fn filename_stem_strategy() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_]{0,10}".prop_map(|s| s)
    }

    /// Generate a JAGS/Stan extension
    fn jags_stan_extension_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("jags".to_string()),
            Just("JAGS".to_string()),
            Just("bugs".to_string()),
            Just("BUGS".to_string()),
            Just("stan".to_string()),
            Just("STAN".to_string()),
            Just("Jags".to_string()),
            Just("Bugs".to_string()),
            Just("Stan".to_string()),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// **Property 11: Workspace indexing includes JAGS/Stan files**
        ///
        /// For any file path with a `.jags`, `.bugs`, or `.stan` extension present
        /// in a workspace directory, after `scan_directory` completes, the workspace
        /// index shall contain an entry for that file's URI.
        ///
        /// **Validates: Requirements 11.1**
        #[test]
        fn prop_workspace_indexing_includes_jags_stan_files(
            stem in filename_stem_strategy(),
            ext in jags_stan_extension_strategy(),
        ) {
            let temp_dir = TempDir::new().unwrap();
            let dir = temp_dir.path();

            let filename = format!("{}.{}", stem, ext);
            let file_path = dir.join(&filename);
            fs::write(&file_path, "model { x ~ dnorm(0, 1) }").unwrap();

            let workspace_url = Url::from_file_path(dir).unwrap();
            let entries = scan_workspace(&[workspace_url], TEST_MAX_CHAIN_DEPTH);

            // The file must be present in the index
            prop_assert_eq!(
                entries.len(), 1,
                "File '{}' should be indexed", filename
            );
            prop_assert_eq!(
                entries.len(), 1,
                "File '{}' should have a new index entry", filename
            );

            // Verify the indexed URI matches the exact file path
            let uri = entries.keys().next().unwrap();
            let expected_uri = Url::from_file_path(&file_path).unwrap();
            prop_assert_eq!(
                uri, &expected_uri,
                "Indexed URI should match the exact file path"
            );
        }
    }
}


// ============================================================================
// Bug Condition Exploration Test — JAGS/Stan files have None tree
// **Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8**
// ============================================================================

#[cfg(test)]
mod bug_condition_exploration {
    use super::super::*;
    use proptest::prelude::*;

    /// Map a JAGS/Stan extension string to its FileType.
    fn file_type_from_ext(ext: &str) -> FileType {
        match ext.to_ascii_lowercase().as_str() {
            "jags" | "bugs" => FileType::Jags,
            "stan" => FileType::Stan,
            _ => unreachable!("strategy only generates jags/bugs/stan extensions"),
        }
    }

    /// Generate a JAGS/Stan extension (reuses the same set as jags_stan_extension_strategy)
    fn jags_stan_extension_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("jags".to_string()),
            Just("JAGS".to_string()),
            Just("bugs".to_string()),
            Just("BUGS".to_string()),
            Just("stan".to_string()),
            Just("STAN".to_string()),
            Just("Jags".to_string()),
            Just("Bugs".to_string()),
            Just("Stan".to_string()),
        ]
    }

    /// Generate arbitrary text content that could appear in a JAGS/Stan file
    fn content_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("x <- 1".to_string()),
            Just("model { x ~ dnorm(0, 1) }".to_string()),
            Just("data { int N; }".to_string()),
            Just("alpha <- dnorm(mu, tau)".to_string()),
            Just("".to_string()),
            "[a-zA-Z0-9 _<>~(){};.,\n]{1,100}",
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50))]

        /// **Property 1: Bug Condition — JAGS/Stan files have None tree**
        ///
        /// For any JAGS or Stan file type with arbitrary text content,
        /// `parse_document` should return `Some(Tree)` so that tree-dependent
        /// LSP features (find-references, go-to-definition, hover, document
        /// symbols) can operate on a best-effort basis.
        ///
        /// On UNFIXED code this test is EXPECTED TO FAIL because
        /// `parse_document` returns `None` for JAGS/Stan file types.
        ///
        /// **Validates: Requirements 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8**
        #[test]
        fn bug_condition_jags_stan_tree_is_some(
            content in content_strategy(),
            ext in jags_stan_extension_strategy(),
        ) {
            let file_type = file_type_from_ext(&ext);
            let doc = Document::new_with_file_type(&content, None, file_type);
            prop_assert!(
                doc.tree.is_some(),
                "parse_document returned None for FileType::{:?} with content {:?} \
                 (extension '{}') — tree-dependent LSP features will not work",
                file_type, content, ext
            );
        }
    }
}


// ============================================================================
// Preservation Property Tests — R parsing, diagnostics suppression, completion filtering
// **Validates: Requirements 3.1, 3.2, 3.3, 3.4**
// ============================================================================

#[cfg(test)]
mod preservation {
    use super::super::*;
    use proptest::prelude::*;

    /// Generate simple R code patterns that tree-sitter-r can parse.
    fn r_code_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            // Assignments
            "[a-z]{1,6}".prop_map(|name| format!("{} <- 1", name)),
            // Function calls
            "[a-z]{1,6}".prop_map(|name| format!("{}()", name)),
            // Function definitions
            "[a-z]{1,6}".prop_map(|name| format!("{} <- function(x) x + 1", name)),
            // Library calls
            "[a-z]{1,6}".prop_map(|pkg| format!("library({})", pkg)),
            // Simple expressions
            Just("1 + 2".to_string()),
            Just("x <- c(1, 2, 3)".to_string()),
            Just("if (TRUE) 1 else 2".to_string()),
            Just("for (i in 1:10) print(i)".to_string()),
            // Empty content (still valid)
            Just("".to_string()),
        ]
    }

    /// Generate a JAGS/Stan file type.
    fn jags_stan_file_type_strategy() -> impl Strategy<Value = FileType> {
        prop_oneof![Just(FileType::Jags), Just(FileType::Stan),]
    }

    /// Generate arbitrary content for JAGS/Stan files.
    fn jags_stan_content_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("model { x ~ dnorm(0, 1) }".to_string()),
            Just("data { int N; }".to_string()),
            Just("x <- 1".to_string()),
            Just("".to_string()),
            "[a-zA-Z0-9 _<>~(){};.,]{1,80}",
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50))]

        /// **Property 2a: R parsing preservation**
        ///
        /// For any R code string with `FileType::R`, `parse_document` returns
        /// `Some(Tree)` with a valid root node. This behavior must be preserved
        /// before and after the JAGS/Stan parse fix.
        ///
        /// **Validates: Requirements 3.1**
        #[test]
        fn prop_r_parsing_returns_tree(code in r_code_strategy()) {
            let doc = Document::new_with_file_type(&code, None, FileType::R);
            prop_assert!(
                doc.tree.is_some(),
                "parse_document returned None for FileType::R with code {:?}",
                code
            );
            let tree = doc.tree.as_ref().unwrap();
            let root = tree.root_node();
            // Root node should always be "program" for R parser
            prop_assert_eq!(
                root.kind(),
                "program",
                "Root node should be 'program' for R code {:?}",
                code
            );
        }

        /// **Property 2b: Diagnostics suppression — JAGS/Stan file types are non-R**
        ///
        /// Diagnostics suppression depends on `file_type != FileType::R` in
        /// `diagnostics_from_snapshot`. This test verifies that JAGS/Stan
        /// `Document` instances have the correct non-R file type, ensuring
        /// diagnostics suppression will work regardless of parse_document changes.
        ///
        /// **Validates: Requirements 3.2**
        #[test]
        fn prop_jags_stan_file_type_is_not_r(
            content in jags_stan_content_strategy(),
            file_type in jags_stan_file_type_strategy(),
        ) {
            let doc = Document::new_with_file_type(&content, None, file_type);
            prop_assert_ne!(
                doc.file_type,
                FileType::R,
                "JAGS/Stan document should not have FileType::R — \
                 diagnostics suppression depends on file_type != R"
            );
            // Verify the file type is preserved exactly
            prop_assert_eq!(
                doc.file_type,
                file_type,
                "Document file_type should match the file_type passed to new_with_file_type"
            );
        }

        /// **Property 2c: Completion filtering — file type preservation**
        ///
        /// Completion filtering depends on `doc.file_type` to route JAGS files
        /// to JAGS-specific completions and Stan files to Stan-specific completions.
        /// This test verifies that `Document::new_with_file_type` correctly
        /// preserves the file type for all variants.
        ///
        /// **Validates: Requirements 3.3, 3.4**
        #[test]
        fn prop_file_type_preserved_in_document(
            content in jags_stan_content_strategy(),
            file_type in jags_stan_file_type_strategy(),
        ) {
            let doc = Document::new_with_file_type(&content, None, file_type);
            prop_assert_eq!(
                doc.file_type,
                file_type,
                "Document should preserve file_type {:?} for completion filtering",
                file_type
            );
            // Verify JAGS stays JAGS and Stan stays Stan
            match file_type {
                FileType::Jags => prop_assert_eq!(doc.file_type, FileType::Jags),
                FileType::Stan => prop_assert_eq!(doc.file_type, FileType::Stan),
                FileType::R => unreachable!("strategy only generates Jags/Stan"),
            }
        }
    }
}

// ============================================================================
// Package mode: tests/testthat one-way visibility integration tests (Phase 6.2)
//
// These tests drive the full pipeline:
//   PackageInputs → WorldState::apply_package_event → scope_at_position_with_graph
// to confirm end-to-end that test files see R/ symbols (one-way visibility) and
// R/ files do NOT see test-only symbols.
// ============================================================================

#[cfg(test)]
mod package_testthat_visibility_tests {
    use super::super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower_lsp::lsp_types::Url;

    use crate::cross_file::dependency::DependencyGraph;
    use crate::cross_file::scope::{
        compute_artifacts, scope_at_position_with_graph, ScopeArtifacts,
    };
    use crate::cross_file::config::PackageMode;
    use crate::package_state::{
        ContentDigest, DescriptionInput, PackageInputDelta, RFileInput, RFileKind,
    };

    // Match the depth used by the other test modules so future adjustments
    // propagate uniformly.
    const TEST_MAX_CHAIN_DEPTH: usize = 20;

    /// Parse R source into `ScopeArtifacts` using tree-sitter-r.
    fn make_artifacts(uri: &Url, code: &str) -> Arc<ScopeArtifacts> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_r::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(code, None).unwrap();
        Arc::new(compute_artifacts(uri, &tree, code))
    }

    /// Resolve the full scope at EOF in `uri` using the given contribution.
    fn resolve_scope(
        uri: &Url,
        artifacts: Arc<ScopeArtifacts>,
        workspace_root: &Url,
        contribution: &crate::package_state::PackageScopeContribution,
    ) -> crate::cross_file::scope::ScopeAtPosition {
        let get_artifacts = |u: &Url| -> Option<Arc<ScopeArtifacts>> {
            if u == uri { Some(artifacts.clone()) } else { None }
        };
        let get_metadata =
            |_u: &Url| -> Option<Arc<crate::cross_file::types::CrossFileMetadata>> { None };
        let graph = DependencyGraph::new();

        scope_at_position_with_graph(
            uri,
            u32::MAX,
            u32::MAX,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(workspace_root),
            TEST_MAX_CHAIN_DEPTH,
            &HashSet::new(),
            false,
            crate::cross_file::config::BackwardDependencyMode::Explicit,
            &|| false,
            Some(contribution),
            None,
        )
    }

    /// Resolve visible symbols at EOF in `uri` using the given contribution.
    fn resolve_symbols(
        uri: &Url,
        artifacts: Arc<ScopeArtifacts>,
        workspace_root: &Url,
        contribution: &crate::package_state::PackageScopeContribution,
    ) -> std::collections::HashMap<Arc<str>, crate::cross_file::scope::ScopedSymbol> {
        resolve_scope(uri, artifacts, workspace_root, contribution).symbols
    }

    /// Build a `WorldState` pre-populated with DESCRIPTION + the given R files,
    /// then call `apply_package_event(Initial)` to drive full package-mode derivation.
    fn build_state_with_files(
        workspace_root_path: &str,
        r_files: Vec<(PathBuf, RFileKind, &str)>,
    ) -> WorldState {
        let mut state = WorldState::new();
        state.package_inputs.workspace_root = Some(PathBuf::from(workspace_root_path));
        state.package_inputs.package_mode = PackageMode::Auto;
        state.package_inputs.description = Some(DescriptionInput {
            text: "Package: foo\n".into(),
        });
        for (path, kind, text) in r_files {
            let text: Arc<str> = Arc::from(text);
            let digest = ContentDigest::of(&text);
            state.package_inputs.r_files.insert(
                path,
                RFileInput { kind, text, content_digest: digest },
            );
        }
        state.apply_package_event(&PackageInputDelta::Initial);
        state
    }

    // ------------------------------------------------------------------
    // Test A: test file sees R/ symbol (one-way: tests → R/)
    // ------------------------------------------------------------------

    /// End-to-end: a symbol defined in R/utils.R is visible when resolving
    /// scope inside tests/testthat/test-utils.R.
    #[test]
    fn test_file_sees_r_dir_symbol_end_to_end() {
        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![
                (
                    PathBuf::from(format!("{}/R/utils.R", root)),
                    RFileKind::Source,
                    "helper <- function() 1\n",
                ),
                (
                    PathBuf::from(format!("{}/tests/testthat/test-utils.R", root)),
                    RFileKind::Test,
                    "result <- helper()\n",
                ),
            ],
        );

        // The derived contribution must carry `helper` from R/.
        assert!(
            state.package_state.scope_contribution().r_internal_symbols.contains("helper"),
            "helper must appear in r_internal_symbols for injection into test files"
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let test_uri = Url::parse("file:///work/pkg/tests/testthat/test-utils.R").unwrap();
        let test_arts = make_artifacts(&test_uri, "result <- helper()\n");

        let symbols = resolve_symbols(
            &test_uri,
            test_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );

        assert!(
            symbols.contains_key("helper"),
            "helper must be visible in tests/testthat/ file after end-to-end package derivation. \
             visible: {:?}",
            symbols.keys().collect::<Vec<_>>()
        );
    }

    // ------------------------------------------------------------------
    // load_all(): a script calling devtools::load_all() sees package symbols
    // ------------------------------------------------------------------

    /// End-to-end: a script under `internal/` (neither R/ nor a dev-context
    /// dir) that calls `devtools::load_all()` sees the package's own R/
    /// internal symbols, modeling the attach of the package under development.
    ///
    /// Post-Task-3 the internals are NO LONGER injected into `scope.symbols`
    /// (the old bespoke whole-file injection branch was removed). Instead the
    /// `load_all()` call attaches the package via the `LOAD_ALL_SENTINEL`
    /// package-load event (Task 1), and the symbol resolves position-aware
    /// through the package library's local-dev overlay (Task 2). So we assert
    /// the sentinel reaches `loaded_packages` at the post-call cursor AND that
    /// the overlay resolves `drive_find` under that sentinel.
    #[test]
    fn load_all_caller_sees_internals_via_overlay_after_call() {
        use crate::package_library::LOAD_ALL_SENTINEL;

        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "drive_find <- function() 1\n",
            )],
        );
        assert!(
            state
                .package_state
                .scope_contribution()
                .r_internal_symbols
                .contains("drive_find")
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let script_uri = Url::parse("file:///work/pkg/internal/demo.R").unwrap();
        let script_code = "pkgload::load_all()\ndrive_find()\n";
        let script_arts = make_artifacts(&script_uri, script_code);
        assert!(
            script_arts.calls_dev_load_all,
            "compute_artifacts must flag the load_all() call"
        );

        // The internals are NOT in scope.symbols any more (the old injection is gone).
        let scope = resolve_scope(
            &script_uri,
            script_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );
        assert!(
            !scope.symbols.contains_key("drive_find"),
            "post-Task-3 the internal is resolved via the sentinel/overlay, not scope.symbols"
        );

        // After the load_all() call, the sentinel is attached.
        assert!(
            scope.loaded_packages.contains(LOAD_ALL_SENTINEL)
                || scope.inherited_packages.contains(LOAD_ALL_SENTINEL),
            "the load_all sentinel must be attached after the call. loaded: {:?}, inherited: {:?}",
            scope.loaded_packages,
            scope.inherited_packages
        );

        // And the overlay resolves the R/ internal symbol under that sentinel,
        // so the symbol is NOT reported undefined.
        assert!(
            state
                .package_library
                .is_symbol_from_loaded_packages("drive_find", &[LOAD_ALL_SENTINEL.to_string()]),
            "the local-dev overlay must resolve drive_find under the load_all sentinel"
        );
    }

    /// Completion (deferred from Task 2, now meaningful): in the same caller,
    /// after the `load_all()` line, completion offers the package's R/ internal
    /// `helper`. This mirrors the completion handler, which builds the package
    /// list from the resolved scope's inherited+loaded packages and feeds it to
    /// `get_owned_exports_for_completions`.
    #[test]
    fn load_all_caller_completion_offers_internal_after_call() {
        use crate::package_library::LOAD_ALL_SENTINEL;

        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "helper <- function() 1\n",
            )],
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let script_uri = Url::parse("file:///work/pkg/internal/demo.R").unwrap();
        let script_arts = make_artifacts(&script_uri, "pkgload::load_all()\nhel\n");
        let scope = resolve_scope(
            &script_uri,
            script_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );

        // Mirror the completion handler's package-list construction.
        let pkgs: Vec<String> = scope
            .inherited_packages
            .iter()
            .chain(scope.loaded_packages.iter())
            .cloned()
            .collect();
        assert!(
            pkgs.iter().any(|p| p == LOAD_ALL_SENTINEL),
            "the load_all sentinel must be among the attached packages: {pkgs:?}"
        );
        let owned = state
            .package_library
            .get_owned_exports_for_completions(&pkgs);
        assert!(
            owned.contains_key("helper"),
            "completion must offer the R/ internal `helper` after load_all(); offered: {:?}",
            owned.keys().collect::<Vec<_>>()
        );
    }

    /// Hover (deferred from Task 2): the R/ internal `helper` used after a
    /// `load_all()` call resolves to an owner (the sentinel) rather than the
    /// bare/undefined fallback. NOTE: the sentinel is workspace-local with no
    /// installed help text, so real package help does NOT render; we assert the
    /// weaker TRUE property — the symbol's owner resolves to the sentinel — which
    /// is what stops hover from showing the undefined fallback.
    #[test]
    fn load_all_caller_hover_resolves_internal_owner() {
        use crate::package_library::LOAD_ALL_SENTINEL;

        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "helper <- function() 1\n",
            )],
        );

        assert_eq!(
            state
                .package_library
                .find_package_owner_for_symbol("helper", &[LOAD_ALL_SENTINEL.to_string()]),
            Some(LOAD_ALL_SENTINEL.to_string()),
            "hover owner-resolution must map the internal to the load_all sentinel, \
             not the undefined fallback"
        );
    }

    /// Regression: removing the bespoke load_all injection must NOT change the
    /// dev-context (`vignettes/`) behavior — such files still see the package's
    /// own R/ internals via the `is_dev_context` branch (unchanged), surfaced
    /// directly into `scope.symbols`.
    #[test]
    fn dev_context_vignette_still_sees_internals_after_injection_removal() {
        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "drive_find <- function() 1\n",
            )],
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        // A vignette never calls load_all() but is dev-context.
        let vign_uri = Url::parse("file:///work/pkg/vignettes/intro.R").unwrap();
        let vign_arts = make_artifacts(&vign_uri, "drive_find()\n");
        let symbols = resolve_symbols(
            &vign_uri,
            vign_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );
        assert!(
            symbols.contains_key("drive_find"),
            "dev-context vignette must still see R/ internals. visible: {:?}",
            symbols.keys().collect::<Vec<_>>()
        );
    }

    /// Regression: an R/ file still sees the package's internals via the
    /// `is_r_source_path` branch (unchanged by the injection removal).
    #[test]
    fn r_source_file_still_sees_internals_after_injection_removal() {
        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "drive_find <- function() 1\n",
            )],
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        // A different R/ file uses drive_find (it lives in another R/ file).
        let r_uri = Url::parse("file:///work/pkg/R/other.R").unwrap();
        let r_arts = make_artifacts(&r_uri, "use <- function() drive_find()\n");
        let symbols = resolve_symbols(
            &r_uri,
            r_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );
        assert!(
            symbols.contains_key("drive_find"),
            "R/ file must still see sibling R/ internals. visible: {:?}",
            symbols.keys().collect::<Vec<_>>()
        );
    }

    /// Negative: a file OUTSIDE the package root that calls `load_all()` must
    /// NOT pull in this package's internals. `load_all()` models attaching the
    /// package only for files within its own source tree; a sibling scratch
    /// file in the same workspace must still get real undefined-name
    /// diagnostics.
    #[test]
    fn out_of_root_load_all_script_does_not_see_package_symbols() {
        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "drive_find <- function() 1\n",
            )],
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        // Sibling file under /work, OUTSIDE the package root /work/pkg.
        let script_uri = Url::parse("file:///work/scratch.R").unwrap();
        let script_code = "devtools::load_all()\ndrive_find()\n";
        let script_arts = make_artifacts(&script_uri, script_code);
        assert!(
            script_arts.calls_dev_load_all,
            "the load_all() call is still flagged regardless of path"
        );

        let symbols = resolve_symbols(
            &script_uri,
            script_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );
        assert!(
            !symbols.contains_key("drive_find"),
            "an out-of-root file calling load_all() must not see package internals. visible: {:?}",
            symbols.keys().collect::<Vec<_>>()
        );
    }

    /// End-to-end refresh: after `apply_package_event` derives the package
    /// contribution, the package library's local-dev overlay resolves the
    /// workspace's R/ internal symbols under the load_all sentinel. With no
    /// package, the overlay is absent and resolution returns false.
    #[test]
    fn apply_package_event_refreshes_local_dev_overlay() {
        use crate::package_library::LOAD_ALL_SENTINEL;

        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "helper <- function() 1\n",
            )],
        );

        // Overlay populated from the freshly-derived contribution.
        assert!(
            state
                .package_library
                .is_symbol_from_loaded_packages("helper", &[LOAD_ALL_SENTINEL.to_string()]),
            "load_all overlay must resolve R/ internal symbol after apply_package_event"
        );

        // No package => overlay is None => sentinel resolves nothing.
        let empty = WorldState::new();
        assert!(
            !empty
                .package_library
                .is_symbol_from_loaded_packages("helper", &[LOAD_ALL_SENTINEL.to_string()]),
            "a fresh WorldState (no package) must have no local-dev overlay"
        );
    }

    /// Swapping `state.package_library` for a fresh library (as backend.rs does
    /// on libpath rebuild / init) drops the overlay, but
    /// `refresh_local_dev_overlay` must re-install it so sentinel resolution
    /// survives the swap. Without the refresh call the fresh empty library would
    /// NOT resolve `helper`.
    #[test]
    fn swapped_package_library_keeps_local_dev_overlay() {
        use crate::package_library::{LOAD_ALL_SENTINEL, PackageLibrary};

        let root = "/work/pkg";
        let mut state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "helper <- function() 1\n",
            )],
        );

        // Sanity: the original library's overlay resolves the internal symbol.
        assert!(
            state
                .package_library
                .is_symbol_from_loaded_packages("helper", &[LOAD_ALL_SENTINEL.to_string()]),
            "precondition: overlay resolves helper before the swap"
        );

        // Simulate a backend libpath rebuild / init that replaces the library
        // with a fresh one (whose overlay is None), then re-applies the overlay.
        state.package_library = Arc::new(PackageLibrary::new_empty());
        state.refresh_local_dev_overlay();

        assert!(
            state
                .package_library
                .is_symbol_from_loaded_packages("helper", &[LOAD_ALL_SENTINEL.to_string()]),
            "after a library swap, refresh_local_dev_overlay must re-install the overlay so the sentinel still resolves helper"
        );
    }

    /// Negative: the SAME `internal/` script WITHOUT a `load_all()` call does
    /// NOT see the package's internal symbols — the injection is gated on the
    /// call, not the path.
    #[test]
    fn internal_script_without_load_all_does_not_see_package_symbols() {
        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![(
                PathBuf::from(format!("{}/R/utils.R", root)),
                RFileKind::Source,
                "drive_find <- function() 1\n",
            )],
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let script_uri = Url::parse("file:///work/pkg/internal/demo.R").unwrap();
        let script_code = "drive_find()\n";
        let script_arts = make_artifacts(&script_uri, script_code);
        assert!(!script_arts.calls_dev_load_all);

        let symbols = resolve_symbols(
            &script_uri,
            script_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );
        assert!(
            !symbols.contains_key("drive_find"),
            "without load_all(), an internal/ script must not see package internals"
        );
    }

    /// Bare `load_all(".")` (pkgload re-export, no namespace qualifier) also
    /// triggers the package-attach modeling.
    #[test]
    fn bare_load_all_call_is_detected() {
        let uri = Url::parse("file:///work/pkg/internal/x.R").unwrap();
        let arts = make_artifacts(&uri, "load_all(\".\")\nfoo()\n");
        assert!(arts.calls_dev_load_all);
    }

    /// The `pkgload::load_all()` qualified form (devtools re-exports it) is
    /// detected as well; an unrelated `somepkg::load_all()` is not.
    #[test]
    fn pkgload_load_all_detected_other_namespace_not() {
        let uri = Url::parse("file:///work/pkg/internal/x.R").unwrap();
        assert!(make_artifacts(&uri, "pkgload::load_all()\n").calls_dev_load_all);
        assert!(!make_artifacts(&uri, "somepkg::load_all()\n").calls_dev_load_all);
    }

    // ------------------------------------------------------------------
    // Test B: R/ file does NOT see test-only symbol (asymmetry enforced)
    // ------------------------------------------------------------------
    /// End-to-end: a symbol defined only in tests/testthat/ must NOT appear
    /// in scope when resolving inside R/main.R — confirming the asymmetry.
    #[test]
    fn r_file_does_not_see_test_only_symbol_end_to_end() {
        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![
                (
                    PathBuf::from(format!("{}/R/main.R", root)),
                    RFileKind::Source,
                    "result <- test_helper()\n",
                ),
                (
                    PathBuf::from(format!("{}/tests/testthat/test-utils.R", root)),
                    RFileKind::Test,
                    "test_helper <- function() 99\n",
                ),
            ],
        );

        // The derived contribution must NOT carry `test_helper` (test symbols
        // are excluded from r_internal_symbols by build_scope_contribution).
        assert!(
            !state.package_state.scope_contribution().r_internal_symbols.contains("test_helper"),
            "test_helper must NOT appear in r_internal_symbols"
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let main_uri = Url::parse("file:///work/pkg/R/main.R").unwrap();
        let main_arts = make_artifacts(&main_uri, "result <- test_helper()\n");

        let symbols = resolve_symbols(
            &main_uri,
            main_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );

        assert!(
            !symbols.contains_key("test_helper"),
            "test_helper must NOT be visible in R/main.R. \
             visible: {:?}",
            symbols.keys().collect::<Vec<_>>()
        );
    }

    // ------------------------------------------------------------------
    // Test C: helper-*.R defs visible to peer test files (issue #275)
    // ------------------------------------------------------------------

    /// End-to-end: a top-level def in `tests/testthat/helper-fixtures.R`
    /// is visible when resolving scope inside `tests/testthat/test-foo.R`.
    #[test]
    fn helper_top_level_def_visible_in_peer_test_file_end_to_end() {
        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![
                (
                    PathBuf::from(format!("{}/tests/testthat/helper-fixtures.R", root)),
                    RFileKind::Test,
                    "fixture <- function() 1\n",
                ),
                (
                    PathBuf::from(format!("{}/tests/testthat/test-foo.R", root)),
                    RFileKind::Test,
                    "result <- fixture()\n",
                ),
            ],
        );

        // Helper symbol must be carried in `test_helper_symbols`.
        assert!(
            state
                .package_state
                .scope_contribution()
                .test_helper_symbols
                .values()
                .any(|syms| syms.contains("fixture")),
            "fixture must appear in test_helper_symbols, got: {:?}",
            state.package_state.scope_contribution().test_helper_symbols,
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let test_uri = Url::parse("file:///work/pkg/tests/testthat/test-foo.R").unwrap();
        let test_arts = make_artifacts(&test_uri, "result <- fixture()\n");

        let symbols = resolve_symbols(
            &test_uri,
            test_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );

        assert!(
            symbols.contains_key("fixture"),
            "helper top-level def must be visible in peer tests/testthat/ test file. \
             visible: {:?}",
            symbols.keys().collect::<Vec<_>>()
        );
    }

    // ------------------------------------------------------------------
    // Test D: helper-*.R defs NOT visible in R/ files (asymmetry holds)
    // ------------------------------------------------------------------

    /// End-to-end: a top-level def in `tests/testthat/helper-fixtures.R` is
    /// NOT visible when resolving scope inside `R/main.R` — preserves the
    /// existing one-way visibility (tests/testthat/ → R/, never the reverse).
    #[test]
    fn helper_top_level_def_not_visible_in_r_dir_end_to_end() {
        let root = "/work/pkg";
        let state = build_state_with_files(
            root,
            vec![
                (
                    PathBuf::from(format!("{}/R/main.R", root)),
                    RFileKind::Source,
                    "result <- fixture()\n",
                ),
                (
                    PathBuf::from(format!("{}/tests/testthat/helper-fixtures.R", root)),
                    RFileKind::Test,
                    "fixture <- function() 1\n",
                ),
            ],
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let main_uri = Url::parse("file:///work/pkg/R/main.R").unwrap();
        let main_arts = make_artifacts(&main_uri, "result <- fixture()\n");

        let symbols = resolve_symbols(
            &main_uri,
            main_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );

        assert!(
            !symbols.contains_key("fixture"),
            "helper top-level def must NOT be visible in R/main.R. \
             visible: {:?}",
            symbols.keys().collect::<Vec<_>>()
        );
    }

    // ------------------------------------------------------------------
    // Test E: testthat implicit attachment under tests/testthat/ (issue #275)
    // ------------------------------------------------------------------

    /// End-to-end: when `testthat` is declared in `DESCRIPTION` (Suggests:),
    /// scope resolution for a file under `tests/testthat/` lists `testthat`
    /// in `inherited_packages` — modelling the implicit `library(testthat)`
    /// that `tests/testthat.R` does before sourcing test files.
    #[test]
    fn testthat_attached_to_test_file_scope_when_in_suggests() {
        use crate::package_state::{DescriptionInput, PackageInputDelta};

        let root = "/work/pkg";
        let mut state = WorldState::new();
        state.package_inputs.workspace_root = Some(PathBuf::from(root));
        state.package_inputs.package_mode = crate::cross_file::config::PackageMode::Auto;
        state.package_inputs.description = Some(DescriptionInput {
            text: "Package: foo\nSuggests: testthat\n".into(),
        });
        let test_path = PathBuf::from(format!("{}/tests/testthat/test-foo.R", root));
        let test_text: Arc<str> = "expect_equal(1, 1)\n".into();
        state.package_inputs.r_files.insert(
            test_path,
            crate::package_state::RFileInput {
                kind: RFileKind::Test,
                text: test_text.clone(),
                content_digest: crate::package_state::ContentDigest::of(&test_text),
            },
        );
        state.apply_package_event(&PackageInputDelta::Initial);

        // The derived contribution must list testthat as an attached package.
        assert!(
            state
                .package_state
                .scope_contribution()
                .test_attached_packages
                .contains("testthat"),
            "testthat must appear in test_attached_packages, got: {:?}",
            state.package_state.scope_contribution().test_attached_packages,
        );

        // Resolved scope under tests/testthat/ must carry testthat in inherited_packages.
        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let test_uri = Url::parse("file:///work/pkg/tests/testthat/test-foo.R").unwrap();
        let test_arts = make_artifacts(&test_uri, &test_text);

        let get_artifacts = |u: &Url| -> Option<Arc<ScopeArtifacts>> {
            if u == &test_uri { Some(test_arts.clone()) } else { None }
        };
        let get_metadata =
            |_u: &Url| -> Option<Arc<crate::cross_file::types::CrossFileMetadata>> { None };
        let graph = DependencyGraph::new();

        let scope = scope_at_position_with_graph(
            &test_uri,
            u32::MAX,
            u32::MAX,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            TEST_MAX_CHAIN_DEPTH,
            &HashSet::new(),
            false,
            crate::cross_file::config::BackwardDependencyMode::Explicit,
            &|| false,
            Some(state.package_state.scope_contribution()),
            None,
        );

        assert!(
            scope.inherited_packages.contains("testthat"),
            "testthat must be in inherited_packages for a file under tests/testthat/. \
             got: {:?}",
            scope.inherited_packages,
        );
    }

    /// End-to-end: testthat must NOT be attached to scope for a file under R/
    /// even if declared in DESCRIPTION — package mode keeps R/ free of
    /// test-only attachments so legitimate undefined-variable diagnostics fire.
    #[test]
    #[allow(non_snake_case)]
    fn testthat_not_attached_to_R_file_scope() {
        use crate::package_state::{DescriptionInput, PackageInputDelta};

        let root = "/work/pkg";
        let mut state = WorldState::new();
        state.package_inputs.workspace_root = Some(PathBuf::from(root));
        state.package_inputs.package_mode = crate::cross_file::config::PackageMode::Auto;
        state.package_inputs.description = Some(DescriptionInput {
            text: "Package: foo\nSuggests: testthat\n".into(),
        });
        let r_path = PathBuf::from(format!("{}/R/main.R", root));
        let r_text: Arc<str> = "f <- function() 1\n".into();
        state.package_inputs.r_files.insert(
            r_path,
            crate::package_state::RFileInput {
                kind: RFileKind::Source,
                text: r_text.clone(),
                content_digest: crate::package_state::ContentDigest::of(&r_text),
            },
        );
        state.apply_package_event(&PackageInputDelta::Initial);

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let main_uri = Url::parse("file:///work/pkg/R/main.R").unwrap();
        let main_arts = make_artifacts(&main_uri, &r_text);

        let get_artifacts = |u: &Url| -> Option<Arc<ScopeArtifacts>> {
            if u == &main_uri { Some(main_arts.clone()) } else { None }
        };
        let get_metadata =
            |_u: &Url| -> Option<Arc<crate::cross_file::types::CrossFileMetadata>> { None };
        let graph = DependencyGraph::new();

        let scope = scope_at_position_with_graph(
            &main_uri,
            u32::MAX,
            u32::MAX,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            TEST_MAX_CHAIN_DEPTH,
            &HashSet::new(),
            false,
            crate::cross_file::config::BackwardDependencyMode::Explicit,
            &|| false,
            Some(state.package_state.scope_contribution()),
            None,
        );

        assert!(
            !scope.inherited_packages.contains("testthat"),
            "testthat must NOT leak into inherited_packages for files under R/. \
             got: {:?}",
            scope.inherited_packages,
        );
    }

    // ------------------------------------------------------------------
    // Test F: a helper file does NOT see its own top-level defs via the
    //         contribution (Codex review, issue 1)
    // ------------------------------------------------------------------

    /// End-to-end regression: a helper file must NOT see its OWN later
    /// top-level def via the package contribution. With per-helper-path
    /// keying in `test_helper_symbols`, the scope-injection layer skips
    /// the queried helper's own entry — so a `use_x()` call earlier than
    /// the `x <- ...` definition in the same helper still triggers the
    /// forward-reference / undefined-variable diagnostic. Peer helpers
    /// and `test-*.R` siblings continue to see all helper defs because
    /// they have a different path.
    ///
    /// Layout:
    ///   helper-a.R line 0: (empty: query position)
    ///   helper-a.R line 1: `fixture_a <- function() 1`
    ///   helper-b.R line 0: `fixture_b <- function() 2`
    ///
    /// Querying helper-a at line 0 col 0 should see `fixture_b` (peer helper),
    /// NOT `fixture_a` (self-contribution skipped).
    #[test]
    fn helper_file_does_not_see_own_top_level_defs_via_contribution() {
        let root = "/work/pkg";
        let helper_a_text = "\nfixture_a <- function() 1\n";
        let state = build_state_with_files(
            root,
            vec![
                (
                    PathBuf::from(format!("{}/tests/testthat/helper-a.R", root)),
                    RFileKind::Test,
                    helper_a_text,
                ),
                (
                    PathBuf::from(format!("{}/tests/testthat/helper-b.R", root)),
                    RFileKind::Test,
                    "fixture_b <- function() 2\n",
                ),
            ],
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();
        let helper_a_uri =
            Url::parse("file:///work/pkg/tests/testthat/helper-a.R").unwrap();
        let helper_a_arts = make_artifacts(&helper_a_uri, helper_a_text);
        let get_artifacts = |u: &Url| -> Option<Arc<ScopeArtifacts>> {
            if u == &helper_a_uri {
                Some(helper_a_arts.clone())
            } else {
                None
            }
        };
        let get_metadata =
            |_u: &Url| -> Option<Arc<crate::cross_file::types::CrossFileMetadata>> {
                None
            };
        let graph = DependencyGraph::new();

        // Query at line 0 col 0 — strictly before the local Def at line 1.
        let scope = scope_at_position_with_graph(
            &helper_a_uri,
            0,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            TEST_MAX_CHAIN_DEPTH,
            &HashSet::new(),
            false,
            crate::cross_file::config::BackwardDependencyMode::Explicit,
            &|| false,
            Some(state.package_state.scope_contribution()),
            None,
        );

        assert!(
            !scope.symbols.contains_key("fixture_a"),
            "helper-a.R must NOT see its own `fixture_a` via the package contribution \
             at line 0 (before the local Def at line 1). symbols: {:?}",
            scope.symbols.keys().collect::<Vec<_>>(),
        );
        // helper-a.R is alphabetically before helper-b.R, so helper-a does
        // NOT see helper-b's defs (testthat sources helpers via
        // `sort()`-then-source, so helper-b runs strictly later).
        assert!(
            !scope.symbols.contains_key("fixture_b"),
            "helper-a.R must NOT see alphabetically-later helper-b.R's `fixture_b`. \
             symbols: {:?}",
            scope.symbols.keys().collect::<Vec<_>>(),
        );
    }

    // ------------------------------------------------------------------
    // Test G: helper sourcing order — helper-b sees helper-a (earlier),
    //         test-foo sees both (all helpers sourced first).
    //         Codex follow-up review, issue 1.
    // ------------------------------------------------------------------

    /// `testthat::source_test_helpers` sources `^helper.*\\.[rR]$` files in
    /// `sort()` order, so a later helper sees earlier ones but not vice
    /// versa. Non-helper test files run after all helpers have been
    /// sourced, so they see all helpers.
    #[test]
    fn helper_sourcing_order_matches_testthat_sort_semantics() {
        let root = "/work/pkg";
        let helper_a_text = "fixture_a <- function() 1\n";
        let helper_b_text = "\nfixture_b <- function() 2\n";
        let state = build_state_with_files(
            root,
            vec![
                (
                    PathBuf::from(format!("{}/tests/testthat/helper-a.R", root)),
                    RFileKind::Test,
                    helper_a_text,
                ),
                (
                    PathBuf::from(format!("{}/tests/testthat/helper-b.R", root)),
                    RFileKind::Test,
                    helper_b_text,
                ),
                (
                    PathBuf::from(format!("{}/tests/testthat/test-foo.R", root)),
                    RFileKind::Test,
                    "result <- fixture_a() + fixture_b()\n",
                ),
            ],
        );

        let workspace_root = Url::parse("file:///work/pkg").unwrap();

        // helper-b.R MUST see fixture_a (helper-a sorts before helper-b).
        // Query at line 0 col 0 so the local Def for fixture_b (line 1) is
        // NOT yet applied — that isolates the contribution path.
        let helper_b_uri = Url::parse("file:///work/pkg/tests/testthat/helper-b.R").unwrap();
        let helper_b_arts = make_artifacts(&helper_b_uri, helper_b_text);
        let get_artifacts = |u: &Url| -> Option<Arc<ScopeArtifacts>> {
            if u == &helper_b_uri {
                Some(helper_b_arts.clone())
            } else {
                None
            }
        };
        let get_metadata =
            |_u: &Url| -> Option<Arc<crate::cross_file::types::CrossFileMetadata>> { None };
        let graph = DependencyGraph::new();
        let scope_b = scope_at_position_with_graph(
            &helper_b_uri,
            0,
            0,
            &get_artifacts,
            &get_metadata,
            &graph,
            Some(&workspace_root),
            TEST_MAX_CHAIN_DEPTH,
            &HashSet::new(),
            false,
            crate::cross_file::config::BackwardDependencyMode::Explicit,
            &|| false,
            Some(state.package_state.scope_contribution()),
            None,
        );
        let symbols_b = scope_b.symbols;
        assert!(
            symbols_b.contains_key("fixture_a"),
            "helper-b.R must see fixture_a from alphabetically-earlier helper-a.R. \
             symbols: {:?}",
            symbols_b.keys().collect::<Vec<_>>(),
        );
        assert!(
            !symbols_b.contains_key("fixture_b"),
            "helper-b.R must NOT see its own fixture_b at (0,0), before the local def \
             at line 1. symbols: {:?}",
            symbols_b.keys().collect::<Vec<_>>(),
        );

        // test-foo.R MUST see both fixture_a and fixture_b (all helpers
        // sourced before any test file runs).
        let test_uri = Url::parse("file:///work/pkg/tests/testthat/test-foo.R").unwrap();
        let test_arts = make_artifacts(&test_uri, "result <- fixture_a() + fixture_b()\n");
        let symbols_test = resolve_symbols(
            &test_uri,
            test_arts,
            &workspace_root,
            state.package_state.scope_contribution(),
        );
        assert!(
            symbols_test.contains_key("fixture_a"),
            "test-foo.R must see fixture_a from helper-a.R. symbols: {:?}",
            symbols_test.keys().collect::<Vec<_>>(),
        );
        assert!(
            symbols_test.contains_key("fixture_b"),
            "test-foo.R must see fixture_b from helper-b.R. symbols: {:?}",
            symbols_test.keys().collect::<Vec<_>>(),
        );
    }
}
