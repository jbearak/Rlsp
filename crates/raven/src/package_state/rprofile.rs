//! Static (never-executed) scan of a workspace-root `.Rprofile` into a
//! script-scope prelude: the top-level symbol names it binds and the packages
//! it attaches, plus the same harvested transitively from any literal
//! `source()` targets. See `docs/r-package-dev.md` ("`.Rprofile` prelude") and
//! the design spec. Mirrors `scan_own_package_data_dir`: synchronous, disk-only,
//! best-effort, and safe to call when the file is absent.
//!
//! INVARIANT (suppressive-only): this scan only ever *adds* names/packages to a
//! file's scope. Over-harvesting can mask a false positive but can never
//! fabricate a diagnostic, so it deliberately uses Raven's normal top-level
//! scope construction (which includes conditional top-level assignments) and
//! never executes anything.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::Url;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RprofileScan {
    pub symbols: BTreeSet<String>,
    pub attached_packages: BTreeSet<String>,
    /// Routing paths of accepted literal `source()` targets from `.Rprofile`
    /// (the `.Rprofile` file itself is NOT included), including targets that are
    /// currently missing or unreadable. Existing paths are canonicalized;
    /// missing paths use a canonical parent when possible. Used to rescan on
    /// edits, later creation, and delete/recreate cycles.
    pub sourced_files: BTreeSet<PathBuf>,
}

/// Synchronously scan `<workspace_root>/.Rprofile` (never executing it) into a
/// script-scope prelude. Returns empty when the file is absent or unreadable.
pub fn scan_workspace_rprofile(workspace_root: &Path) -> RprofileScan {
    let rprofile_path = workspace_root.join(".Rprofile");
    // Decode through the shared BOM-aware seam so a UTF-8 BOM at the start of
    // `.Rprofile` does not make the first harvested declaration/source call
    // differ from normal source ingestion (`crate::state::read_source`).
    let Ok(text) = crate::state::read_source(&rprofile_path) else {
        return RprofileScan::default();
    };
    scan_rprofile_worklist(workspace_root, text, None)
}

/// Like [`scan_workspace_rprofile`], but skips the root `.Rprofile` and any
/// transitively sourced helper files matched by `[workspace].exclude`.
pub fn scan_workspace_rprofile_with_exclusions(
    workspace_root: &Path,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> RprofileScan {
    if exclusions.is_empty() {
        return scan_workspace_rprofile(workspace_root);
    }

    let rprofile_path = workspace_root.join(".Rprofile");
    if exclusions.is_excluded_path(&rprofile_path) {
        return RprofileScan::default();
    }
    // Decode through the shared BOM-aware seam so a UTF-8 BOM at the start of
    // `.Rprofile` does not make the first harvested declaration/source call
    // differ from normal source ingestion (`crate::state::read_source`).
    let Ok(text) = crate::state::read_source(&rprofile_path) else {
        return RprofileScan::default();
    };
    scan_rprofile_worklist(workspace_root, text, Some(exclusions))
}

/// Like [`scan_workspace_rprofile`], but seeds the scan with the GIVEN root
/// `.Rprofile` text instead of reading it from disk. Used by the live-buffer
/// path (an open, possibly-unsaved `.Rprofile`) so the prelude reflects
/// in-memory edits before they hit disk. Transitively-`source()`d helpers are
/// still read from disk — the rarer case of an unsaved open helper is not
/// overlaid here (documented save-time gap).
pub fn scan_workspace_rprofile_with_root_text(
    workspace_root: &Path,
    root_text: &str,
) -> RprofileScan {
    scan_rprofile_worklist(workspace_root, root_text.to_string(), None)
}

/// Like [`scan_workspace_rprofile_with_root_text`], but applies
/// `[workspace].exclude` to the root `.Rprofile` and transitive helper files.
pub fn scan_workspace_rprofile_with_root_text_and_exclusions(
    workspace_root: &Path,
    root_text: &str,
    exclusions: &crate::config_file::CompiledWorkspaceExclusions,
) -> RprofileScan {
    if exclusions.is_empty() {
        return scan_workspace_rprofile_with_root_text(workspace_root, root_text);
    }

    let rprofile_path = workspace_root.join(".Rprofile");
    if exclusions.is_excluded_path(&rprofile_path) {
        return RprofileScan::default();
    }
    scan_rprofile_worklist(workspace_root, root_text.to_string(), Some(exclusions))
}

/// Shared scan runner: harvest top-level defs + attached packages from the root
/// `.Rprofile` text, then follow its transitive literal `source()` targets from
/// disk through the package-state static-source closure walker. Both public entry
/// points differ only in where the root text comes from (disk vs. live buffer).
fn scan_rprofile_worklist(
    workspace_root: &Path,
    root_text: String,
    exclusions: Option<&crate::config_file::CompiledWorkspaceExclusions>,
) -> RprofileScan {
    let rprofile_path = workspace_root.join(".Rprofile");
    let workspace_url = Url::from_file_path(workspace_root).ok();
    let renv_activate = workspace_root.join("renv").join("activate.R");
    let mut policy = RprofileClosurePolicy {
        exclusions,
        renv_activate: super::preamble::canonicalize_for_routing(&renv_activate),
        scan: RprofileScan::default(),
    };
    let closure = super::walk_static_source_closure(
        &rprofile_path,
        root_text,
        workspace_url.as_ref(),
        &mut policy,
    );
    policy.scan.sourced_files = closure.sourced_files;
    policy.scan
}

struct RprofileClosurePolicy<'a> {
    exclusions: Option<&'a crate::config_file::CompiledWorkspaceExclusions>,
    renv_activate: PathBuf,
    scan: RprofileScan,
}

