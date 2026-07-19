//! Integration tests for `source(system.file(...))` resolution.
//!
//! Tests that:
//! 1. `source(system.file("helper.R", package = <self>))` resolves the helper's
//!    top-level definitions into the sourcing file's scope (no undefined-variable
//!    false positives, no "Cannot resolve path" diagnostic).
//! 2. An unresolved cross-package `system.file()` produces ZERO diagnostics
//!    (graceful degradation — silent skip, not a spurious error).
//! 3. Resolution is recoverable across package lifecycle events: a package
//!    installed/removed after startup, or a workspace `Package:` rename, must
//!    re-resolve through the detached two-attempt convergence transaction used
//!    by the `LibpathEvent::Changed` consumer and DESCRIPTION manifest branch,
//!    without the user editing the sourcing file.
//!
//! Run with: `cargo test --release -p raven --test system_file_source`

use std::process::Command;
use tempfile::TempDir;

fn raven_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove `deps`
    path.push("raven");
    path
}

fn run_check(workspace: &std::path::Path) -> String {
    let output = Command::new(raven_binary())
        .args(["check", "--workspace"])
        .arg(workspace)
        .args(["--max-severity", "off", "--no-color"])
        .output()
        .expect("failed to execute raven check");
    // `raven check` exits 0 (clean) or 1 (diagnostics emitted; under
    // `--max-severity off` any diagnostic trips this). Only exit 2 (operator
    // error) or a signal indicate a genuine failure — without this guard the
    // string assertions below would pass vacuously on empty stdout.
    assert!(
        matches!(output.status.code(), Some(0) | Some(1)),
        "raven check failed (status {:?}). stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Escape a filesystem path for embedding in a TOML basic string. On Windows a
/// raw path contains backslashes (and may contain quotes) that TOML would
/// otherwise treat as escape sequences, producing invalid config.
fn toml_escape(path: &std::path::Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// source(system.file("helper.R", package = <self>)) resolves the helper's
/// definitions into scope — no undefined-variable FP, no path diagnostic.
#[test]
fn system_file_self_package_resolves_helper_into_scope() {
    let dir = TempDir::new().unwrap();

    // DESCRIPTION makes this a package workspace
    std::fs::write(
        dir.path().join("DESCRIPTION"),
        "Package: testpkg\nTitle: Test\nVersion: 0.1.0\n",
    )
    .unwrap();

    // inst/helper.R defines a function
    std::fs::create_dir_all(dir.path().join("inst")).unwrap();
    std::fs::write(
        dir.path().join("inst").join("helper.R"),
        "my_helper <- function(x) x + 1\n",
    )
    .unwrap();

    // R/main.R sources via system.file and uses the helper
    std::fs::create_dir_all(dir.path().join("R")).unwrap();
    std::fs::write(
        dir.path().join("R").join("main.R"),
        "source(system.file(\"helper.R\", package = \"testpkg\"))\nresult <- my_helper(42)\n",
    )
    .unwrap();

    let output = run_check(dir.path());
    assert!(
        !output.contains("Cannot resolve path"),
        "system.file(\"helper.R\", package = \"testpkg\") should resolve \
         to inst/helper.R. Output:\n{output}"
    );
    assert!(
        !output.contains("my_helper is not defined")
            && !output.contains("Undefined function: my_helper"),
        "my_helper should be in scope via source(system.file(...)). Output:\n{output}"
    );
}

/// source(system.file("helper.R", package = "otherpkg")) resolves the helper's
/// definitions into scope when otherpkg is present at a configured lib_path —
/// no undefined-variable FP, no path diagnostic.
#[test]
fn system_file_cross_package_installed_resolves_helper_into_scope() {
    let workspace = TempDir::new().unwrap();
    let libdir = TempDir::new().unwrap();

    // Create installed "otherpkg" with helper.R at libdir/otherpkg/helper.R
    // (installed layout: NO inst/ prefix)
    let pkg_dir = libdir.path().join("otherpkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("helper.R"),
        "cross_pkg_fn <- function(x) x * 2\n",
    )
    .unwrap();

    // Workspace DESCRIPTION
    std::fs::write(
        workspace.path().join("DESCRIPTION"),
        "Package: mypkg\nTitle: Test\nVersion: 0.1.0\n",
    )
    .unwrap();

    // raven.toml with additionalLibraryPaths
    std::fs::write(
        workspace.path().join("raven.toml"),
        format!(
            "[packages]\nadditionalLibraryPaths = [\"{}\"]\n",
            toml_escape(libdir.path())
        ),
    )
    .unwrap();

    // R/main.R sources from otherpkg and uses the symbol
    std::fs::create_dir_all(workspace.path().join("R")).unwrap();
    std::fs::write(
        workspace.path().join("R").join("main.R"),
        "source(system.file(\"helper.R\", package = \"otherpkg\"))\nresult <- cross_pkg_fn(42)\n",
    )
    .unwrap();

    let output = run_check(workspace.path());
    assert!(
        !output.contains("Cannot resolve path"),
        "system.file(\"helper.R\", package = \"otherpkg\") should resolve \
         via additionalLibraryPaths. Output:\n{output}"
    );
    assert!(
        !output.contains("cross_pkg_fn is not defined")
            && !output.contains("Undefined function: cross_pkg_fn"),
        "cross_pkg_fn should be in scope via cross-package system.file(). Output:\n{output}"
    );
}

/// An unresolved cross-package system.file() produces ZERO diagnostics —
/// graceful degradation matching pre-feature behavior (silent skip).
#[test]
fn system_file_cross_package_unresolved_produces_no_diagnostics() {
    let dir = TempDir::new().unwrap();

    // DESCRIPTION for self package
    std::fs::write(
        dir.path().join("DESCRIPTION"),
        "Package: mypkg\nTitle: Test\nVersion: 0.1.0\n",
    )
    .unwrap();

    // R/main.R sources from a different (not-installed) package
    std::fs::create_dir_all(dir.path().join("R")).unwrap();
    std::fs::write(
        dir.path().join("R").join("main.R"),
        "source(system.file(\"scripts/setup.R\", package = \"otherpkg\"))\nx <- 1\n",
    )
    .unwrap();

    let output = run_check(dir.path());
    assert!(
        !output.contains("Cannot resolve path"),
        "Unresolved cross-package system.file() must not produce path diagnostics. \
         Output:\n{output}"
    );
    assert!(
        !output.contains("system.file") && !output.contains("otherpkg"),
        "No diagnostic should mention system.file or the unresolved package. \
         Output:\n{output}"
    );
}

/// Transitive system.file() resolution: a closed child reached via
/// source(system.file(...)) itself contains another source(system.file(...)),
/// and both hops resolve correctly so the grandchild's definitions are in scope.
#[test]
fn system_file_transitive_nested_resolution() {
    let workspace = TempDir::new().unwrap();
    let libdir = TempDir::new().unwrap();

    // Create installed "otherpkg" with two files:
    //   otherpkg/entry.R -> source(system.file("impl.R", package = "otherpkg"))
    //   otherpkg/impl.R  -> defines `deep_fn`
    let pkg_dir = libdir.path().join("otherpkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("entry.R"),
        "source(system.file(\"impl.R\", package = \"otherpkg\"))\nentry_fn <- function() deep_fn()\n",
    )
    .unwrap();
    std::fs::write(pkg_dir.join("impl.R"), "deep_fn <- function() 42\n").unwrap();

    // Workspace DESCRIPTION
    std::fs::write(
        workspace.path().join("DESCRIPTION"),
        "Package: mypkg\nTitle: Test\nVersion: 0.1.0\n",
    )
    .unwrap();

    // raven.toml with additionalLibraryPaths
    std::fs::write(
        workspace.path().join("raven.toml"),
        format!(
            "[packages]\nadditionalLibraryPaths = [\"{}\"]\n",
            toml_escape(libdir.path())
        ),
    )
    .unwrap();

    // R/main.R sources entry.R which itself sources impl.R
    std::fs::create_dir_all(workspace.path().join("R")).unwrap();
    std::fs::write(
        workspace.path().join("R").join("main.R"),
        "source(system.file(\"entry.R\", package = \"otherpkg\"))\nresult <- entry_fn()\n",
    )
    .unwrap();

    let output = run_check(workspace.path());
    assert!(
        !output.contains("Cannot resolve path"),
        "Transitive system.file() should resolve both hops. Output:\n{output}"
    );
    assert!(
        !output.contains("entry_fn is not defined")
            && !output.contains("Undefined function: entry_fn"),
        "entry_fn should be in scope via transitive system.file(). Output:\n{output}"
    );
}

/// `collect_diagnostics_blocking` (the test helper in check.rs) resolves
/// workspace system.file() edges, matching `run()`.
#[test]
fn system_file_collect_diagnostics_blocking_resolves_installed_package() {
    let workspace = TempDir::new().unwrap();
    let libdir = TempDir::new().unwrap();

    // Installed package with a helper
    let pkg_dir = libdir.path().join("instpkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("helper.R"),
        "inst_helper <- function(x) x + 1\n",
    )
    .unwrap();

    // Workspace
    std::fs::write(
        workspace.path().join("DESCRIPTION"),
        "Package: mypkg\nTitle: Test\nVersion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("raven.toml"),
        format!(
            "[packages]\nadditionalLibraryPaths = [\"{}\"]\n",
            toml_escape(libdir.path())
        ),
    )
    .unwrap();

    std::fs::create_dir_all(workspace.path().join("R")).unwrap();
    std::fs::write(
        workspace.path().join("R").join("main.R"),
        "source(system.file(\"helper.R\", package = \"instpkg\"))\nresult <- inst_helper(1)\n",
    )
    .unwrap();

    // Use raven check (which exercises both run() and, implicitly, the same
    // resolve path as collect_diagnostics_blocking after our fix).
    let output = run_check(workspace.path());
    assert!(
        !output.contains("inst_helper is not defined")
            && !output.contains("Undefined function: inst_helper"),
        "inst_helper should be resolved via system.file() in raven check. Output:\n{output}"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle recoverability: `WorldState`-level tests exercising
// the same detached two-attempt transaction as the `LibpathEvent::Changed`
// consumer and DESCRIPTION manifest branch after a package install/removal or
// `Package:` rename. No file edits happen between resolution passes — recovery
// must come from re-resolution alone.
// ---------------------------------------------------------------------------

mod lifecycle {
    use std::sync::Arc;

    use raven::backend::run_system_file_convergence_for_test;
    use raven::cross_file::FileSnapshot;
    use raven::cross_file::source_detect::SystemFileCall;
    use raven::cross_file::types::{CrossFileMetadata, ForwardSource};
    use raven::package_library::PackageLibrary;
    use raven::state::WorldState;
    use raven::workspace_index::IndexEntry;
    use tower_lsp::lsp_types::Url;

    /// Index entry whose only source is `source(system.file(<part>, package = <pkg>))`.
    fn system_file_entry(part: &str, pkg: &str) -> IndexEntry {
        let metadata = CrossFileMetadata {
            sources: vec![ForwardSource {
                system_file: Some(SystemFileCall {
                    parts: vec![part.to_string()],
                    package: pkg.to_string(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        IndexEntry {
            contents: ropey::Rope::from_str(&format!(
                "source(system.file(\"{part}\", package = \"{pkg}\"))\n"
            )),
            tree: None,
            loaded_packages: Vec::new(),
            data_packages: vec![],
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: Some(1),
            },
            metadata: Arc::new(metadata),
            artifacts: Arc::new(raven::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 1,
        }
    }

    fn state_with_lib(libdir: &std::path::Path) -> WorldState {
        let mut state = WorldState::new();
        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![libdir.to_path_buf()]);
        state.package_library = Arc::new(lib);
        state
    }

    async fn converge(
        state: &mut WorldState,
        only_packages: Option<std::collections::HashSet<String>>,
    ) -> Vec<Url> {
        run_system_file_convergence_for_test(state, only_packages)
            .await
            .expect("the exact system.file owner should commit within two attempts")
    }

    /// Package installed AFTER startup: the first resolution pass runs with
    /// non-empty lib_paths and fails (package not installed). The
    /// `LibpathEvent::Changed` consumer then re-runs resolution — the edge must
    /// form without any edit to the sourcing file.
    #[tokio::test]
    async fn edge_forms_after_package_install_without_edit() {
        let libdir = tempfile::tempdir().unwrap();
        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();

        let mut state = state_with_lib(libdir.path());
        state
            .workspace_index
            .insert(uri.clone(), system_file_entry("helper.R", "otherpkg"));

        // Startup pass: lib_paths non-empty, otherpkg not installed.
        converge(&mut state, None).await;

        // The source entry must survive the failed attempt — dropping it makes
        // the staleness unrecoverable.
        let entry = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed");
        assert_eq!(
            entry.metadata.sources.len(),
            1,
            "unresolved system.file source must be retained for later re-resolution"
        );

        // "Install" the package (what LibpathEvent::Changed reports), then
        // re-run resolution exactly as the Changed consumer does.
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();
        converge(&mut state, None).await;

        let entry = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed");
        let resolved_uri = entry.metadata.sources[0]
            .resolved_uri
            .clone()
            .expect("source must resolve after the package install event");
        let resolved_path = resolved_uri.to_file_path().unwrap();
        assert!(
            resolved_path.ends_with("otherpkg/helper.R"),
            "must resolve into the installed package, got {resolved_path:?}"
        );
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&uri)
                .iter()
                .any(|e| e.to == resolved_uri),
            "dependency edge must form after the install event"
        );
    }

    /// Package REMOVED after a successful resolution: the stale resolved_uri
    /// and dependency edge must be cleared by the next resolution pass, and a
    /// reinstall must bring them back.
    #[tokio::test]
    async fn resolution_clears_after_package_removal_and_recovers_on_reinstall() {
        let libdir = tempfile::tempdir().unwrap();
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();

        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        let mut state = state_with_lib(libdir.path());
        state
            .workspace_index
            .insert(uri.clone(), system_file_entry("helper.R", "otherpkg"));

        converge(&mut state, None).await;
        let entry = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed");
        let resolved_uri = entry.metadata.sources[0]
            .resolved_uri
            .clone()
            .expect("source must resolve while the package is installed");

        // Remove the package, then re-run resolution as the Changed consumer does.
        std::fs::remove_dir_all(&pkg_dir).unwrap();
        converge(&mut state, None).await;

        let entry = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed");
        assert_eq!(
            entry.metadata.sources.len(),
            1,
            "source entry must be retained after the package removal"
        );
        assert!(
            entry.metadata.sources[0].resolved_uri.is_none(),
            "stale resolved_uri must be cleared after the package removal"
        );
        assert!(
            !state
                .cross_file_graph
                .get_dependencies(&uri)
                .iter()
                .any(|e| e.to == resolved_uri),
            "stale dependency edge must be removed after the package removal"
        );

        // Reinstall: resolution must recover.
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();
        converge(&mut state, None).await;

        let entry = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed");
        assert!(
            entry.metadata.sources[0].resolved_uri.is_some(),
            "source must re-resolve after the package is reinstalled"
        );
    }

    /// Index entry whose only source carries an already-resolved
    /// cross-package target (`resolved_uri` set), as left behind by a prior
    /// successful branch-2 resolution.
    fn resolved_entry(pkg: &str, target: &Url) -> IndexEntry {
        let metadata = CrossFileMetadata {
            sources: vec![ForwardSource {
                system_file: Some(SystemFileCall {
                    parts: vec!["helper.R".to_string()],
                    package: pkg.to_string(),
                }),
                resolved_uri: Some(target.clone()),
                ..Default::default()
            }],
            ..Default::default()
        };
        IndexEntry {
            contents: ropey::Rope::from_str(&format!(
                "source(system.file(\"helper.R\", package = \"{pkg}\"))\n"
            )),
            tree: None,
            loaded_packages: Vec::new(),
            data_packages: vec![],
            snapshot: FileSnapshot {
                mtime: std::time::SystemTime::UNIX_EPOCH,
                size: 1,
                content_hash: Some(1),
            },
            metadata: Arc::new(metadata),
            artifacts: Arc::new(raven::cross_file::scope::ScopeArtifacts::default()),
            indexed_at_version: 1,
        }
    }

    /// A successful cross-package resolution indexes the outside-workspace
    /// target so its artifacts reach scope resolution. When the package is
    /// removed and the resolution clears, nothing references that external
    /// entry anymore — it must be dropped from the index rather than linger
    /// in an LRU slot until natural eviction (#425).
    #[tokio::test]
    async fn orphaned_external_entry_dropped_after_package_removal() {
        let libdir = tempfile::tempdir().unwrap();
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();

        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        let mut state = state_with_lib(libdir.path());
        state
            .workspace_index
            .insert(uri.clone(), system_file_entry("helper.R", "otherpkg"));

        converge(&mut state, None).await;
        let target = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed")
            .metadata
            .sources[0]
            .resolved_uri
            .clone()
            .expect("source must resolve while the package is installed");
        assert!(
            state.workspace_index.contains(&target),
            "precondition: the cross-package target is indexed after resolution"
        );

        std::fs::remove_dir_all(&pkg_dir).unwrap();
        converge(&mut state, None).await;

        assert!(
            !state.workspace_index.contains(&target),
            "orphaned external index entry must be dropped once no resolution references it"
        );
    }

    /// A library swap re-targets the resolution to a different installed
    /// copy: the new target must be indexed and the previous one dropped —
    /// nothing points at it anymore (#425).
    #[tokio::test]
    async fn orphaned_external_entry_dropped_after_libpath_retarget() {
        let libdir_a = tempfile::tempdir().unwrap();
        let libdir_b = tempfile::tempdir().unwrap();
        for lib in [&libdir_a, &libdir_b] {
            let pkg_dir = lib.path().join("otherpkg");
            std::fs::create_dir_all(&pkg_dir).unwrap();
            std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();
        }

        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        let mut state = state_with_lib(libdir_a.path());
        state
            .workspace_index
            .insert(uri.clone(), system_file_entry("helper.R", "otherpkg"));

        converge(&mut state, None).await;
        let old_target = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed")
            .metadata
            .sources[0]
            .resolved_uri
            .clone()
            .expect("source must resolve against libdir_a");
        assert!(
            state.workspace_index.contains(&old_target),
            "precondition: libdir_a target indexed"
        );

        // Library swap: libdir_b now wins resolution.
        let mut lib = PackageLibrary::new_empty();
        lib.set_lib_paths(vec![libdir_b.path().to_path_buf()]);
        state.package_library = Arc::new(lib);
        converge(&mut state, None).await;

        let new_target = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed")
            .metadata
            .sources[0]
            .resolved_uri
            .clone()
            .expect("source must re-resolve against libdir_b");
        assert_ne!(
            old_target, new_target,
            "precondition: resolution re-targeted"
        );
        assert!(
            state.workspace_index.contains(&new_target),
            "the re-targeted external file must be indexed"
        );
        assert!(
            !state.workspace_index.contains(&old_target),
            "the previous external target must be dropped after the re-target"
        );
    }

    /// The external entry must be RETAINED while any other resolution still
    /// points at it. Here a second file's resolution (whose package the
    /// filtered event pass does not touch) references the same target as the
    /// one being cleared.
    #[tokio::test]
    async fn external_entry_retained_while_another_resolution_references_it() {
        let libdir = tempfile::tempdir().unwrap();
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();

        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        let mut state = state_with_lib(libdir.path());
        state
            .workspace_index
            .insert(uri.clone(), system_file_entry("helper.R", "otherpkg"));

        converge(&mut state, None).await;
        let target = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed")
            .metadata
            .sources[0]
            .resolved_uri
            .clone()
            .expect("source must resolve while the package is installed");
        assert!(
            state.workspace_index.contains(&target),
            "precondition: target indexed"
        );

        // A second file's resolution still points at the same target; its
        // package is untouched by the filtered event below.
        let keeper_uri = Url::parse("file:///workspace/keeper.R").unwrap();
        state
            .workspace_index
            .insert(keeper_uri, resolved_entry("keeperpkg", &target));

        // otherpkg removed; the libpath-event consumer re-resolves only
        // entries referencing {otherpkg}.
        std::fs::remove_dir_all(&pkg_dir).unwrap();
        let pkgs: std::collections::HashSet<String> =
            std::iter::once("otherpkg".to_string()).collect();
        converge(&mut state, Some(pkgs.clone())).await;

        assert!(
            state
                .workspace_index
                .get(&uri)
                .expect("entry still indexed")
                .metadata
                .sources[0]
                .resolved_uri
                .is_none(),
            "precondition: the cleared resolution actually cleared"
        );
        assert!(
            state.workspace_index.contains(&target),
            "external entry must be retained while another resolved_uri still references it"
        );
    }

    /// renv-style layout: the package library lives INSIDE a workspace
    /// folder, so the resolved target is a workspace file owned by the
    /// workspace scan. The orphan cleanup must never drop entries under a
    /// workspace folder.
    #[tokio::test]
    async fn workspace_entry_not_dropped_by_orphan_cleanup() {
        let workspace = tempfile::tempdir().unwrap();
        // Canonicalize so the resolved target and the folder agree on
        // symlinked temp paths (macOS /var -> /private/var).
        let ws_root = workspace.path().canonicalize().unwrap();
        let libdir = ws_root.join("renv").join("library");
        let pkg_dir = libdir.join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();

        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        let mut state = state_with_lib(&libdir);
        state
            .workspace_folders
            .push(Url::from_directory_path(&ws_root).unwrap());
        state
            .workspace_index
            .insert(uri.clone(), system_file_entry("helper.R", "otherpkg"));

        converge(&mut state, None).await;
        let target = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed")
            .metadata
            .sources[0]
            .resolved_uri
            .clone()
            .expect("source must resolve while the package is installed");
        assert!(
            state.workspace_index.contains(&target),
            "precondition: target indexed"
        );

        std::fs::remove_dir_all(&pkg_dir).unwrap();
        converge(&mut state, None).await;

        assert!(
            state.workspace_index.contains(&target),
            "entries under a workspace folder are owned by the workspace scan \
             and must not be dropped by the orphan cleanup"
        );
    }

    /// Open buffers are authoritative: their metadata lives in the document
    /// store, not the workspace index, so the resolution pass must cover it
    /// too. An open buffer (here not even present in the workspace index —
    /// e.g. an unsaved buffer, or `index_workspace = false`) whose
    /// `system.file()` source was unresolved must re-resolve after the
    /// install event, report its URI for republish, and gain a graph edge.
    #[tokio::test]
    async fn open_buffer_metadata_re_resolves_after_package_install() {
        let libdir = tempfile::tempdir().unwrap();
        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        let text = "source(system.file(\"helper.R\", package = \"otherpkg\"))\n";

        let mut state = state_with_lib(libdir.path());
        state.open_document_with_language_id(uri.clone(), text, Some(1), Some("r"));

        // Startup pass: otherpkg not installed → stays unresolved.
        converge(&mut state, None).await;

        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();
        let changed = converge(&mut state, None).await;

        let metadata = state
            .get_enriched_metadata(&uri)
            .expect("document still open");
        let resolved_uri = metadata.sources[0]
            .resolved_uri
            .clone()
            .expect("open-buffer metadata must re-resolve after the install event");
        assert!(
            resolved_uri
                .to_file_path()
                .unwrap()
                .ends_with("otherpkg/helper.R"),
            "must resolve into the installed package"
        );
        assert!(
            changed.contains(&uri),
            "the open buffer must be reported as changed for republish, got {changed:?}"
        );
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&uri)
                .iter()
                .any(|e| e.to == resolved_uri),
            "dependency edge must form from the open buffer's metadata"
        );
    }

    /// A file present in BOTH stores: the index pass may rebuild the graph
    /// from stale index metadata (scanned before the package was installed,
    /// source call at a different line than the edited buffer). Even when the
    /// open buffer's own resolution is unchanged, the open-document pass must
    /// re-assert buffer-derived graph edges — open documents are
    /// authoritative.
    #[tokio::test]
    async fn open_buffer_graph_edges_win_over_index_rebuild_when_unchanged() {
        let libdir = tempfile::tempdir().unwrap();
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();

        let uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        // Buffer has a comment line first → source call at line 1; the
        // (stale) index entry has it at line 0.
        let buffer_text = "# edited\nsource(system.file(\"helper.R\", package = \"otherpkg\"))\n";

        let mut state = state_with_lib(libdir.path());
        state.open_document_with_language_id(uri.clone(), buffer_text, Some(1), Some("r"));

        // First pass resolves the open buffer (did_open-time enrichment):
        // edge from buffer metadata, call site at line 1.
        converge(&mut state, None).await;
        assert!(
            state
                .cross_file_graph
                .get_dependencies(&uri)
                .iter()
                .any(|e| e.call_site_line == Some(1)),
            "precondition: buffer-derived edge at line 1"
        );

        // A stale scan result lands in the index: unresolved, source at line 0.
        state
            .workspace_index
            .insert(uri.clone(), system_file_entry("helper.R", "otherpkg"));

        // Event pass: the index entry resolves (edge rebuild from index
        // metadata, line 0); the buffer's resolution is unchanged, but its
        // edges must still win.
        converge(&mut state, None).await;
        let deps = state.cross_file_graph.get_dependencies(&uri);
        assert!(
            deps.iter().any(|e| e.call_site_line == Some(1)),
            "graph must reflect the authoritative open buffer (call site line 1), got {:?}",
            deps.iter().map(|e| e.call_site_line).collect::<Vec<_>>()
        );
        assert!(
            !deps.iter().any(|e| e.call_site_line == Some(0)),
            "stale index-derived edge (line 0) must not survive the open-document pass"
        );
    }

    /// Open transitive dependents of a file whose system.file resolution
    /// changed must be in the republish set: file_b sources file_a, file_a's
    /// system.file edge forms after a package install — file_b's cross-file
    /// scope now includes the helper's definitions, so its diagnostics are
    /// stale unless it is republished too.
    #[tokio::test]
    async fn open_dependents_of_changed_files_are_in_republish_set() {
        let libdir = tempfile::tempdir().unwrap();
        let child_uri = Url::parse("file:///workspace/uses_helper.R").unwrap();
        let parent_uri = Url::parse("file:///workspace/parent.R").unwrap();

        let mut state = state_with_lib(libdir.path());
        state
            .workspace_index
            .insert(child_uri.clone(), system_file_entry("helper.R", "otherpkg"));

        // parent.R sources uses_helper.R (ordinary path source).
        let parent_meta = CrossFileMetadata {
            sources: vec![ForwardSource {
                path: "uses_helper.R".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        state
            .cross_file_graph
            .update_file(&parent_uri, &parent_meta, None, |_| None);

        // Both files are open documents.
        state.open_document_with_language_id(
            parent_uri.clone(),
            "source(\"uses_helper.R\")\n",
            None,
            Some("r"),
        );
        state.open_document_with_language_id(
            child_uri.clone(),
            "source(system.file(\"helper.R\", package = \"otherpkg\"))\n",
            None,
            Some("r"),
        );

        // Startup pass fails (otherpkg not installed), then the install event.
        converge(&mut state, None).await;
        let pkg_dir = libdir.path().join("otherpkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("helper.R"), "helper_fn <- function() 42\n").unwrap();
        let changed = converge(&mut state, None).await;
        assert_eq!(
            changed,
            vec![child_uri.clone()],
            "only the file with the system.file source changes resolution"
        );

        let republish = state.system_file_republish_set(&changed);
        assert!(
            republish.contains(&child_uri),
            "the changed file itself must be republished, got {republish:?}"
        );
        assert!(
            republish.contains(&parent_uri),
            "an open dependent sourcing the changed file must be republished, got {republish:?}"
        );
    }

    /// The package-filtered resolution variant (used by the libpath-event
    /// consumer) must only re-probe entries referencing the changed packages,
    /// leaving unrelated resolved entries untouched.
    #[tokio::test]
    async fn filtered_resolution_skips_unrelated_packages() {
        use std::collections::HashSet;

        let libdir = tempfile::tempdir().unwrap();
        for pkg in ["pkga", "pkgb"] {
            let dir = libdir.path().join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("helper.R"), "f <- function() 1\n").unwrap();
        }

        let uri_a = Url::parse("file:///workspace/a.R").unwrap();
        let uri_b = Url::parse("file:///workspace/b.R").unwrap();
        let mut state = state_with_lib(libdir.path());
        state
            .workspace_index
            .insert(uri_a.clone(), system_file_entry("helper.R", "pkga"));
        state
            .workspace_index
            .insert(uri_b.clone(), system_file_entry("helper.R", "pkgb"));

        converge(&mut state, None).await;
        for uri in [&uri_a, &uri_b] {
            assert!(
                state.workspace_index.get(uri).unwrap().metadata.sources[0]
                    .resolved_uri
                    .is_some(),
                "both packages installed → both entries resolve"
            );
        }

        // Both packages disappear from disk, but the event names only pkgb.
        std::fs::remove_dir_all(libdir.path().join("pkga")).unwrap();
        std::fs::remove_dir_all(libdir.path().join("pkgb")).unwrap();
        let only_b: HashSet<String> = std::iter::once("pkgb".to_string()).collect();
        let changed = converge(&mut state, Some(only_b.clone())).await;

        assert_eq!(
            changed,
            vec![uri_b.clone()],
            "only the entry referencing the filtered package may change"
        );
        assert!(
            state.workspace_index.get(&uri_a).unwrap().metadata.sources[0]
                .resolved_uri
                .is_some(),
            "entry for a package outside the filter must not be re-probed"
        );
        assert!(
            state.workspace_index.get(&uri_b).unwrap().metadata.sources[0]
                .resolved_uri
                .is_none(),
            "entry for the filtered package must be re-resolved (cleared)"
        );

        // A later event naming pkga clears the remaining stale entry.
        let only_a: HashSet<String> = std::iter::once("pkga".to_string()).collect();
        let changed = converge(&mut state, Some(only_a.clone())).await;
        assert_eq!(changed, vec![uri_a.clone()]);
        assert!(
            state.workspace_index.get(&uri_a).unwrap().metadata.sources[0]
                .resolved_uri
                .is_none()
        );
    }

    /// Workspace `Package:` rename: a self-package `system.file()` reference
    /// that could not resolve under the old name (and ran with non-empty
    /// lib_paths, the previously-dropped case) must resolve via branch 1 once
    /// DESCRIPTION names the matching package — the call the manifest-change
    /// branch makes after `apply_package_event`.
    #[tokio::test]
    async fn branch1_resolution_recovers_after_package_rename() {
        use raven::package_state::{DescriptionInput, PackageInputDelta};

        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("inst")).unwrap();
        std::fs::write(
            workspace.path().join("inst").join("helper.R"),
            "helper_fn <- function() 42\n",
        )
        .unwrap();

        // Non-empty lib_paths so the failed cross-package attempt is the
        // "attempted, not deferred" case.
        let libdir = tempfile::tempdir().unwrap();
        let mut state = state_with_lib(libdir.path());
        state.package_inputs.workspace_root = Some(workspace.path().to_path_buf());
        state.package_inputs.description = Some(DescriptionInput {
            text: Arc::from("Package: oldpkg\nTitle: T\nVersion: 0.1.0\n"),
        });
        state.apply_package_event(&PackageInputDelta::DescriptionChanged);

        let uri = Url::from_file_path(workspace.path().join("R").join("main.R")).unwrap();
        state
            .workspace_index
            .insert(uri.clone(), system_file_entry("helper.R", "newpkg"));

        // Under "Package: oldpkg" the reference to "newpkg" cannot resolve.
        converge(&mut state, None).await;
        let entry = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed");
        assert_eq!(
            entry.metadata.sources.len(),
            1,
            "unresolved system.file source must be retained across the rename"
        );

        // Rename the workspace package, then re-resolve as the manifest branch does.
        state.package_inputs.description = Some(DescriptionInput {
            text: Arc::from("Package: newpkg\nTitle: T\nVersion: 0.1.0\n"),
        });
        state.apply_package_event(&PackageInputDelta::DescriptionChanged);
        converge(&mut state, None).await;

        let entry = state
            .workspace_index
            .get(&uri)
            .expect("entry still indexed");
        assert_eq!(
            entry.metadata.sources[0].path, "/inst/helper.R",
            "self-package system.file must resolve via branch 1 after the rename"
        );
    }
}
