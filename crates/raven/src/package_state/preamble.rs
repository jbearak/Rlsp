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
//! best-effort, bounded, and suppressive-only — over-harvesting can mask a
//! false positive but can never fabricate a diagnostic. Scans normally read
//! disk, while live-editor refreshes can overlay authoritative open buffers.
//! Path
//! resolution uses forward-source semantics (`PathContext::from_metadata`
//! with empty metadata), so a preamble's relative `source()` targets anchor
//! at the implicit testthat working directory and computed
//! `file.path()`/`normalizePath()`/variable paths fold via
//! `cross_file::static_path` — the two halves of issue #638 that make the
//! sourced closure statically resolvable in the first place.
//!
//! Freshness: results enter `PackageInputs` (`preamble_sourced_*`) and are
//! refreshed when a preamble file or any file in `preamble_sourced_files`
//! changes in an open buffer or on disk. The per-preamble source-file index
//! lets watched changes rescan only closures that contain the affected path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_lsp::lsp_types::Url;

/// Authoritative in-memory text keyed by lexical or canonical file path.
pub(crate) type PreambleTextOverrides = BTreeMap<PathBuf, Arc<str>>;

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
    /// Canonical sourced-file closure for each preamble. This routing index
    /// identifies which closures intersect an edited helper without rebuilding
    /// unrelated preambles.
    pub sourced_files_by_preamble: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
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
    scan_testthat_preambles_with_overrides_and_exclusions(
        workspace_root,
        &PreambleTextOverrides::new(),
        exclusions,
    )
}