impl super::StaticSourceClosurePolicy for RprofileClosurePolicy<'_> {
    fn harvest_root(&self) -> bool {
        true
    }

    fn accept_target(&self, resolved: &Path, routing_path: &Path) -> bool {
        !self
            .exclusions
            .is_some_and(|exclusions| exclusions.is_excluded_path(resolved))
            && routing_path != self.renv_activate
    }

    fn read_source(&mut self, resolved: &Path) -> Option<String> {
        crate::state::read_source(resolved).ok()
    }

    fn harvest(&mut self, facts: &crate::cross_file::source_detect::StaticScriptFacts) {
        super::merge_static_script_prelude(
            facts,
            &mut self.scan.symbols,
            &mut self.scan.attached_packages,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn missing_rprofile_yields_empty() {
        let tmp = TempDir::new().unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert_eq!(scan, RprofileScan::default());
    }

    #[test]
    fn harvests_top_level_assignments() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "my_helper <- function() 1\nCONST = 42\nglob <<- 7\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(scan.symbols.contains("my_helper"), "got {:?}", scan.symbols);
        assert!(scan.symbols.contains("CONST"), "got {:?}", scan.symbols);
        assert!(scan.symbols.contains("glob"), "got {:?}", scan.symbols);
    }

    #[test]
    fn harvests_attached_packages() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "library(stringr)\nrequire(dplyr)\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            scan.attached_packages.contains("stringr"),
            "got {:?}",
            scan.attached_packages
        );
        assert!(
            scan.attached_packages.contains("dplyr"),
            "got {:?}",
            scan.attached_packages
        );
    }

    #[test]
    fn unified_facts_honor_capture_removal_and_reachable_attachment() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            r#"x <- 1
