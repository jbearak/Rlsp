// edit_to_publish.rs - End-to-end diagnostic update latency benchmark
//
// Simulates the per-file work that happens between `did_change` and
// `publish_diagnostics` (snapshot build + diagnostic computation) under
// realistic cross-file topologies:
//
//   * `linear_chain_15` — linear `source()` chain of depth 15 (no fanout)
//   * `fanout_30`       — 30 leaf files all source one shared "utility" file
//   * `mixed`           — fanout hub of 20 leaves, each leaf sources a 5-deep chain
//
// For each topology we measure the cost of producing diagnostics for the
// edited file (`single_file`) and the cumulative cost of producing
// diagnostics for the hub + every dependent serially (`hub_and_all_dependents`),
// since that bounds the worst-case edit-to-publish latency when the runtime
// or the lock serializes the dependent revalidations.
//
// Run with: cargo bench --features test-support --bench edit_to_publish

use std::path::{Path, PathBuf};
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use tempfile::TempDir;
use url::Url;

use raven::handlers::{DiagCancelToken, diagnostics_via_snapshot};
use raven::state::{WorldState, scan_workspace};

// ---------------------------------------------------------------------------
// Topology generators
// ---------------------------------------------------------------------------

/// Write `path` with `content`.
fn write(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// A short body of identifier references and a function definition that
/// keeps diagnostic collectors busy without becoming pathologically large.
fn body_for(file_idx: usize) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "func_{idx} <- function(x, y = {idx}) {{\n",
        idx = file_idx
    ));
    s.push_str("    result <- x + y\n");
    s.push_str(&format!("    helper_{idx}(result)\n", idx = file_idx));
    s.push_str("    if (is.na(result)) return(NULL)\n");
    s.push_str("    result\n");
    s.push_str("}\n\n");
    s.push_str(&format!(
        "helper_{idx} <- function(z) z + 1\n",
        idx = file_idx
    ));
    for i in 0..10 {
        s.push_str(&format!("var_{idx}_{i} <- {i}\n", idx = file_idx, i = i));
    }
    s
}

/// Linear chain: file_0 sources file_1 sources file_2 ... up to depth.
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

/// Fanout: `leaves` files each source a single shared `utility.R`.
/// The hub itself defines functions that the leaves can reference.
fn write_fanout(dir: &Path, leaves: usize) {
    // Hub
    let mut hub = String::from("library(stats)\n\n");
    for i in 0..5 {
        hub.push_str(&format!("util_{i} <- function(x) x + {i}\n", i = i));
    }
    hub.push('\n');
    hub.push_str(&body_for(999));
    write(&dir.join("utility.R"), &hub);

    // Leaves
    for i in 0..leaves {
        let mut leaf = String::from("library(dplyr)\n\nsource(\"utility.R\")\n\n");
        leaf.push_str(&body_for(i));
        leaf.push_str(&format!("call_util_{i} <- util_{}(1)\n", i % 5));
        write(&dir.join(format!("leaf_{i}.R")), &leaf);
    }
}

