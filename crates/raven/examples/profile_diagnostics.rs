// Profile diagnostic phases on representative cross-file topologies.
//
// Run with:
//   cargo run --release --example profile_diagnostics --features test-support
//
// Prints phase-by-phase timing for snapshot build vs diagnostic computation.

use std::path::{Path, PathBuf};

use raven::handlers::{DiagCancelToken, diagnostics_via_snapshot_profile};
use raven::state::{WorldState, scan_workspace};
use tempfile::TempDir;
use url::Url;

fn write(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
}

fn body_for(file_idx: usize) -> String {
    let mut s = String::new();
    s.push_str(&format!("func_{0} <- function(x, y = {0}) {{\n", file_idx));
    s.push_str("    result <- x + y\n");
    s.push_str(&format!("    helper_{0}(result)\n", file_idx));
    s.push_str("    if (is.na(result)) return(NULL)\n");
    s.push_str("    result\n");
    s.push_str("}\n\n");
    s.push_str(&format!("helper_{0} <- function(z) z + 1\n", file_idx));
    for i in 0..10 {
        s.push_str(&format!("var_{0}_{1} <- {1}\n", file_idx, i));
    }
    s
}

fn write_linear_chain(dir: &Path, depth: usize) {
    for i in 0..depth {
        let mut content = String::new();
        content.push_str("library(stats)\n\n");
        if i + 1 < depth {
            content.push_str(&format!("source(\"file_{}.R\")\n\n", i + 1));
        }
        content.push_str(&body_for(i));
        write(&dir.join(format!("file_{i}.R")), &content);
    }
}

fn write_fanout(dir: &Path, leaves: usize) {
    let mut hub = String::from("library(stats)\n\n");
    for i in 0..5 {
        hub.push_str(&format!("util_{0} <- function(x) x + {0}\n", i));
    }
    hub.push('\n');
    hub.push_str(&body_for(999));
    write(&dir.join("utility.R"), &hub);
    for i in 0..leaves {
        let mut leaf = String::from("library(dplyr)\n\nsource(\"utility.R\")\n\n");
        leaf.push_str(&body_for(i));
        leaf.push_str(&format!("call_util_{0} <- util_{1}(1)\n", i, i % 5));
        write(&dir.join(format!("leaf_{i}.R")), &leaf);
    }
}

fn write_mixed(dir: &Path, leaves: usize, chain_depth: usize) {
    let mut hub = String::from("library(stats)\n\n");
    for i in 0..5 {
        hub.push_str(&format!("util_{0} <- function(x) x + {0}\n", i));
    }
    write(&dir.join("utility.R"), &hub);
    for i in 0..leaves {
        let mut leaf = String::from("source(\"utility.R\")\n");
        if chain_depth > 0 {
            leaf.push_str(&format!("source(\"chain_{i}_0.R\")\n\n"));
        } else {
            leaf.push('\n');
        }
        leaf.push_str(&body_for(i));
        write(&dir.join(format!("leaf_{i}.R")), &leaf);
        for c in 0..chain_depth {
            let mut content = String::new();
            content.push_str("library(stats)\n\n");
            if c + 1 < chain_depth {
                content.push_str(&format!("source(\"chain_{i}_{}.R\")\n\n", c + 1));
            }
            content.push_str(&body_for(c * 1000 + i));
            write(&dir.join(format!("chain_{i}_{c}.R")), &content);
        }
    }
}

fn build_state(workspace: &Path) -> WorldState {
    let mut state = WorldState::new();
    let folder_url = Url::from_file_path(workspace).unwrap();
    state.workspace_folders.push(folder_url.clone());
    let mut entries: Vec<PathBuf> = std::fs::read_dir(workspace)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "R").unwrap_or(false))
        .collect();
    entries.sort();
    for path in &entries {
        let content = std::fs::read_to_string(path).unwrap();
        let uri = Url::from_file_path(path).unwrap();
        state.open_document_with_language_id(uri.clone(), &content, Some(1), Some("r"));
    }
    let (index, cross_file_entries, new_index_entries) = scan_workspace(&[folder_url], 20);
    state.apply_workspace_index(index, cross_file_entries, new_index_entries);
    state
}

fn uri_of(workspace: &Path, name: &str) -> Url {
    Url::from_file_path(workspace.join(name)).unwrap()
}

