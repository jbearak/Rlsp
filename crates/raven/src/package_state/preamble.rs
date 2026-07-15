//! Static (never-executed) scan of testthat preamble files
//! (`tests/testthat/helper*.R` / `setup*.R`) into per-preamble harvested
//! symbol/attach sets: the top-level defs and `library()`/`require()` attaches
//! of each file the preamble transitively `source()`s (issue #638). The
//! preamble file's OWN defs/attaches are deliberately NOT harvested here —
//! they come from the live `RFileFacts` pipeline (`derive_r_file_facts`),
//! which stays fresh on unsaved buffer edits; this scan covers only the
//! sourced closure, which lives outside the tracked package inputs (e.g.
//! `scripts/helpers.R`).
//!
//! Directly mirrors the `.Rprofile` prelude scan (`rprofile.rs`): synchronous,
//! disk-only, best-effort, bounded, and suppressive-only — over-harvesting can
//! mask a false positive but can never fabricate a diagnostic. Path
//! resolution uses forward-source semantics (`PathContext::from_metadata`
//! with empty metadata), so a preamble's relative `source()` targets anchor
//! at the implicit testthat working directory and computed
//! `file.path()`/`normalizePath()`/variable paths fold via
//! `cross_file::static_path` — the two halves of issue #638 that make the
//! sourced closure statically resolvable in the first place.
//!
//! Freshness: results enter `PackageInputs` (`preamble_sourced_*`) and are
//! re-scanned when a preamble file or any file in `preamble_sourced_files`
//! changes ON DISK (watched-file events; see `event.rs`). Unsaved buffer
//! edits to a preamble's `source()` lines take effect on save — a narrower
//! guarantee than the `.Rprofile` prelude's buffer-authoritative path, and an
//! accepted v1 simplification.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::Url;

/// Result of scanning every testthat preamble file's sourced closure.
/// Maps are keyed by the preamble file's path exactly as tracked in
/// `PackageInputs.r_files` (root-joined, non-canonical) so
/// `build_scope_contribution` can join them against `RFileFacts` keys.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PreambleScan {
    /// Per-preamble: top-level symbol names harvested from its transitive
    /// `source()` targets (NOT the preamble's own defs).
    pub symbols: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Per-preamble: packages attached by its transitive `source()` targets.
    pub attached_packages: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Canonicalized paths of all files followed out of any preamble via
    /// `source()` (preamble files themselves NOT included). Used by the
    /// watched-file freshness wiring to rescan when a sourced helper is
    /// edited — mirrors `RprofileScan::sourced_files`.
    pub sourced_files: BTreeSet<PathBuf>,
}

/// Maximum depth of `source()` chains followed out of one preamble file.
const PREAMBLE_MAX_SOURCE_DEPTH: usize = 64;
/// Maximum number of distinct files visited per preamble (cycle + fan-out
/// guard).
const PREAMBLE_MAX_SOURCE_FILES: usize = 1000;

/// Synchronously scan every `helper*.R`/`setup*.R` direct child of
/// `<workspace_root>/tests/testthat/`, following each one's transitive static
/// `source()` targets from disk. Returns an empty scan when the directory is
/// absent. Never executes any R code.
pub fn scan_testthat_preambles_with_exclusions(
    workspace_root: &Path,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> PreambleScan {
    let mut scan = PreambleScan::default();
    let preamble_dir = workspace_root.join("tests").join("testthat");
    let Ok(entries) = std::fs::read_dir(&preamble_dir) else {
        return scan;
    };
    let workspace_url = Url::from_file_path(workspace_root).ok();

    let mut preamble_paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(super::is_test_preamble_filename)
                    .unwrap_or(false)
        })
        .collect();
    preamble_paths.sort();

    for preamble_path in preamble_paths {
        if !exclusions.is_empty() && exclusions.is_excluded_path(&preamble_path) {
            continue;
        }
        let Ok(text) = crate::state::read_source(&preamble_path) else {
            continue;
        };
        let (symbols, attached, sourced) =
            scan_one_preamble(&preamble_path, text, workspace_url.as_ref(), exclusions);
        scan.sourced_files.extend(sourced);
        if !symbols.is_empty() {
            scan.symbols.insert(preamble_path.clone(), symbols);
        }
        if !attached.is_empty() {
            scan.attached_packages.insert(preamble_path, attached);
        }
    }
    scan
}

