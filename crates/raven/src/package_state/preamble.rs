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
//! Path resolution uses metadata-free forward-source semantics through the
//! shared package-state closure walker, so a preamble's relative `source()`
//! targets anchor
//! at the implicit testthat working directory and computed
//! `file.path()`/`normalizePath()`/variable paths fold via
//! `cross_file::static_path` — the two halves of issue #638 that make the
//! sourced closure statically resolvable in the first place.
//!
//! Freshness: results enter `PackageInputs` (`preamble_sourced_*`) and are
//! refreshed when a preamble file or any file in `preamble_sourced_files`
//! changes in an open buffer or on disk. The per-preamble source-file index
//! lets watched changes rescan only closures that contain the affected path.

use ropey::Rope;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::Url;

/// Authoritative in-memory text keyed by lexical or canonical file path.
///
/// Values remain as cheaply cloned ropes while the world-state lock is held;
/// [`read_source_with_overrides`] materializes only paths the preamble scan
/// actually reaches. This preserves unsaved newly referenced/transitive
/// helpers without flattening every unrelated open document during snapshot.
pub(crate) type PreambleTextOverrides = BTreeMap<PathBuf, Rope>;

/// Keyed preamble results retained across an incremental rescan.
///
/// Unlike [`PreambleScan`], this snapshot deliberately has no derived
/// `sourced_files` union. Keeping the keyed-only shape distinct makes it
/// impossible for callers to mistake a lock-cheap snapshot for a complete scan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PreambleSnapshot {
    /// Per-preamble: top-level symbol names harvested from its transitive
    /// `source()` targets (NOT the preamble's own defs).
    pub(crate) symbols: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Per-preamble: packages attached by its transitive `source()` targets.
    pub(crate) attached_packages: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Sourced-target routing closure for each preamble.
    pub(crate) sourced_files_by_preamble: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

impl PreambleSnapshot {
    pub(crate) fn from_inputs(inputs: &super::PackageInputs) -> Self {
        Self {
            symbols: inputs.preamble_sourced_symbols.clone(),
            attached_packages: inputs.preamble_sourced_attached_packages.clone(),
            sourced_files_by_preamble: inputs.preamble_sourced_files_by_preamble.clone(),
        }
    }

    fn into_scan(self) -> PreambleScan {
        let sourced_files = sourced_files_union(&self.sourced_files_by_preamble);
        PreambleScan {
            symbols: self.symbols,
            attached_packages: self.attached_packages,
            sourced_files,
            sourced_files_by_preamble: self.sourced_files_by_preamble,
        }
    }
}

fn sourced_files_union(
    sourced_files_by_preamble: &BTreeMap<PathBuf, BTreeSet<PathBuf>>,
) -> BTreeSet<PathBuf> {
    sourced_files_by_preamble
        .values()
        .flat_map(|files| files.iter().cloned())
        .collect()
}

/// Complete result of scanning every testthat preamble file's sourced closure.
/// Maps are keyed by the preamble file's path exactly as tracked in
/// `PackageInputs.r_files` (root-joined, non-canonical) so
/// `build_scope_contribution` can join them against `RFileFacts` keys.
/// `sourced_files` is always the union of `sourced_files_by_preamble`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PreambleScan {
    /// Per-preamble: top-level symbol names harvested from its transitive
    /// `source()` targets (NOT the preamble's own defs).
    pub symbols: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Per-preamble: packages attached by its transitive `source()` targets.
    pub attached_packages: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Routing paths of all static `source()` targets from any preamble,
    /// including targets that are currently missing or unreadable (preamble
    /// files themselves NOT included). Existing paths are canonicalized;
    /// missing paths use a canonical parent when possible. Used by watched-file
    /// freshness wiring so both edits and later creation trigger a rescan.
    pub sourced_files: BTreeSet<PathBuf>,
    /// Sourced-target routing closure for each preamble. This index identifies
    /// which closures intersect an edited or newly created helper without
    /// rebuilding unrelated preambles.
    pub sourced_files_by_preamble: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