/// Scan all testthat preambles while preferring authoritative open-buffer text
/// from `overrides` for both roots and transitive sourced helpers.
pub(crate) fn scan_testthat_preambles_with_overrides_and_exclusions(
    workspace_root: &Path,
    overrides: &PreambleTextOverrides,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> PreambleScan {
    let mut scan = PreambleScan::default();
    let preamble_dir = workspace_root.join("tests").join("testthat");
    let workspace_url = Url::from_file_path(workspace_root).ok();

    let mut preamble_paths: Vec<PathBuf> = std::fs::read_dir(&preamble_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_testthat_preamble_path(p, workspace_root))
        .collect();
    preamble_paths.extend(
        overrides
            .keys()
            .filter(|p| is_testthat_preamble_path(p, workspace_root))
            .cloned(),
    );
    preamble_paths.sort();
    preamble_paths.dedup();

    for preamble_path in preamble_paths {
        scan_preamble_into(
            &mut scan,
            preamble_path,
            workspace_url.as_ref(),
            overrides,
            exclusions,
        );
    }
    rebuild_sourced_files_union(&mut scan);
    scan
}

/// Refresh only preambles whose root or prior sourced closure intersects an
/// affected path, retaining the prior results for all unrelated preambles.
pub(crate) fn rescan_testthat_preambles_for_paths_with_overrides_and_exclusions(
    workspace_root: &Path,
    previous: &PreambleScan,
    affected_paths: &[PathBuf],
    overrides: &PreambleTextOverrides,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> PreambleScan {
    let mut scan = previous.clone();
    let workspace_url = Url::from_file_path(workspace_root).ok();
    let canonical_affected: Vec<PathBuf> = affected_paths
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect();
    let mut affected_preambles = BTreeSet::new();

    for path in affected_paths {
        if is_testthat_preamble_path(path, workspace_root) {
            affected_preambles.insert(path.clone());
        }
    }
    for preamble in previous
        .symbols
        .keys()
        .chain(previous.attached_packages.keys())
        .chain(previous.sourced_files_by_preamble.keys())
    {
        let canonical_preamble = preamble.canonicalize().unwrap_or_else(|_| preamble.clone());
        if canonical_affected.contains(&canonical_preamble)
            || previous
                .sourced_files_by_preamble
                .get(preamble)
                .is_some_and(|files| {
                    canonical_affected
                        .iter()
                        .any(|affected| files.contains(affected))
                })
        {
            affected_preambles.insert(preamble.clone());
        }
    }

    for preamble_path in affected_preambles {
        scan.symbols.remove(&preamble_path);
        scan.attached_packages.remove(&preamble_path);
        scan.sourced_files_by_preamble.remove(&preamble_path);
        scan_preamble_into(
            &mut scan,
            preamble_path,
            workspace_url.as_ref(),
            overrides,
            exclusions,
        );
    }
    rebuild_sourced_files_union(&mut scan);
    scan
}

pub(crate) fn is_testthat_preamble_path(path: &Path, workspace_root: &Path) -> bool {
    let preamble_dir = workspace_root.join("tests/testthat");
    path.parent() == Some(preamble_dir.as_path())
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(super::is_test_preamble_filename)
}

fn scan_preamble_into(
    scan: &mut PreambleScan,
    preamble_path: PathBuf,
    workspace_url: Option<&Url>,
    overrides: &PreambleTextOverrides,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) {
    if !exclusions.is_empty() && exclusions.is_excluded_path(&preamble_path) {
        return;
    }
    let Some(text) = read_source_with_overrides(&preamble_path, overrides) else {
        return;
    };
    let (symbols, attached, sourced) =
        scan_one_preamble(&preamble_path, text, workspace_url, overrides, exclusions);
    if !symbols.is_empty() {
        scan.symbols.insert(preamble_path.clone(), symbols);
    }
    if !attached.is_empty() {
        scan.attached_packages
            .insert(preamble_path.clone(), attached);
    }
    if !sourced.is_empty() {
        scan.sourced_files_by_preamble
            .insert(preamble_path, sourced);
    }
}

fn rebuild_sourced_files_union(scan: &mut PreambleScan) {
    scan.sourced_files = scan
        .sourced_files_by_preamble
        .values()
        .flat_map(|files| files.iter().cloned())
        .collect();
}

fn read_source_with_overrides(path: &Path, overrides: &PreambleTextOverrides) -> Option<String> {
    if let Some(text) = overrides.get(path) {
        return Some(text.to_string());
    }
    let canonical = path.canonicalize().ok();
    if let Some(text) = canonical.as_ref().and_then(|path| overrides.get(path)) {
        return Some(text.to_string());
    }
    crate::state::read_source(path).ok()
}

/// Follow one preamble file's transitive static `source()` targets, harvesting
/// top-level defs and attaches from each target (but not from the preamble
/// itself). Mirrors `rprofile.rs`'s worklist loop.
fn scan_one_preamble(
    preamble_path: &Path,
    preamble_text: String,
    workspace_url: Option<&Url>,
    overrides: &PreambleTextOverrides,
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
            if let Some(sourced_text) = read_source_with_overrides(&resolved, overrides) {
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
        assert!(
            scan.sourced_files_by_preamble
                .get(&helper)
                .is_some_and(|files| files.contains(&canonical_helpers))
        );
    }

    #[test]
    fn incremental_rescan_keeps_unaffected_preamble_closure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        let helper_a = root.join("scripts/a.R");
        let helper_b = root.join("scripts/b.R");
        let preamble_a = root.join("tests/testthat/helper-a.R");
        let preamble_b = root.join("tests/testthat/helper-b.R");
        std::fs::write(&helper_a, "a_old <- 1\n").unwrap();
        std::fs::write(&helper_b, "b_old <- 1\n").unwrap();
        std::fs::write(&preamble_a, "source(\"../../scripts/a.R\")\n").unwrap();
        std::fs::write(&preamble_b, "source(\"../../scripts/b.R\")\n").unwrap();
        let initial = scan_testthat_preambles_with_exclusions(root, &no_exclusions());

        std::fs::write(&helper_a, "a_new <- 1\n").unwrap();
        // If the incremental path rebuilt every preamble, this unrelated
        // closure would disappear after its helper is removed.
        std::fs::remove_file(&helper_b).unwrap();
        let scan = rescan_testthat_preambles_for_paths_with_overrides_and_exclusions(
            root,
            &initial,
            std::slice::from_ref(&helper_a),
            &PreambleTextOverrides::new(),
            &no_exclusions(),
        );

        let symbols_a = scan.symbols.get(&preamble_a).unwrap();
        assert!(symbols_a.contains("a_new"));
        assert!(!symbols_a.contains("a_old"));
        assert!(
            scan.symbols
                .get(&preamble_b)
                .is_some_and(|symbols| symbols.contains("b_old")),
            "unaffected preamble must retain its previous closure"
        );
    }

    #[test]
    fn open_buffer_overrides_apply_to_root_and_sourced_helper() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        let helper_a = root.join("scripts/a.R");
        let helper_b = root.join("scripts/b.R");
        let preamble = root.join("tests/testthat/helper-project.R");
        std::fs::write(&helper_a, "disk_a <- 1\n").unwrap();
        std::fs::write(&helper_b, "disk_b <- 1\n").unwrap();
        std::fs::write(&preamble, "source(\"../../scripts/a.R\")\n").unwrap();
        let overrides = PreambleTextOverrides::from([
            (
                preamble.clone(),
                Arc::<str>::from("source(\"../../scripts/b.R\")\n"),
            ),
            (helper_b.clone(), Arc::<str>::from("buffer_b <- 1\n")),
        ]);

        let scan = scan_testthat_preambles_with_overrides_and_exclusions(
            root,
            &overrides,
            &no_exclusions(),
        );
        let symbols = scan.symbols.get(&preamble).unwrap();
        assert!(symbols.contains("buffer_b"));
        assert!(!symbols.contains("disk_a"));
        assert!(!symbols.contains("disk_b"));
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