/// Follow one preamble file's transitive static `source()` targets, harvesting
/// top-level defs and attaches from each target (but not from the preamble
/// itself). Mirrors `rprofile.rs`'s worklist loop.
fn scan_one_preamble(
    preamble_path: &Path,
    preamble_text: String,
    workspace_url: Option<&Url>,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<PathBuf>) {
    let mut symbols: BTreeSet<String> = BTreeSet::new();
    let mut attached: BTreeSet<String> = BTreeSet::new();
    let mut sourced: BTreeSet<PathBuf> = BTreeSet::new();

    // Worklist of (file_path, file_text, depth, harvest). The preamble seeds
    // the walk with harvest=false — its own defs come from RFileFacts.
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    visited.insert(
        preamble_path
            .canonicalize()
            .unwrap_or_else(|_| preamble_path.to_path_buf()),
    );
    let mut worklist: Vec<(PathBuf, String, usize, bool)> =
        vec![(preamble_path.to_path_buf(), preamble_text, 0, false)];

    while let Some((path, text, depth, harvest)) = worklist.pop() {
        if harvest {
            for def in crate::roxygen::extract_top_level_defs(&text) {
                symbols.insert(def);
            }
            for pkg in crate::cross_file::source_detect::extract_attached_packages(&text) {
                attached.insert(pkg);
            }
        }
        if depth >= PREAMBLE_MAX_SOURCE_DEPTH || visited.len() >= PREAMBLE_MAX_SOURCE_FILES {
            continue;
        }
        let Ok(file_uri) = Url::from_file_path(&path) else {
            continue;
        };
        // Forward-source resolution semantics: `from_metadata` with empty
        // metadata gives the preamble its implicit testthat working directory
        // (issue #638) and every file the workspace-root fallback. `# raven:
        // cd` in sourced helpers is not honored here, matching the
        // `.Rprofile` scan's documented exception.
        let Some(ctx) = crate::cross_file::path_resolve::PathContext::from_metadata(
            &file_uri,
            &crate::cross_file::types::CrossFileMetadata::default(),
            workspace_url,
        ) else {
            continue;
        };
        for target in static_source_targets(&text) {
            let Some(resolved) =
                crate::cross_file::path_resolve::resolve_path_with_workspace_fallback(
                    &target, &ctx,
                )
            else {
                continue;
            };
            if !exclusions.is_empty() && exclusions.is_excluded_path(&resolved) {
                continue;
            }
            let canonical = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
            if !visited.insert(canonical.clone()) {
                continue;
            }
            if visited.len() >= PREAMBLE_MAX_SOURCE_FILES {
                break;
            }
            if let Ok(sourced_text) = crate::state::read_source(&resolved) {
                sourced.insert(canonical);
                worklist.push((resolved, sourced_text, depth + 1, true));
            }
        }
    }
    (symbols, attached, sourced)
}