/// Mixed: fanout hub of `leaves` leaves; each leaf is the head of a chain of
/// length `chain_depth` (leaf_i sources chain_i_1 sources chain_i_2 ...).
fn write_mixed(dir: &Path, leaves: usize, chain_depth: usize) {
    // Hub
    let mut hub = String::from("library(stats)\n\n");
    for i in 0..5 {
        hub.push_str(&format!("util_{i} <- function(x) x + {i}\n", i = i));
    }
    write(&dir.join("utility.R"), &hub);

    for i in 0..leaves {
        // Leaf sources hub and head of its own chain.
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

// ---------------------------------------------------------------------------
// Workspace -> WorldState helper
// ---------------------------------------------------------------------------

/// Build a fully-populated `WorldState` mirroring the LSP at steady-state:
/// each `.R` file is registered both in the `open-document authority` (where production
/// caches artifacts) AND in the legacy `documents` HashMap (which `did_change`
/// still maintains for backward compatibility), and the workspace scan is applied.
fn build_state(workspace: &Path) -> (WorldState, Arc<Url>) {
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

    let entries = scan_workspace(std::slice::from_ref(&folder_url), 20);
    state.apply_workspace_index(entries);

    (state, Arc::new(folder_url))
}

fn uri_of(workspace: &Path, name: &str) -> Url {
    Url::from_file_path(workspace.join(name)).unwrap()
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Per-file synchronous diagnostic computation: matches the work done
/// between `did_change` debounce expiry and `publish_diagnostics` for a
/// single file (snapshot build + collectors + sync resolution).
fn bench_single_file(c: &mut Criterion) {
    let mut group = c.benchmark_group("edit_to_publish/single_file");
    group.sample_size(20);

    // Topology 1: linear chain
    let lc_dir = TempDir::new().unwrap();
    write_linear_chain(lc_dir.path(), 15);
    let (lc_state, _) = build_state(lc_dir.path());
    let lc_root = uri_of(lc_dir.path(), "file_0.R");
    let lc_leaf = uri_of(lc_dir.path(), "file_14.R");
    let cancel = DiagCancelToken::never();

    group.bench_with_input(
        BenchmarkId::new("linear_chain_15", "root"),
        &(&lc_state, &lc_root, &cancel),
        |b, &(state, uri, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(
                    black_box(state),
                    black_box(uri),
                    black_box(c),
                ))
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("linear_chain_15", "leaf"),
        &(&lc_state, &lc_leaf, &cancel),
        |b, &(state, uri, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(
                    black_box(state),
                    black_box(uri),
                    black_box(c),
                ))
            })
        },
    );

    // Topology 2: fanout
    let fan_dir = TempDir::new().unwrap();
    write_fanout(fan_dir.path(), 30);
    let (fan_state, _) = build_state(fan_dir.path());
    let fan_hub = uri_of(fan_dir.path(), "utility.R");
    let fan_leaf = uri_of(fan_dir.path(), "leaf_0.R");

    group.bench_with_input(
        BenchmarkId::new("fanout_30", "hub"),
        &(&fan_state, &fan_hub, &cancel),
        |b, &(state, uri, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(
                    black_box(state),
                    black_box(uri),
                    black_box(c),
                ))
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("fanout_30", "leaf"),
        &(&fan_state, &fan_leaf, &cancel),
        |b, &(state, uri, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(
                    black_box(state),
                    black_box(uri),
                    black_box(c),
                ))
            })
        },
    );

    // Topology 3: mixed (hub + 20 leaves, each leaf has chain depth 5)
    let mix_dir = TempDir::new().unwrap();
    write_mixed(mix_dir.path(), 20, 5);
    let (mix_state, _) = build_state(mix_dir.path());
    let mix_hub = uri_of(mix_dir.path(), "utility.R");
    let mix_leaf = uri_of(mix_dir.path(), "leaf_0.R");
    let mix_chain_tail = uri_of(mix_dir.path(), "chain_0_4.R");

    group.bench_with_input(
        BenchmarkId::new("mixed_20x5", "hub"),
        &(&mix_state, &mix_hub, &cancel),
        |b, &(state, uri, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(
                    black_box(state),
                    black_box(uri),
                    black_box(c),
                ))
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("mixed_20x5", "leaf"),
        &(&mix_state, &mix_leaf, &cancel),
        |b, &(state, uri, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(
                    black_box(state),
                    black_box(uri),
                    black_box(c),
                ))
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("mixed_20x5", "chain_tail"),
        &(&mix_state, &mix_chain_tail, &cancel),
        |b, &(state, uri, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(
                    black_box(state),
                    black_box(uri),
                    black_box(c),
                ))
            })
        },
    );

    group.finish();
}

/// Cumulative time to recompute diagnostics for the edited file *and* every
/// open dependent serially. Bounds the worst-case time for "all diagnostics
/// up-to-date" after editing the hub.
fn bench_hub_and_all_dependents(c: &mut Criterion) {
    let mut group = c.benchmark_group("edit_to_publish/hub_and_all_dependents");
    group.sample_size(10);
    let cancel = DiagCancelToken::never();

    // Topology: fanout 30
    let fan_dir = TempDir::new().unwrap();
    write_fanout(fan_dir.path(), 30);
    let (fan_state, _) = build_state(fan_dir.path());
    let fan_hub = uri_of(fan_dir.path(), "utility.R");
    let fan_leaves: Vec<Url> = (0..30)
        .map(|i| uri_of(fan_dir.path(), &format!("leaf_{i}.R")))
        .collect();

    group.bench_with_input(
        BenchmarkId::new("fanout_30", "serial_30_leaves"),
        &(&fan_state, &fan_hub, &fan_leaves, &cancel),
        |b, &(state, hub, leaves, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(state, hub, c));
                for leaf in leaves.iter() {
                    black_box(diagnostics_via_snapshot(state, leaf, c));
                }
            })
        },
    );

    // Topology: mixed
    let mix_dir = TempDir::new().unwrap();
    write_mixed(mix_dir.path(), 20, 5);
    let (mix_state, _) = build_state(mix_dir.path());
    let mix_hub = uri_of(mix_dir.path(), "utility.R");
    let mix_leaves: Vec<Url> = (0..20)
        .map(|i| uri_of(mix_dir.path(), &format!("leaf_{i}.R")))
        .collect();

    group.bench_with_input(
        BenchmarkId::new("mixed_20x5", "serial_20_leaves"),
        &(&mix_state, &mix_hub, &mix_leaves, &cancel),
        |b, &(state, hub, leaves, c)| {
            b.iter(|| {
                black_box(diagnostics_via_snapshot(state, hub, c));
                for leaf in leaves.iter() {
                    black_box(diagnostics_via_snapshot(state, leaf, c));
                }
            })
        },
    );

    group.finish();
}

criterion_group!(benches, bench_single_file, bench_hub_and_all_dependents);
criterion_main!(benches);