impl PreambleScan {
    /// Whether every preamble recomputed by this scan already matches live
    /// package inputs.
    ///
    /// Async rescans call this under the write lock before mutating live inputs.
    /// Only `rescanned` keys may be installed, so equality for those keys is
    /// enough to prove the derived `sourced_files` union is also unchanged;
    /// unrelated live preamble entries are deliberately ignored.
    /// Conversely, when this returns false, [`apply_rescanned_preambles`] must
    /// change at least one keyed live entry because it replaces every rescanned
    /// key with this scan's corresponding entry.
    pub(crate) fn rescanned_match_inputs(
        &self,
        rescanned: &BTreeSet<PathBuf>,
        inputs: &super::PackageInputs,
    ) -> bool {
        rescanned.iter().all(|preamble| {
            self.symbols.get(preamble) == inputs.preamble_sourced_symbols.get(preamble)
                && self.attached_packages.get(preamble)
                    == inputs.preamble_sourced_attached_packages.get(preamble)
                && self.sourced_files_by_preamble.get(preamble)
                    == inputs.preamble_sourced_files_by_preamble.get(preamble)
        })
    }
}

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
    let mut snapshot = PreambleSnapshot::default();
    let preamble_dir = workspace_root.join("tests").join("testthat");
    let workspace_url = Url::from_file_path(workspace_root).ok();

    let mut preamble_paths: Vec<PathBuf> = std::fs::read_dir(&preamble_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| lexical_testthat_preamble_path(&p, workspace_root))
        .collect();
    preamble_paths.extend(
        overrides
            .keys()
            .filter_map(|p| lexical_testthat_preamble_path(p, workspace_root)),
    );
    preamble_paths.sort();
    preamble_paths.dedup();

    for preamble_path in preamble_paths {
        scan_preamble_into(
            &mut snapshot,
            preamble_path,
            workspace_url.as_ref(),
            overrides,
            exclusions,
            true,
        );
    }
    snapshot.into_scan()
}