/// Statically-known symbol-contributing `source()` targets in `text` — same
/// filters as the `.Rprofile` scan's `static_source_targets` (non-directive,
/// symbol-inheriting, not function-scoped, literal-or-folded path).
fn static_source_targets(text: &str) -> Vec<String> {
    use tree_sitter::Parser;
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    crate::cross_file::source_detect::detect_source_calls(&tree, text)
        .into_iter()
        .filter(|s| {
            !s.is_directive && s.inherits_symbols() && !s.is_function_scoped && !s.path.is_empty()
        })
        .map(|s| s.path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_exclusions() -> crate::config_file::CompiledWorkspaceExclusions {
        crate::config_file::CompiledWorkspaceExclusions::default()
    }

    /// The issue #638 repro: a helper computing the repo root and sourcing a
    /// scripts/ file through it. The sourced defs must be harvested and keyed
    /// by the helper's path; the helper's own defs must NOT be harvested.
    #[test]
    fn harvests_computed_source_closure_of_helper() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(
            root.join("scripts/helpers.R"),
            "my_theme <- function() 1\nmake_result <- function() 2\nlibrary(glue)\n",
        )
        .unwrap();
        let helper = root.join("tests/testthat/helper-project.R");
        std::fs::write(
            &helper,
            "own_def <- 1\nrepo_root <- normalizePath(file.path(\"..\", \"..\"))\nsource(file.path(repo_root, \"scripts/helpers.R\"))\n",
        )
        .unwrap();

        let scan = scan_testthat_preambles_with_exclusions(root, &no_exclusions());
        let symbols = scan.symbols.get(&helper).expect("helper keyed in scan");
        assert!(symbols.contains("my_theme"));
        assert!(symbols.contains("make_result"));
        assert!(
            !symbols.contains("own_def"),
            "preamble's own defs come from RFileFacts, not the scan"
        );
        assert!(
            !symbols.contains("repo_root"),
            "preamble's own defs come from RFileFacts, not the scan"
        );
        let attached = scan
            .attached_packages
            .get(&helper)
            .expect("attaches harvested");
        assert!(attached.contains("glue"));
        let canonical_helpers = root
            .join("scripts/helpers.R")
            .canonicalize()
            .unwrap_or_else(|_| root.join("scripts/helpers.R"));
        assert!(scan.sourced_files.contains(&canonical_helpers));
    }

    /// Transitive chains are followed; setup*.R files participate; non-preamble
    /// and nested files are ignored as scan roots.
    #[test]
    fn follows_transitive_chain_and_scopes_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests/testthat/fixtures")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(
            root.join("scripts/a.R"),
            "a_def <- 1\nsource(\"scripts/b.R\")\n",
        )
        .unwrap();
        std::fs::write(root.join("scripts/b.R"), "b_def <- 2\n").unwrap();
        let setup = root.join("tests/testthat/setup-env.R");
        std::fs::write(&setup, "source(\"../../scripts/a.R\")\n").unwrap();
        // Not preamble-named and nested — never scan roots.
        std::fs::write(
            root.join("tests/testthat/test-x.R"),
            "source(\"../../scripts/a.R\")\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/testthat/fixtures/helper-nested.R"),
            "source(\"../../../scripts/a.R\")\n",
        )
        .unwrap();

        let scan = scan_testthat_preambles_with_exclusions(root, &no_exclusions());
        assert_eq!(scan.symbols.len(), 1, "only the setup file is a root");
        let symbols = scan.symbols.get(&setup).unwrap();
        assert!(symbols.contains("a_def"));
        assert!(
            symbols.contains("b_def"),
            "transitive source() chain must be followed (scripts/b.R resolves \
             via the workspace-root fallback from scripts/a.R)"
        );
    }

    /// `local = TRUE`, function-scoped, and directive sources contribute
    /// nothing; a missing tests/testthat dir yields an empty scan.
    #[test]
    fn respects_source_filters_and_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            scan_testthat_preambles_with_exclusions(root, &no_exclusions()),
            PreambleScan::default()
        );

        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/x.R"), "x_def <- 1\n").unwrap();
        let helper = root.join("tests/testthat/helper-local.R");
        std::fs::write(
            &helper,
            "source(\"../../scripts/x.R\", local = TRUE)\nf <- function() source(\"../../scripts/x.R\")\n",
        )
        .unwrap();
        let scan = scan_testthat_preambles_with_exclusions(root, &no_exclusions());
        assert!(
            !scan.symbols.contains_key(&helper),
            "local/function-scoped sources must not contribute: {scan:?}"
        );
    }
}