fn measure(label: &str, state: &WorldState, uri: &Url) {
    let cancel = DiagCancelToken::never();
    // Warmup
    for _ in 0..3 {
        let _ = diagnostics_via_snapshot_profile(state, uri, &cancel);
    }
    let mut builds = Vec::new();
    let mut diags = Vec::new();
    let mut last_outcome: Option<usize> = None;
    for _ in 0..10 {
        let (b, d, outcome) = diagnostics_via_snapshot_profile(state, uri, &cancel);
        builds.push(b);
        diags.push(d);
        last_outcome = outcome;
    }
    builds.sort();
    diags.sort();
    let median_build = builds[builds.len() / 2];
    let median_diag = diags[diags.len() / 2];
    let total = median_build + median_diag;
    let count_str = match last_outcome {
        Some(n) => n.to_string(),
        None => "<short-circuited>".to_string(),
    };
    println!(
        "{:50} build={:>6.2}ms  diag={:>6.2}ms  total={:>6.2}ms  diags={}",
        label,
        median_build.as_secs_f64() * 1000.0,
        median_diag.as_secs_f64() * 1000.0,
        total.as_secs_f64() * 1000.0,
        count_str,
    );
}

fn main() {
    // Enable trace logging so DiagnosticsSnapshot::build prints phase timings.
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .try_init();

    println!("=== Profile: diagnostic snapshot phases ===\n");

    println!("Topology: linear_chain_15");
    let dir = TempDir::new().unwrap();
    write_linear_chain(dir.path(), 15);
    let state = build_state(dir.path());
    measure("  file_0.R (root)", &state, &uri_of(dir.path(), "file_0.R"));
    measure("  file_7.R (mid)", &state, &uri_of(dir.path(), "file_7.R"));
    measure(
        "  file_14.R (leaf)",
        &state,
        &uri_of(dir.path(), "file_14.R"),
    );

    println!("\nTopology: fanout_30");
    let dir = TempDir::new().unwrap();
    write_fanout(dir.path(), 30);
    let state = build_state(dir.path());
    measure(
        "  utility.R (hub)",
        &state,
        &uri_of(dir.path(), "utility.R"),
    );
    measure("  leaf_0.R", &state, &uri_of(dir.path(), "leaf_0.R"));
    measure("  leaf_15.R", &state, &uri_of(dir.path(), "leaf_15.R"));

    println!("\nTopology: mixed_20x5");
    let dir = TempDir::new().unwrap();
    write_mixed(dir.path(), 20, 5);
    let state = build_state(dir.path());
    measure(
        "  utility.R (hub)",
        &state,
        &uri_of(dir.path(), "utility.R"),
    );
    measure("  leaf_0.R", &state, &uri_of(dir.path(), "leaf_0.R"));
    measure("  chain_0_0.R", &state, &uri_of(dir.path(), "chain_0_0.R"));
    measure(
        "  chain_0_4.R (tail)",
        &state,
        &uri_of(dir.path(), "chain_0_4.R"),
    );

    println!("\nTopology: fanout_100");
    let dir = TempDir::new().unwrap();
    write_fanout(dir.path(), 100);
    let state = build_state(dir.path());
    measure(
        "  utility.R (hub)",
        &state,
        &uri_of(dir.path(), "utility.R"),
    );
    measure("  leaf_0.R", &state, &uri_of(dir.path(), "leaf_0.R"));
    measure("  leaf_99.R", &state, &uri_of(dir.path(), "leaf_99.R"));

    println!("\nTopology: mixed_50x10  (stress: 1 hub + 50 leaves * 10-deep chains = 551 files)");
    let dir = TempDir::new().unwrap();
    write_mixed(dir.path(), 50, 10);
    let state = build_state(dir.path());
    measure(
        "  utility.R (hub)",
        &state,
        &uri_of(dir.path(), "utility.R"),
    );
    measure("  leaf_0.R", &state, &uri_of(dir.path(), "leaf_0.R"));
    measure(
        "  chain_0_5.R (mid)",
        &state,
        &uri_of(dir.path(), "chain_0_5.R"),
    );
    measure(
        "  chain_0_9.R (tail)",
        &state,
        &uri_of(dir.path(), "chain_0_9.R"),
    );
}