/// Scan explicit preamble roots and their closures from a detached seed's
/// captured text map without reopening missing or invalid paths from disk.
pub(crate) fn scan_testthat_preambles_from_captured_texts_and_exclusions(
    workspace_root: &Path,
    preamble_paths: Vec<PathBuf>,
    overrides: &PreambleTextOverrides,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> PreambleScan {
    let mut snapshot = PreambleSnapshot::default();
    let workspace_url = Url::from_file_path(workspace_root).ok();
    for preamble_path in preamble_paths {
        scan_preamble_into(
            &mut snapshot,
            preamble_path,
            workspace_url.as_ref(),
            overrides,
            exclusions,
            false,
        );
    }
    snapshot.into_scan()
}

/// Refresh only preambles whose root or prior sourced closure intersects an
/// affected path, retaining the prior results for all unrelated preambles.
/// Consumes a keyed-only [`PreambleSnapshot`] and returns a complete
/// [`PreambleScan`] whose `sourced_files` union is populated.
///
/// Also returns the set of preamble roots that were rescanned (including ones
/// that vanished and now contribute nothing). Callers that apply the result
/// asynchronously (snapshot → off-lock scan → write-lock apply) must update
/// only those preambles' entries in the *live* state via
/// [`apply_rescanned_preambles`] — wholesale-installing the returned scan would
/// revert any concurrent update to an unrelated preamble (e.g. from the spawned
/// watched-file resync task) back to its snapshot-time entries.
pub(crate) fn rescan_testthat_preambles_for_paths_with_overrides_and_exclusions(
    workspace_root: &Path,
    mut snapshot: PreambleSnapshot,
    affected_paths: &[PathBuf],
    overrides: &PreambleTextOverrides,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> (PreambleScan, BTreeSet<PathBuf>) {
    let workspace_url = Url::from_file_path(workspace_root).ok();
    let routing_affected: BTreeSet<PathBuf> = affected_paths
        .iter()
        .map(|path| canonicalize_for_routing(path))
        .collect();
    // Preamble keys are lexical workspace-root paths. Canonicalize the root
    // once, then derive every key's routing spelling by relative join instead
    // of issuing one `canonicalize()` syscall per known preamble per rescan.
    let routing_workspace_root = canonicalize_for_routing(workspace_root);
    let mut affected_preambles = BTreeSet::new();

    for path in affected_paths {
        if let Some(preamble) = lexical_testthat_preamble_path(path, workspace_root) {
            affected_preambles.insert(preamble);
        }
    }
    for preamble in snapshot
        .symbols
        .keys()
        .chain(snapshot.attached_packages.keys())
        .chain(snapshot.sourced_files_by_preamble.keys())
    {
        let routing_preamble = preamble
            .strip_prefix(workspace_root)
            .map(|relative| routing_workspace_root.join(relative))
            .unwrap_or_else(|_| canonicalize_for_routing(preamble));
        if routing_affected.contains(&routing_preamble)
            || snapshot
                .sourced_files_by_preamble
                .get(preamble)
                .is_some_and(|files| {
                    routing_affected
                        .iter()
                        .any(|affected| files.contains(affected))
                })
        {
            affected_preambles.insert(preamble.clone());
        }
    }

    for preamble_path in &affected_preambles {
        snapshot.symbols.remove(preamble_path);
        snapshot.attached_packages.remove(preamble_path);
        snapshot.sourced_files_by_preamble.remove(preamble_path);
        scan_preamble_into(
            &mut snapshot,
            preamble_path.clone(),
            workspace_url.as_ref(),
            overrides,
            exclusions,
            true,
        );
    }
    (snapshot.into_scan(), affected_preambles)
}

/// Apply the `rescanned` preambles' entries from `scan` directly to live
/// package inputs, leaving every unrelated preamble entry untouched.
///
/// This is the write-lock-side companion of
/// [`rescan_testthat_preambles_for_paths_with_overrides_and_exclusions`] for
/// callers whose scan ran off-lock against a snapshot. A rescanned root absent
/// from a keyed scan map is removed from the corresponding live map, which
/// handles deleted roots and roots whose refreshed contribution is now empty.
/// The derived routing union is always rebuilt from the resulting *live* keyed
/// state so concurrent updates to unrelated preambles participate in it.
/// Returns whether any keyed field or the derived union changed.
pub(crate) fn apply_rescanned_preambles(
    inputs: &mut super::PackageInputs,
    scan: PreambleScan,
    rescanned: &BTreeSet<PathBuf>,
) -> bool {
    let PreambleScan {
        mut symbols,
        mut attached_packages,
        sourced_files: _,
        mut sourced_files_by_preamble,
    } = scan;
    let mut changed = false;
    for preamble in rescanned {
        changed |=
            replace_rescanned_entry(&mut inputs.preamble_sourced_symbols, &mut symbols, preamble);
        changed |= replace_rescanned_entry(
            &mut inputs.preamble_sourced_attached_packages,
            &mut attached_packages,
            preamble,
        );
        changed |= replace_rescanned_entry(
            &mut inputs.preamble_sourced_files_by_preamble,
            &mut sourced_files_by_preamble,
            preamble,
        );
    }

    let sourced_files = sourced_files_union(&inputs.preamble_sourced_files_by_preamble);
    if inputs.preamble_sourced_files != sourced_files {
        inputs.preamble_sourced_files = sourced_files;
        changed = true;
    }
    changed
}

fn replace_rescanned_entry<T: PartialEq>(
    current: &mut BTreeMap<PathBuf, T>,
    scanned: &mut BTreeMap<PathBuf, T>,
    preamble: &Path,
) -> bool {
    let replacement = scanned.remove(preamble);
    if current.get(preamble) == replacement.as_ref() {
        return false;
    }
    match replacement {
        Some(value) => {
            current.insert(preamble.to_path_buf(), value);
        }
        None => {
            current.remove(preamble);
        }
    }
    true
}

pub(crate) fn is_testthat_preamble_path(path: &Path, workspace_root: &Path) -> bool {
    lexical_testthat_preamble_path(path, workspace_root).is_some()
}

/// Normalize a recognized preamble root to the workspace's lexical spelling.
///
/// Open-document aliases can present a preamble through a canonical path while
/// the package scanner discovers it through a symlinked workspace root. Scope
/// maps must use the lexical root-joined spelling (to join `r_files`), so this
/// is the boundary where both spellings collapse to one key.
fn lexical_testthat_preamble_path(path: &Path, workspace_root: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    if !name.to_str().is_some_and(super::is_test_preamble_filename) {
        return None;
    }
    let preamble_dir = workspace_root.join("tests/testthat");
    let parent = path.parent()?;
    if parent == preamble_dir
        || canonicalize_for_routing(parent) == canonicalize_for_routing(&preamble_dir)
    {
        Some(preamble_dir.join(name))
    } else {
        None
    }
}

fn scan_preamble_into(
    snapshot: &mut PreambleSnapshot,
    preamble_path: PathBuf,
    workspace_url: Option<&Url>,
    overrides: &PreambleTextOverrides,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
    allow_disk_fallback: bool,
) {
    if !exclusions.is_empty() && exclusions.is_excluded_path(&preamble_path) {
        return;
    }
    let Some(text) = read_source_with_overrides(&preamble_path, overrides, allow_disk_fallback)
    else {
        return;
    };
    let (symbols, attached, sourced) = scan_one_preamble(
        &preamble_path,
        text,
        workspace_url,
        overrides,
        exclusions,
        allow_disk_fallback,
    );
    if !symbols.is_empty() {
        snapshot.symbols.insert(preamble_path.clone(), symbols);
    }
    if !attached.is_empty() {
        snapshot
            .attached_packages
            .insert(preamble_path.clone(), attached);
    }
    if !sourced.is_empty() {
        snapshot
            .sourced_files_by_preamble
            .insert(preamble_path, sourced);
    }
}

fn read_source_with_overrides(
    path: &Path,
    overrides: &PreambleTextOverrides,
    allow_disk_fallback: bool,
) -> Option<String> {
    if let Some(text) = overrides.get(path) {
        return Some(text.to_string());
    }
    let routing_path = canonicalize_for_routing(path);
    if let Some(text) = overrides.get(&routing_path) {
        return Some(text.to_string());
    }
    allow_disk_fallback
        .then(|| crate::state::read_source(path).ok())
        .flatten()
}

/// Stable watcher-routing spelling for an existing or currently missing path.
///
/// Walk to the nearest existing ancestor, canonicalize that prefix, then append
/// the missing suffix. This preserves symlink identity across delete/create
/// events even when both the target and one or more parent directories are
/// absent.
pub(crate) fn canonicalize_for_routing(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut missing_suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = ancestor.canonicalize() {
            for component in missing_suffix.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) else {
            return path.to_path_buf();
        };
        missing_suffix.push(name.to_os_string());
        ancestor = parent;
    }
}

/// Follow one preamble file's transitive static `source()` targets, harvesting
/// top-level defs and attaches from each target (but not from the preamble
/// itself) through the shared package-state closure walker.
fn scan_one_preamble(
    preamble_path: &Path,
    preamble_text: String,
    workspace_url: Option<&Url>,
    overrides: &PreambleTextOverrides,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
    allow_disk_fallback: bool,
) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<PathBuf>) {
    let mut policy = PreambleClosurePolicy {
        overrides,
        exclusions,
        allow_disk_fallback,
        symbols: BTreeSet::new(),
        attached: BTreeSet::new(),
    };
    let closure =
        super::walk_static_source_closure(preamble_path, preamble_text, workspace_url, &mut policy);
    (policy.symbols, policy.attached, closure.sourced_files)
}