bquote(expr = .(library(dplyr)), where = { rm(x); parent.frame() })
"#,
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(!scan.symbols.contains("x"), "got {:?}", scan.symbols);
        assert!(
            scan.attached_packages.contains("dplyr"),
            "runtime-reachable attachment must survive final prelude harvest: {:?}",
            scan.attached_packages
        );
    }

    #[test]
    fn bquote_function_syntax_facts_follow_runtime_scope() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("child.R"), "child_bound <- 1\n").unwrap();
        fs::write(tmp.path().join("outer.R"), "outer_sourced <- 1\n").unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
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

        let scan = scan_workspace_rprofile(tmp.path());
        for name in ["top_bound", "child_bound", "outer", "ordinary"] {
            assert!(scan.symbols.contains(name), "{name}: {:?}", scan.symbols);
        }
        for name in ["removed", "outer_only", "outer_sourced", "ordinary_only"] {
            assert!(!scan.symbols.contains(name), "{name}: {:?}", scan.symbols);
        }
        assert!(scan.attached_packages.contains("dplyr"));
        assert!(!scan.attached_packages.contains("tidyr"));
    }

    #[test]
    fn function_body_assignments_are_not_harvested() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "outer <- function() { local_only <- 1 }\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(scan.symbols.contains("outer"));
        assert!(
            !scan.symbols.contains("local_only"),
            "function-local must not leak: {:?}",
            scan.symbols
        );
    }

    #[test]
    fn follows_literal_source_with_workspace_fallback() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(
            tmp.path().join("R").join("functions.r"),
            "r_bind <- function() 1\n",
        )
        .unwrap();
        fs::write(tmp.path().join(".Rprofile"), "source(\"R/functions.r\")\n").unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(scan.symbols.contains("r_bind"), "got {:?}", scan.symbols);
    }

    #[test]
    fn source_following_skips_excluded_helper() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("helpers")).unwrap();
        let helper = tmp.path().join("helpers").join("setup.R");
        fs::write(&helper, "excluded_helper <- function() 1\n").unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "profile_local <- 1\nsource(\"helpers/setup.R\")\n",
        )
        .unwrap();
        let exclusions = crate::config_file::compile_workspace_exclusions(
            &serde_json::json!({ "workspace": { "exclude": ["helpers/**"] } }),
            vec![tmp.path().to_path_buf()],
        );

        let scan = scan_workspace_rprofile_with_exclusions(tmp.path(), &exclusions);

        assert!(
            scan.symbols.contains("profile_local"),
            "non-excluded .Rprofile symbols must remain: {:?}",
            scan.symbols
        );
        assert!(
            !scan.symbols.contains("excluded_helper"),
            "excluded sourced helper must not contribute symbols: {:?}",
            scan.symbols
        );
        let canonical_helper = helper.canonicalize().unwrap();
        assert!(
            !scan.sourced_files.contains(&canonical_helper),
            "excluded helper must not be recorded as a followed source: {:?}",
            scan.sourced_files
        );
    }

    #[test]
    fn with_root_text_uses_buffer_not_disk_and_follows_source() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(
            tmp.path().join("R").join("functions.r"),
            "r_bind <- function() 1\n",
        )
        .unwrap();
        // Disk `.Rprofile` defines `disk_only` and sources the helper.
        fs::write(
            tmp.path().join(".Rprofile"),
            "disk_only <- 1\nsource(\"R/functions.r\")\n",
        )
        .unwrap();
        // The in-memory buffer (unsaved) instead defines `buffer_only` and still
        // sources the helper. The scan must reflect the BUFFER, not disk.
        let scan = scan_workspace_rprofile_with_root_text(
            tmp.path(),
            "buffer_only <- 1\nsource(\"R/functions.r\")\n",
        );
        assert!(
            scan.symbols.contains("buffer_only"),
            "must harvest from the in-memory root text: {:?}",
            scan.symbols
        );
        assert!(
            !scan.symbols.contains("disk_only"),
            "must NOT harvest the stale disk root text: {:?}",
            scan.symbols
        );
        assert!(
            scan.symbols.contains("r_bind"),
            "transitive source() helpers still resolve from disk: {:?}",
            scan.symbols
        );
    }

    #[test]
    fn missing_source_target_stays_routed_and_is_harvested_after_creation() {
        let tmp = TempDir::new().unwrap();
        let helper = tmp.path().join("scripts/later.R");
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "source(\"scripts/later.R\")\n",
        )
        .unwrap();

        let initial = scan_workspace_rprofile(tmp.path());
        let routed = super::super::preamble::canonicalize_for_routing(&helper);
        assert!(initial.sourced_files.contains(&routed));
        assert!(!initial.symbols.contains("created_def"));

        fs::write(&helper, "created_def <- 1\n").unwrap();
        let created = scan_workspace_rprofile(tmp.path());
        assert!(created.sourced_files.contains(&routed));
        assert!(created.symbols.contains("created_def"));
    }

    #[test]
    fn missing_transitive_source_target_stays_routed() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("scripts/parent.R");
        let missing = tmp.path().join("scripts/missing.R");
        fs::create_dir_all(parent.parent().unwrap()).unwrap();
        fs::write(&parent, "source(\"missing.R\")\nparent_def <- 1\n").unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "source(\"scripts/parent.R\")\n",
        )
        .unwrap();

        let scan = scan_workspace_rprofile(tmp.path());
        assert!(scan.symbols.contains("parent_def"));
        assert!(
            scan.sourced_files
                .contains(&super::super::preamble::canonicalize_for_routing(&missing))
        );
    }

    #[test]
    fn follows_source_transitively() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("R")).unwrap();
        fs::write(
            tmp.path().join("R").join("r.bind.r"),
            "r_bind <- function() 1\n",
        )
        .unwrap();
        // functions.r sources r.bind.r; in R this resolves via cwd (root), which
        // Raven models with the workspace-root fallback.
        fs::write(
            tmp.path().join("R").join("functions.r"),
            "source(\"R/r.bind.r\")\n",
        )
        .unwrap();
        fs::write(tmp.path().join(".Rprofile"), "source(\"R/functions.r\")\n").unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            scan.symbols.contains("r_bind"),
            "transitive source must resolve: {:?}",
            scan.symbols
        );
    }

    #[test]
    fn attached_packages_followed_through_source() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("setup.R"),
            "library(tidyr)\nhelper <- function() 1\n",
        )
        .unwrap();
        fs::write(tmp.path().join(".Rprofile"), "source(\"setup.R\")\n").unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            scan.attached_packages.contains("tidyr"),
            "got {:?}",
            scan.attached_packages
        );
        assert!(scan.symbols.contains("helper"));
    }

    #[test]
    fn skips_renv_activate() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("renv")).unwrap();
        // activate.R defines machinery we must NOT harvest as user globals.
        fs::write(
            tmp.path().join("renv").join("activate.R"),
            "should_not_leak <- function() 1\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "source(\"renv/activate.R\")\nlocal_def <- 1\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            !scan.symbols.contains("should_not_leak"),
            "renv/activate.R must be skipped: {:?}",
            scan.symbols
        );
        assert!(
            scan.symbols.contains("local_def"),
            "statements after the renv line still harvest"
        );
    }

    #[test]
    fn local_true_source_is_not_followed() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("priv.R"), "private_def <- 1\n").unwrap();
        // source(..., local = TRUE) puts defs in a local env, not globals.
        fs::write(
            tmp.path().join(".Rprofile"),
            "source(\"priv.R\", local = TRUE)\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            !scan.symbols.contains("private_def"),
            "local=TRUE source must not contribute globals: {:?}",
            scan.symbols
        );
    }

    #[test]
    fn dynamic_source_path_is_ignored() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "source(paste0(\"R/\", \"x.R\"))\nstill_here <- 1\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        // No crash; non-literal source() ignored; sibling assignment still harvested.
        assert!(scan.symbols.contains("still_here"));
    }

    #[test]
    fn conditional_top_level_assignment_is_harvested() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "if (interactive()) helper <- function() {}\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            scan.symbols.contains("helper"),
            "conditional top-level assignment must be harvested: {:?}",
            scan.symbols
        );
    }

    #[test]
    fn function_body_source_is_not_followed() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("dev.R"), "dev_only <- 1\n").unwrap();
        // source() inside a function body only runs when the fn is called.
        fs::write(
            tmp.path().join(".Rprofile"),
            "f <- function() source(\"dev.R\")\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            !scan.symbols.contains("dev_only"),
            "function-body source() must not be followed: {:?}",
            scan.symbols
        );
        assert!(scan.symbols.contains("f"));
    }

    #[test]
    fn rprofile_load_all_attaches_sentinel() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".Rprofile"), "pkgload::load_all()\n").unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            scan.attached_packages
                .contains(crate::package_library::LOAD_ALL_SENTINEL),
            "a load_all() in .Rprofile must attach the load_all sentinel: {:?}",
            scan.attached_packages
        );
    }

    #[test]
    fn rprofile_only_load_all_still_attaches() {
        // A profile whose ONLY content is a bare load_all() must still produce a
        // non-empty attached set so the prelude early-return guard passes.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".Rprofile"), "load_all()\n").unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            scan.attached_packages
                .contains(crate::package_library::LOAD_ALL_SENTINEL),
            "bare load_all() must attach the sentinel: {:?}",
            scan.attached_packages
        );
    }

    #[test]
    fn rprofile_load_all_in_function_body_does_not_attach() {
        // A load_all() lexically inside a function body only runs when the
        // function is called, so it must not attach at profile-load time.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "f <- function() pkgload::load_all()\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            !scan
                .attached_packages
                .contains(crate::package_library::LOAD_ALL_SENTINEL),
            "function-body load_all() must not attach the sentinel: {:?}",
            scan.attached_packages
        );
    }

    #[test]
    fn rprofile_load_all_in_quote_does_not_attach() {
        // A load_all() lexically inside a non-evaluating quoting call (e.g.
        // `quote(...)`) captures code without running it, so it must not attach
        // at profile-load time.
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".Rprofile"),
            "quote(devtools::load_all())\n",
        )
        .unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            !scan
                .attached_packages
                .contains(crate::package_library::LOAD_ALL_SENTINEL),
            "quoted load_all() must not attach the sentinel: {:?}",
            scan.attached_packages
        );
    }

    #[test]
    fn rprofile_load_all_followed_through_source() {
        // load_all() in a transitively-sourced helper also attaches the sentinel.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("setup.R"), "pkgload::load_all()\n").unwrap();
        fs::write(tmp.path().join(".Rprofile"), "source(\"setup.R\")\n").unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            scan.attached_packages
                .contains(crate::package_library::LOAD_ALL_SENTINEL),
            "transitively-sourced load_all() must attach the sentinel: {:?}",
            scan.attached_packages
        );
    }

    #[test]
    fn sys_source_without_global_env_is_not_followed() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("priv.R"), "priv_only <- 1\n").unwrap();
        // sys.source() defaults to a non-global env → symbols are not inherited.
        fs::write(tmp.path().join(".Rprofile"), "sys.source(\"priv.R\")\n").unwrap();
        let scan = scan_workspace_rprofile(tmp.path());
        assert!(
            !scan.symbols.contains("priv_only"),
            "sys.source() (non-global env) must not contribute globals: {:?}",
            scan.symbols
        );
    }
}
