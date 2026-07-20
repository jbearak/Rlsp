//! End-to-end regressions for exact optional `source()` guards.
//!
//! The fixture is entirely synthetic. It verifies both halves of the contract:
//! an absent guarded file is not a path error, while an existing guarded file
//! remains a real dependency whose definitions are visible after the call.

use std::process::Command;

fn raven_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
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
    let code = output.status.code();
    assert!(
        matches!(code, Some(0) | Some(1)),
        "raven check did not complete cleanly (exit {code:?}).\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn absent_exact_file_exists_guard_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.R"),
        "if (file.exists(\"scripts/config.R\")) {\n\
             source(\"scripts/config.R\")\n\
         } else {\n\
             message(\"Using built-in defaults\")\n\
         }\n",
    )
    .unwrap();

    let output = run_check(dir.path());
    assert!(
        !output.contains("unresolved-source-path")
            && !output.contains("File not found")
            && !output.contains("Cannot resolve path"),
        "an absent optional source must not fail raven check:\n{output}"
    );
}

#[test]
fn existing_exact_file_exists_guard_keeps_dependency_scope() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("scripts")).unwrap();
    std::fs::write(
        dir.path().join("scripts/config.R"),
        "optional_helper <- function() 42\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.R"),
        "if (file.exists(\"scripts/config.R\")) source(\"scripts/config.R\")\n\
         result <- optional_helper()\n",
    )
    .unwrap();

    let output = run_check(dir.path());
    assert!(
        !output.contains("optional_helper is not defined"),
        "the guard is diagnostic-only; an existing source must still lend scope:\n{output}"
    );
}