struct PreambleClosurePolicy<'a> {
    overrides: &'a PreambleTextOverrides,
    exclusions: &'a crate::config_file::CompiledWorkspaceExclusions,
    allow_disk_fallback: bool,
    symbols: BTreeSet<String>,
    attached: BTreeSet<String>,
}

impl super::StaticSourceClosurePolicy for PreambleClosurePolicy<'_> {
    fn harvest_root(&self) -> bool {
        false
    }

    fn accept_target(&self, resolved: &Path, _routing_path: &Path) -> bool {
        self.exclusions.is_empty() || !self.exclusions.is_excluded_path(resolved)
    }

    fn read_source(&mut self, resolved: &Path) -> Option<String> {
        read_source_with_overrides(resolved, self.overrides, self.allow_disk_fallback)
    }

    fn harvest(&mut self, facts: &crate::cross_file::source_detect::StaticScriptFacts) {
        super::merge_static_script_prelude(facts, &mut self.symbols, &mut self.attached);
    }
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
    fn sourced_helper_unified_facts_honor_capture_runtime_effects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(
            root.join("scripts/runtime.R"),
            r#"x <- 1
bquote(expr = .(library(dplyr)), where = { rm(x); parent.frame() })
"#,
        )
        .unwrap();
        let preamble = root.join("tests/testthat/helper-project.R");
        std::fs::write(&preamble, "source(\"../../scripts/runtime.R\")\n").unwrap();

        let scan = scan_testthat_preambles_with_exclusions(root, &no_exclusions());
        assert!(
            scan.symbols
                .get(&preamble)
                .is_none_or(|symbols| !symbols.contains("x")),
            "got {:?}",
            scan.symbols
        );
        let attached = scan.attached_packages.get(&preamble).unwrap();
        assert!(attached.contains("dplyr"), "got {attached:?}");
    }

    #[test]
    fn sourced_helper_bquote_function_syntax_follows_runtime_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/child.R"), "child_bound <- 1\n").unwrap();
        std::fs::write(root.join("scripts/outer.R"), "outer_sourced <- 1\n").unwrap();
        std::fs::write(
            root.join("scripts/runtime.R"),
            r#"
                bquote(function() .({
                    top_bound <- 1
                    removed <- 1
                    rm(removed)
                    source("child.R")
                    library(dplyr)
                }))
                outer <- function() {
                    bquote(function() .({
                        outer_only <- 1
                        source("outer.R")
                        library(tidyr)
                    }))
                }
                ordinary <- function() ordinary_only <- 1
            "#,
        )
        .unwrap();
        let preamble = root.join("tests/testthat/helper-project.R");
        std::fs::write(&preamble, "source(\"../../scripts/runtime.R\")\n").unwrap();

        let scan = scan_testthat_preambles_with_exclusions(root, &no_exclusions());
        let symbols = scan.symbols.get(&preamble).expect("helper closure symbols");
        for name in ["top_bound", "child_bound", "outer", "ordinary"] {
            assert!(symbols.contains(name), "{name}: {symbols:?}");
        }
        for name in ["removed", "outer_only", "outer_sourced", "ordinary_only"] {
            assert!(!symbols.contains(name), "{name}: {symbols:?}");
        }
        let attached = scan.attached_packages.get(&preamble).unwrap();
        assert!(attached.contains("dplyr"));
        assert!(!attached.contains("tidyr"));
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
        let mut inputs = crate::package_state::PackageInputs::default();
        inputs.preamble_sourced_symbols = initial.symbols;
        inputs.preamble_sourced_attached_packages = initial.attached_packages;
        inputs.preamble_sourced_files = initial.sourced_files;
        inputs.preamble_sourced_files_by_preamble = initial.sourced_files_by_preamble;
        let previous = PreambleSnapshot::from_inputs(&inputs);

        std::fs::write(&helper_a, "a_new <- 1\n").unwrap();
        // If the incremental path rebuilt every preamble, this unrelated
        // closure would disappear after its helper is removed.
        std::fs::remove_file(&helper_b).unwrap();
        let (scan, rescanned) = rescan_testthat_preambles_for_paths_with_overrides_and_exclusions(
            root,
            previous,
            std::slice::from_ref(&helper_a),
            &PreambleTextOverrides::new(),
            &no_exclusions(),
        );
        assert!(rescanned.contains(&preamble_a));
        assert!(
            !rescanned.contains(&preamble_b),
            "unaffected preamble must not be reported as rescanned"
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
        let expected_union: BTreeSet<PathBuf> = scan
            .sourced_files_by_preamble
            .values()
            .flat_map(|files| files.iter().cloned())
            .collect();
        assert_eq!(
            scan.sourced_files, expected_union,
            "incremental rescan must return a complete sourced-file union"
        );
        assert!(
            scan.sourced_files
                .contains(&canonicalize_for_routing(&helper_a))
        );
        assert!(
            scan.sourced_files
                .contains(&canonicalize_for_routing(&helper_b))
        );
    }

    #[test]
    fn apply_rescan_keeps_concurrent_updates_to_unrelated_preambles() {
        // While A's off-lock rescan was in flight, another refresh committed B.
        // The stale B entries carried by A's scan must never overwrite live B.
        let preamble_a = PathBuf::from("/ws/tests/testthat/helper-a.R");
        let preamble_b = PathBuf::from("/ws/tests/testthat/helper-b.R");
        let set = |s: &str| BTreeSet::from([s.to_string()]);
        let files = |p: &str| BTreeSet::from([PathBuf::from(p)]);

        let mut inputs = crate::package_state::PackageInputs::default();
        inputs
            .preamble_sourced_symbols
            .insert(preamble_a.clone(), set("a_old"));
        inputs
            .preamble_sourced_symbols
            .insert(preamble_b.clone(), set("b_new"));
        inputs
            .preamble_sourced_attached_packages
            .insert(preamble_b.clone(), set("pkg_b_new"));
        inputs
            .preamble_sourced_files_by_preamble
            .insert(preamble_b.clone(), files("/ws/scripts/b_new.R"));

        let mut snapshot = PreambleSnapshot::default();
        snapshot.symbols.insert(preamble_a.clone(), set("a_new"));
        snapshot.symbols.insert(preamble_b.clone(), set("b_stale"));
        snapshot
            .attached_packages
            .insert(preamble_a.clone(), set("pkg_a_new"));
        snapshot
            .attached_packages
            .insert(preamble_b.clone(), set("pkg_b_stale"));
        snapshot
            .sourced_files_by_preamble
            .insert(preamble_a.clone(), files("/ws/scripts/a_new.R"));
        snapshot
            .sourced_files_by_preamble
            .insert(preamble_b.clone(), files("/ws/scripts/b_stale.R"));

        assert!(apply_rescanned_preambles(
            &mut inputs,
            snapshot.into_scan(),
            &BTreeSet::from([preamble_a.clone()]),
        ));

        assert_eq!(
            inputs.preamble_sourced_symbols.get(&preamble_a),
            Some(&set("a_new"))
        );
        assert_eq!(
            inputs.preamble_sourced_symbols.get(&preamble_b),
            Some(&set("b_new"))
        );
        assert_eq!(
            inputs.preamble_sourced_attached_packages.get(&preamble_b),
            Some(&set("pkg_b_new"))
        );
        assert_eq!(
            inputs.preamble_sourced_files_by_preamble.get(&preamble_b),
            Some(&files("/ws/scripts/b_new.R"))
        );
        assert_eq!(
            inputs.preamble_sourced_files,
            BTreeSet::from([
                PathBuf::from("/ws/scripts/a_new.R"),
                PathBuf::from("/ws/scripts/b_new.R"),
            ])
        );
    }

    #[test]
    fn apply_rescan_removes_deleted_or_now_empty_preambles() {
        let deleted = PathBuf::from("/ws/tests/testthat/helper-deleted.R");
        let emptied = PathBuf::from("/ws/tests/testthat/helper-empty.R");
        let unrelated = PathBuf::from("/ws/tests/testthat/helper-live.R");
        let set = |s: &str| BTreeSet::from([s.to_string()]);
        let files = |p: &str| BTreeSet::from([PathBuf::from(p)]);
        let mut inputs = crate::package_state::PackageInputs::default();

        for preamble in [&deleted, &emptied, &unrelated] {
            inputs
                .preamble_sourced_symbols
                .insert(preamble.clone(), set("old_symbol"));
            inputs
                .preamble_sourced_attached_packages
                .insert(preamble.clone(), set("old_package"));
        }
        inputs
            .preamble_sourced_files_by_preamble
            .insert(deleted.clone(), files("/ws/scripts/deleted.R"));
        inputs
            .preamble_sourced_files_by_preamble
            .insert(emptied.clone(), files("/ws/scripts/empty.R"));
        inputs
            .preamble_sourced_files_by_preamble
            .insert(unrelated.clone(), files("/ws/scripts/live.R"));

        assert!(apply_rescanned_preambles(
            &mut inputs,
            PreambleScan::default(),
            &BTreeSet::from([deleted.clone(), emptied.clone()]),
        ));

        for preamble in [&deleted, &emptied] {
            assert!(!inputs.preamble_sourced_symbols.contains_key(preamble));
            assert!(
                !inputs
                    .preamble_sourced_attached_packages
                    .contains_key(preamble)
            );
            assert!(
                !inputs
                    .preamble_sourced_files_by_preamble
                    .contains_key(preamble)
            );
        }
        assert_eq!(
            inputs.preamble_sourced_symbols.get(&unrelated),
            Some(&set("old_symbol"))
        );
        assert_eq!(inputs.preamble_sourced_files, files("/ws/scripts/live.R"));
    }

    #[test]
    fn apply_rescan_rebuilds_derived_union_from_live_keyed_state() {
        let preamble_a = PathBuf::from("/ws/tests/testthat/helper-a.R");
        let preamble_b = PathBuf::from("/ws/tests/testthat/helper-b.R");
        let files = |p: &str| BTreeSet::from([PathBuf::from(p)]);
        let mut inputs = crate::package_state::PackageInputs::default();
        inputs
            .preamble_sourced_files_by_preamble
            .insert(preamble_a.clone(), files("/ws/scripts/a.R"));
        inputs
            .preamble_sourced_files_by_preamble
            .insert(preamble_b, files("/ws/scripts/b-live.R"));
        inputs
            .preamble_sourced_files
            .insert(PathBuf::from("/ws/scripts/stale.R"));

        let mut snapshot = PreambleSnapshot::default();
        snapshot
            .sourced_files_by_preamble
            .insert(preamble_a.clone(), files("/ws/scripts/a.R"));
        assert!(apply_rescanned_preambles(
            &mut inputs,
            snapshot.into_scan(),
            &BTreeSet::from([preamble_a]),
        ));

        assert_eq!(
            inputs.preamble_sourced_files,
            BTreeSet::from([
                PathBuf::from("/ws/scripts/a.R"),
                PathBuf::from("/ws/scripts/b-live.R"),
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_override_keeps_lexical_preamble_key_under_symlinked_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real-pkg");
        let linked_root = dir.path().join("linked-pkg");
        std::fs::create_dir_all(real_root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(real_root.join("scripts")).unwrap();
        symlink(&real_root, &linked_root).unwrap();

        let lexical_preamble = linked_root.join("tests/testthat/helper-project.R");
        let real_preamble = real_root.join("tests/testthat/helper-project.R");
        std::fs::write(&real_preamble, "source(\"../../scripts/on-disk.R\")\n").unwrap();
        std::fs::write(real_root.join("scripts/on-disk.R"), "disk_def <- 1\n").unwrap();
        std::fs::write(real_root.join("scripts/in-buffer.R"), "buffer_def <- 1\n").unwrap();
        let canonical_preamble = real_preamble.canonicalize().unwrap();
        // The buffer remains authoritative after an on-disk deletion. Its
        // canonical alias must still route through the now-missing lexical
        // symlink spelling.
        std::fs::remove_file(&real_preamble).unwrap();

        let overrides = PreambleTextOverrides::from([(
            canonical_preamble.clone(),
            Rope::from_str("source(\"../../scripts/in-buffer.R\")\n"),
        )]);
        let scan = scan_testthat_preambles_with_overrides_and_exclusions(
            &linked_root,
            &overrides,
            &no_exclusions(),
        );

        assert_eq!(scan.symbols.len(), 1, "duplicate preamble keys: {scan:?}");
        let symbols = scan.symbols.get(&lexical_preamble).unwrap();
        assert!(symbols.contains("buffer_def"), "got {symbols:?}");
        assert!(!symbols.contains("disk_def"), "got {symbols:?}");
        assert!(!scan.symbols.contains_key(&canonical_preamble));
    }

    #[test]
    fn snapshot_clones_only_keyed_fields_and_builds_complete_scan() {
        let preamble = PathBuf::from("/ws/tests/testthat/helper-project.R");
        let sourced = PathBuf::from("/ws/scripts/helper.R");
        let mut inputs = crate::package_state::PackageInputs::default();
        inputs
            .preamble_sourced_symbols
            .insert(preamble.clone(), BTreeSet::from(["helper".to_string()]));
        inputs
            .preamble_sourced_attached_packages
            .insert(preamble.clone(), BTreeSet::from(["testthat".to_string()]));
        inputs.preamble_sourced_files.insert(sourced.clone());
        inputs
            .preamble_sourced_files_by_preamble
            .insert(preamble.clone(), BTreeSet::from([sourced]));

        let snapshot = PreambleSnapshot::from_inputs(&inputs);

        assert_eq!(snapshot.symbols, inputs.preamble_sourced_symbols);
        assert_eq!(
            snapshot.attached_packages,
            inputs.preamble_sourced_attached_packages
        );
        assert_eq!(
            snapshot.sourced_files_by_preamble,
            inputs.preamble_sourced_files_by_preamble
        );

        let scan = snapshot.into_scan();
        assert_eq!(scan.sourced_files, inputs.preamble_sourced_files);
    }

    #[test]
    fn rescanned_match_ignores_unrelated_live_updates_but_detects_deletion() {
        let rescanned = PathBuf::from("/ws/tests/testthat/helper-a.R");
        let unrelated = PathBuf::from("/ws/tests/testthat/helper-b.R");
        let mut inputs = crate::package_state::PackageInputs::default();
        inputs
            .preamble_sourced_symbols
            .insert(rescanned.clone(), BTreeSet::from(["a".to_string()]));
        let scan = PreambleSnapshot::from_inputs(&inputs).into_scan();
        let roots = BTreeSet::from([rescanned.clone()]);

        inputs.preamble_sourced_symbols.insert(
            unrelated,
            BTreeSet::from(["new concurrent value".to_string()]),
        );
        assert!(scan.rescanned_match_inputs(&roots, &inputs));

        inputs.preamble_sourced_symbols.remove(&rescanned);
        assert!(!scan.rescanned_match_inputs(&roots, &inputs));
    }

    #[test]
    fn incremental_rescan_follows_a_newly_created_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        let preamble = root.join("tests/testthat/helper-project.R");
        let helper = root.join("scripts/later.R");
        std::fs::write(&preamble, "source(\"../../scripts/later.R\")\n").unwrap();

        let initial = scan_testthat_preambles_with_exclusions(root, &no_exclusions());
        let routed_helper = root.canonicalize().unwrap().join("scripts/later.R");
        assert!(initial.sourced_files.contains(&routed_helper));
        assert!(
            initial
                .sourced_files_by_preamble
                .get(&preamble)
                .is_some_and(|paths| paths.contains(&routed_helper))
        );
        assert!(!initial.symbols.contains_key(&preamble));

        let previous = PreambleSnapshot {
            symbols: initial.symbols,
            attached_packages: initial.attached_packages,
            sourced_files_by_preamble: initial.sourced_files_by_preamble,
        };
        std::fs::write(&helper, "created_def <- 1\n").unwrap();
        let (scan, _) = rescan_testthat_preambles_for_paths_with_overrides_and_exclusions(
            root,
            previous,
            std::slice::from_ref(&helper),
            &PreambleTextOverrides::new(),
            &no_exclusions(),
        );

        assert!(
            scan.symbols
                .get(&preamble)
                .is_some_and(|symbols| symbols.contains("created_def"))
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
                Rope::from_str("source(\"../../scripts/b.R\")\n"),
            ),
            (helper_b.clone(), Rope::from_str("buffer_b <- 1\n")),
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
