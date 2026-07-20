//! Regression tests for issue #638: computed `source()` paths in testthat
//! helper files (non-package layout with project code under `scripts/`).
//!
//! These exercise the `raven check` CLI on temporary workspaces reproducing
//! the issue exactly: a `tests/testthat/helper-*.R` that computes the repo
//! root from testthat's working directory and sources project files through
//! it. Symbols those files define must be visible in peer `test-*.R` files.
//!
//! Run with: `cargo test -p raven --test testthat_computed_source_regression`

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
    // Exit-code guard: anything outside {0, 1} (or a signal kill) means the
    // analysis did not run to completion, and negative `!contains` assertions
    // would pass vacuously.
    let code = output.status.code();
    assert!(
        matches!(code, Some(0) | Some(1)),
        "raven check did not complete cleanly (exit {code:?}).\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Write the issue #638 repro: DESCRIPTION (package mode on), project code
/// under scripts/, a helper computing the repo root via
/// `normalizePath(file.path("..", ".."))`, and a test using the sourced defs.
fn write_repro(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::create_dir_all(root.join("tests/testthat")).unwrap();
    std::fs::write(
        root.join("DESCRIPTION"),
        "Package: repro638\nVersion: 0.0.1\nSuggests: testthat\n",
    )
    .unwrap();
    std::fs::write(
        root.join("scripts/helpers.R"),
        "my_theme <- function() 1\nmake_result <- function() 2\n",
    )
    .unwrap();
    std::fs::write(
        root.join("tests/testthat/helper-project.R"),
        "repo_root <- normalizePath(file.path(\"..\", \"..\"))\n\
         source(file.path(repo_root, \"scripts/helpers.R\"))\n",
    )
    .unwrap();
    // Bare-variable references so the assertion doesn't depend on how
    // unknown *calls* are treated when no R installation is available.
    std::fs::write(
        root.join("tests/testthat/test-contract.R"),
        "testthat::test_that(\"uses project helpers\", {\n\
             p <- my_theme\n\
             r <- make_result\n\
         })\n",
    )
    .unwrap();
}

/// The exact issue #638 repro must produce no undefined-variable diagnostics
/// for symbols defined in the helper's computed-path source() target.
#[test]
fn computed_helper_source_suppresses_undefined_in_test_files() {
    let dir = TempDir::new().unwrap();
    write_repro(dir.path());
    let stdout = run_check(dir.path());
    assert!(
        !stdout.contains("my_theme is not defined"),
        "my_theme must resolve via the helper's computed source() (issue #638):\n{stdout}"
    );
    assert!(
        !stdout.contains("make_result is not defined"),
        "make_result must resolve via the helper's computed source() (issue #638):\n{stdout}"
    );
    // The helper file itself must also see the sourced defs (dependency-graph
    // path, not the preamble injection).
    assert!(
        !stdout.contains("helper-project.R"),
        "no diagnostics expected in the helper file:\n{stdout}"
    );
}

/// Guard against over-suppression: a genuinely undefined symbol in a test
/// file is still flagged in the same workspace.
#[test]
fn genuine_undefined_still_flagged_alongside_computed_helper_source() {
    let dir = TempDir::new().unwrap();
    write_repro(dir.path());
    std::fs::write(
        dir.path().join("tests/testthat/test-genuine.R"),
        "testthat::test_that(\"x\", {\n    q <- genuinely_undefined_xyz\n})\n",
    )
    .unwrap();
    let stdout = run_check(dir.path());
    assert!(
        stdout.contains("genuinely_undefined_xyz is not defined"),
        "genuine undefined variables must still be flagged:\n{stdout}"
    );
    assert!(
        !stdout.contains("my_theme is not defined"),
        "sourced defs stay resolved:\n{stdout}"
    );
}

/// Reading a file in a later helper must not retroactively erase the earlier
/// top-level path binding used to discover the synthetic source closure.
#[test]
fn unrelated_function_effect_does_not_drop_computed_source() {
    let dir = TempDir::new().unwrap();
    write_repro(dir.path());
    std::fs::write(
        dir.path().join("tests/testthat/helper-project.R"),
        "repo_root <- normalizePath(file.path(\"..\", \"..\"))\n\
         source(file.path(repo_root, \"scripts/helpers.R\"))\n\
         inspect_file <- function(path) {\n\
             lines <- readLines(path, warn = FALSE)\n\
             length(lines)\n\
         }\n",
    )
    .unwrap();
    let stdout = run_check(dir.path());
    assert!(
        !stdout.contains("my_theme is not defined"),
        "an unrelated function-local effect must not drop the source edge:\n{stdout}"
    );
    assert!(
        !stdout.contains("make_result is not defined"),
        "all sourced definitions must remain visible:\n{stdout}"
    );
}

/// An unfoldable computed path (paste0) must NOT create the edge — the
/// sourced defs stay flagged, proving folding is strict rather than guessy.
#[test]
fn unfoldable_computed_path_still_flags() {
    let dir = TempDir::new().unwrap();
    write_repro(dir.path());
    std::fs::write(
        dir.path().join("tests/testthat/helper-project.R"),
        "repo_root <- normalizePath(paste0(\"..\", \"/\", \"..\"))\n\
         source(paste0(repo_root, \"/scripts/helpers.R\"))\n",
    )
    .unwrap();
    let stdout = run_check(dir.path());
    assert!(
        stdout.contains("my_theme is not defined"),
        "paste0 paths are not foldable; defs must stay unresolved:\n{stdout}"
    );
}
