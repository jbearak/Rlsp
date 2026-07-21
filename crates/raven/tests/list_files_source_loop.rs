//! End-to-end regressions for bounded `list.files()` source loops.
//!
//! Every fixture is synthetic and self-contained. The tests pin both ordered
//! shared scope between members and post-loop visibility in the driver.

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
        "raven check failed unexpectedly:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn write_fixture(root: &std::path::Path, driver_prefix: &str) {
    std::fs::create_dir(root.join("functions")).unwrap();
    std::fs::write(root.join("functions/01_helpers.R"), "shared_value <- 41\n").unwrap();
    std::fs::write(
        root.join("functions/02_consumer.R"),
        "derived_value <- shared_value + 1\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.R"),
        format!(
            "{driver_prefix}files <- list.files(\"functions\", pattern = \"\\\\.R$\", \
             full.names = TRUE)\n\
             for (file in files) source(file)\n\
             result <- derived_value\n"
        ),
    )
    .unwrap();
}

#[test]
fn ordered_members_share_scope_and_lend_it_after_the_loop() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "");

    let output = run_check(dir.path());
    assert!(
        !output.contains("shared_value is not defined")
            && !output.contains("derived_value is not defined"),
        "ordered members and the driver must share source scope:\n{output}"
    );
}

#[test]
fn batch_scope_starts_at_the_source_loop_not_before_it() {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(dir.path(), "before <- derived_value\n");

    let output = run_check(dir.path());
    assert_eq!(
        output.matches("derived_value is not defined").count(),
        1,
        "only the pre-loop use should remain undefined:\n{output}"
    );
}
