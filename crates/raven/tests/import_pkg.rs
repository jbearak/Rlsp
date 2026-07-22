//! End-to-end `raven check` coverage for static `{import}` calls.

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
    assert!(
        matches!(output.status.code(), Some(0) | Some(1)),
        "raven check failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn static_script_imports_are_selective_and_nested_here_imports_stay_private() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("base.R"), "inner <- function() 1\n").unwrap();
    std::fs::write(
        dir.path().join("mod.R"),
        "import::from('base.R', inner)\n\
         .dotted <- 2\n\
         .packageName <- 'synthetic'\n\
         outer <- function() inner()\n\
         private <- 3\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.R"),
        "import::here('mod.R', outer, nested = inner, local_dot = .dotted, imported_package_name = .packageName)\n\
         value <- outer() + nested() + local_dot\n\
         synthetic <- imported_package_name\n\
         leaked <- private\n",
    )
    .unwrap();

    let output = run_check(dir.path());
    assert!(!output.contains("outer is not defined"), "{output}");
    assert!(!output.contains("nested is not defined"), "{output}");
    assert!(!output.contains("local_dot is not defined"), "{output}");
    assert!(
        output.contains("imported_package_name is not defined"),
        "{output}"
    );
    assert!(output.contains("private is not defined"), "{output}");
}

#[test]
fn named_destination_is_a_fallback_and_missing_script_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mod.R"),
        "same <- function() 1\nfrom_module <- function() 2\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.R"),
        "same <- function() 3\n\
         import::from('mod.R', same, from_module, .into = 'tools')\n\
         value <- same() + from_module()\n\
         import::here('missing.R', nope)\n",
    )
    .unwrap();

    let output = run_check(dir.path());
    assert!(!output.contains("same is not defined"), "{output}");
    assert!(!output.contains("from_module is not defined"), "{output}");
    assert!(output.contains("import-module-not-found"), "{output}");
}

#[test]
fn case_only_script_path_is_diagnosed_and_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Helpers.R"), "helper <- function() 1\n").unwrap();
    std::fs::write(
        dir.path().join("main.R"),
        "import::here('helpers.R', helper)\nvalue <- helper()\n",
    )
    .unwrap();

    let output = run_check(dir.path());
    assert!(output.contains("import-module-case-mismatch"), "{output}");
    assert!(output.contains("helper is not defined"), "{output}");
}
