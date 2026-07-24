// Profile cold-start lag on the user's real ~/repos/worldwide workspace
// (or any other workspace passed as the first CLI argument).
//
// Times each phase of the cold-start path that runs between
// `Backend::initialized` and the first non-deferred diagnostic publish for
// `scripts/data.r`:
//
//   1. Filesystem walk + per-file parse/metadata/artifacts (the body of
//      `scan_workspace`'s single-threaded inner loop, broken into phases).
//   2. End-to-end `scan_workspace` (matches what runs on Task A).
//   3. `apply_workspace_index` (the post-scan write-lock-held step).
//   4. `RSubprocess::new` + `lib.initialize` (the work `did_open` runs
//      synchronously when it arrives before Task B finishes, and what
//      `Backend::initialized` does in `spawn_blocking`).
//   5. Cold-start `did_open` simulation: build snapshot + run diagnostic
//      collector for `scripts/data.r` BEFORE the workspace scan completes
//      (the deferred path).
//   6. Post-scan `did_open` simulation: same file, but with the workspace
//      index applied (the path that fires after the post-scan force-republish).
//
// Run with:
//   cargo run --release --example profile_worldwide --features test-support [-- WORKSPACE_PATH]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use url::Url;

use raven::cross_file::file_cache::FileSnapshot;
use raven::cross_file::{CrossFileMetadata, extract_metadata, extract_metadata_with_tree, scope};
use raven::handlers::{DiagCancelToken, diagnostics_via_snapshot_profile};
use raven::package_library::PackageLibrary;
use raven::r_subprocess::RSubprocess;
use raven::state::{Document, WorldState, scan_workspace};
use raven::workspace_index as new_workspace_index;

const SKIP_DIRECTORIES: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    ".Rproj.user",
    "renv",
    "packrat",
    ".vscode",
    ".idea",
    "target",
];

fn should_skip(name: &str) -> bool {
    SKIP_DIRECTORIES
        .iter()
        .any(|s| name.eq_ignore_ascii_case(s))
}

fn is_r_file(p: &Path) -> bool {
    p.extension().and_then(|s| s.to_str()).is_some_and(|ext| {
        ext.eq_ignore_ascii_case("r")
            || ext.eq_ignore_ascii_case("jags")
            || ext.eq_ignore_ascii_case("bugs")
            || ext.eq_ignore_ascii_case("bug")
            || ext.eq_ignore_ascii_case("stan")
    })
}

fn discover(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(n) = p.file_name().and_then(|n| n.to_str())
                && should_skip(n)
            {
                continue;
            }
            discover(&p, out);
        } else if is_r_file(&p) {
            out.push(p);
        }
    }
}

/// Sequential per-file phase breakdown — mirrors the body of `scan_directory`.
struct ScanPhaseTimings {
    files: usize,
    bytes: usize,
    discover: Duration,
    read: Duration,
    parse: Duration,
    metadata: Duration,
    artifacts: Duration,
    fs_meta: Duration,
    insert: Duration,
}

fn measure_scan_phases(workspace: &Path) -> ScanPhaseTimings {
    let mut paths = Vec::new();
    let t = Instant::now();
    discover(workspace, &mut paths);
    let discover_t = t.elapsed();

    paths.sort();

    let mut t_read = Duration::ZERO;
    let mut t_parse = Duration::ZERO;
    let mut t_metadata = Duration::ZERO;
    let mut t_artifacts = Duration::ZERO;
    let mut t_fs_meta = Duration::ZERO;
    let mut t_insert = Duration::ZERO;
    let mut total_bytes = 0usize;

    let mut index: HashMap<Url, Document> = HashMap::new();
    let mut new_index: HashMap<Url, new_workspace_index::IndexEntry> = HashMap::new();

    for path in &paths {
        let t = Instant::now();
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        t_read += t.elapsed();
        total_bytes += text.len();

        let Ok(uri) = Url::from_file_path(path) else {
            continue;
        };

        let t = Instant::now();
        let doc = Document::new_with_uri(&text, None, &uri);
        t_parse += t.elapsed();

        let t = Instant::now();
        let Ok(fs_meta) = std::fs::metadata(path) else {
            continue;
        };
        t_fs_meta += t.elapsed();

        let t = Instant::now();
        let xf_meta = extract_metadata(&text);
        t_metadata += t.elapsed();

        let t = Instant::now();
        let artifacts = Arc::new(if let Some(tree) = doc.tree.as_ref() {
            scope::compute_artifacts_with_metadata(&uri, tree, &text, Some(&xf_meta))
        } else {
            scope::ScopeArtifacts::default()
        });
        t_artifacts += t.elapsed();

        let snapshot = FileSnapshot::with_content_hash(&fs_meta, &text);
        let xf_meta_arc: Arc<CrossFileMetadata> = Arc::new(xf_meta);

        let t = Instant::now();
        new_index.insert(
            uri.clone(),
            new_workspace_index::IndexEntry {
                contents: doc.contents.clone(),
                tree: doc.tree.clone(),
                loaded_packages: doc.loaded_packages.clone(),
                data_packages: vec![],
                snapshot,
                metadata: xf_meta_arc,
                artifacts,
                indexed_at_version: 0,
            },
        );
        index.insert(uri, doc);
        t_insert += t.elapsed();
    }

    ScanPhaseTimings {
        files: paths.len(),
        bytes: total_bytes,
        discover: discover_t,
        read: t_read,
        parse: t_parse,
        metadata: t_metadata,
        artifacts: t_artifacts,
        fs_meta: t_fs_meta,
        insert: t_insert,
    }
}

fn make_state(workspace: &Path) -> WorldState {
    let mut state = WorldState::new();
    let folder = Url::from_file_path(workspace).unwrap();
    state.workspace_folders.push(folder);
    state
}

fn open_doc(state: &mut WorldState, path: &Path, _rt: &tokio::runtime::Runtime) -> Url {
    let text = std::fs::read_to_string(path).unwrap();
    let uri = Url::from_file_path(path).unwrap();
    state.open_document_with_language_id(uri.clone(), &text, Some(1), Some("r"));
    uri
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn percent(d: Duration, total: Duration) -> f64 {
    if total.as_secs_f64() == 0.0 {
        0.0
    } else {
        d.as_secs_f64() / total.as_secs_f64() * 100.0
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp(None)
        .try_init()
        .ok();

    let workspace = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").expect("HOME env var");
            PathBuf::from(home).join("repos/worldwide")
        });

    if !workspace.exists() {
        eprintln!("Workspace does not exist: {}", workspace.display());
        std::process::exit(2);
    }

    println!("=== Profile: cold-start on {} ===\n", workspace.display());

    // ------------------------------------------------------------------
    // Phase 1: filesystem walk + per-file scan body
    // ------------------------------------------------------------------
    println!("[1] Per-file scan body (single-threaded, mirrors scan_directory):");
    let phases = measure_scan_phases(&workspace);
    let inner = phases.read
        + phases.parse
        + phases.metadata
        + phases.artifacts
        + phases.fs_meta
        + phases.insert;
    let total = phases.discover + inner;
    println!(
        "    files: {}, bytes: {} KB",
        phases.files,
        phases.bytes / 1024
    );
    println!(
        "    discover (read_dir tree): {:>7.1}ms",
        ms(phases.discover)
    );
    println!(
        "    fs::read_to_string:        {:>7.1}ms ({:>4.1}%)",
        ms(phases.read),
        percent(phases.read, total)
    );
    println!(
        "    Document::new_with_uri:    {:>7.1}ms ({:>4.1}%)  <- tree-sitter parse",
        ms(phases.parse),
        percent(phases.parse, total)
    );
    println!(
        "    fs::metadata:              {:>7.1}ms ({:>4.1}%)",
        ms(phases.fs_meta),
        percent(phases.fs_meta, total)
    );
    println!(
        "    extract_metadata:          {:>7.1}ms ({:>4.1}%)",
        ms(phases.metadata),
        percent(phases.metadata, total)
    );
    println!(
        "    compute_artifacts:         {:>7.1}ms ({:>4.1}%)  <- AST walk",
        ms(phases.artifacts),
        percent(phases.artifacts, total)
    );
    println!(
        "    HashMap inserts:           {:>7.1}ms ({:>4.1}%)",
        ms(phases.insert),
        percent(phases.insert, total)
    );
    println!("    TOTAL:                     {:>7.1}ms", ms(total));
    println!();

    // ------------------------------------------------------------------
    // Phase 2: end-to-end scan_workspace (median of 3 runs)
    // ------------------------------------------------------------------
    println!("[2] scan_workspace (end-to-end, includes dependency-graph buildup):");
    let folder = Url::from_file_path(&workspace).unwrap();
    let mut runs = Vec::new();
    let mut last: Option<usize> = None;
    for _ in 0..3 {
        let t = Instant::now();
        let entries = scan_workspace(std::slice::from_ref(&folder), 20);
        let elapsed = t.elapsed();
        runs.push(elapsed);
        last = Some(entries.len());
    }
    runs.sort();
    let median = runs[runs.len() / 2];
    println!("    median scan_workspace: {:>7.1}ms", ms(median));
    println!(
        "    runs: {:?}",
        runs.iter().map(|d| ms(*d)).collect::<Vec<_>>()
    );
    if let Some(entries) = last {
        println!("    index entries: {}", entries);
    }
    println!();

    // ------------------------------------------------------------------
    // Phase 3: apply_workspace_index (write-lock-held step)
    // ------------------------------------------------------------------
    println!("[3] apply_workspace_index (write-lock-held in Task A):");
    let mut apply_runs = Vec::new();
    for _ in 0..3 {
        // Fresh scan each time so the inputs aren't moved
        let entries = scan_workspace(std::slice::from_ref(&folder), 20);
        let mut state = make_state(&workspace);
        let t = Instant::now();
        state.apply_workspace_index(entries);
        apply_runs.push(t.elapsed());
    }
    apply_runs.sort();
    let median_apply = apply_runs[apply_runs.len() / 2];
    println!(
        "    median apply_workspace_index: {:>7.1}ms",
        ms(median_apply)
    );
    println!(
        "    runs: {:?}",
        apply_runs.iter().map(|d| ms(*d)).collect::<Vec<_>>()
    );
    println!();

    // ------------------------------------------------------------------
    // Phase 4: PackageLibrary init (RSubprocess::new + lib.initialize)
    // ------------------------------------------------------------------
    println!("[4] PackageLibrary init (Task B / on-demand from did_open):");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let t_total = Instant::now();
    let t = Instant::now();
    let r_subprocess = RSubprocess::new(None);
    let t_rsub = t.elapsed();
    println!(
        "    RSubprocess::new:             {:>7.1}ms (R discovery + sanity check)",
        ms(t_rsub)
    );

    let workspace_root = workspace.clone();
    let r_subprocess = r_subprocess.map(|sub| sub.with_working_dir(workspace_root.clone()));

    let t = Instant::now();
    let mut lib = PackageLibrary::with_subprocess(r_subprocess);
    let init_result = rt.block_on(async { lib.initialize().await });
    let t_init = t.elapsed();
    println!(
        "    lib.initialize:               {:>7.1}ms ({} lib_paths, {} base_packages, {} base_exports)",
        ms(t_init),
        lib.lib_paths().len(),
        lib.base_packages().len(),
        lib.base_exports().len()
    );
    if let Err(e) = init_result {
        println!("    lib.initialize ERROR: {}", e);
    }
    let lib = Arc::new(lib);
    println!(
        "    Total init:                   {:>7.1}ms",
        ms(t_total.elapsed())
    );
    println!();

    // ------------------------------------------------------------------
    // Phase 5: scripts/data.r diagnostics — cold start (no scan applied)
    // ------------------------------------------------------------------
    let target = workspace.join("scripts/data.r");
    if !target.exists() {
        println!("(skipping phases 5/6: {} does not exist)", target.display());
        return;
    }
    println!("[5] scripts/data.r diagnostics — COLD start (workspace_scan_complete=false):");
    let mut state = make_state(&workspace);
    state.package_library = lib.clone();
    state.package_library_ready = true;
    let target_uri = open_doc(&mut state, &target, &rt);
    // No apply_workspace_index call — workspace_scan_complete remains false.
    let cancel = DiagCancelToken::never();
    // Warmup
    for _ in 0..3 {
        let _ = diagnostics_via_snapshot_profile(&state, &target_uri, &cancel);
    }
    let mut builds = Vec::new();
    let mut diags = Vec::new();
    let mut last_outcome: Option<usize> = None;
    for _ in 0..10 {
        let (b, d, outcome) = diagnostics_via_snapshot_profile(&state, &target_uri, &cancel);
        builds.push(b);
        diags.push(d);
        last_outcome = outcome;
    }
    builds.sort();
    diags.sort();
    let median_build_cold = builds[builds.len() / 2];
    let median_diag_cold = diags[diags.len() / 2];
    let count_cold = match last_outcome {
        Some(n) => n.to_string(),
        None => "<short-circuited>".to_string(),
    };
    println!(
        "    snapshot build: {:>6.2}ms  diag: {:>6.2}ms  total: {:>6.2}ms  diags={}",
        ms(median_build_cold),
        ms(median_diag_cold),
        ms(median_build_cold + median_diag_cold),
        count_cold
    );
    println!();

    // ------------------------------------------------------------------
    // Phase 6: scripts/data.r diagnostics — POST-scan (workspace index applied)
    // ------------------------------------------------------------------
    println!("[6] scripts/data.r diagnostics — POST-scan (workspace_scan_complete=true):");
    let entries = scan_workspace(std::slice::from_ref(&folder), 20);
    state.apply_workspace_index(entries);
    // Warmup
    for _ in 0..3 {
        let _ = diagnostics_via_snapshot_profile(&state, &target_uri, &cancel);
    }
    let mut builds = Vec::new();
    let mut diags = Vec::new();
    let mut last_outcome: Option<usize> = None;
    for _ in 0..10 {
        let (b, d, outcome) = diagnostics_via_snapshot_profile(&state, &target_uri, &cancel);
        builds.push(b);
        diags.push(d);
        last_outcome = outcome;
    }
    builds.sort();
    diags.sort();
    let median_build_warm = builds[builds.len() / 2];
    let median_diag_warm = diags[diags.len() / 2];
    let count_warm = match last_outcome {
        Some(n) => n.to_string(),
        None => "<short-circuited>".to_string(),
    };
    println!(
        "    snapshot build: {:>6.2}ms  diag: {:>6.2}ms  total: {:>6.2}ms  diags={}",
        ms(median_build_warm),
        ms(median_diag_warm),
        ms(median_build_warm + median_diag_warm),
        count_warm
    );
    println!();

    // ------------------------------------------------------------------
    // Summary
    // ------------------------------------------------------------------
    println!("=== Cold-start lag attribution ===\n");
    println!("Wall-clock from did_open(data.r) to first non-deferred diagnostic publish ≈");
    println!("  scan_workspace          : {:>7.1}ms", ms(median));
    println!("  apply_workspace_index   : {:>7.1}ms", ms(median_apply));
    println!(
        "  post-scan diagnostic     : {:>7.1}ms (build {:>5.1}ms + diag {:>5.1}ms)",
        ms(median_build_warm + median_diag_warm),
        ms(median_build_warm),
        ms(median_diag_warm)
    );
    let cold_total = median + median_apply + median_build_warm + median_diag_warm;
    println!(
        "  --------------------------- {:>7.1}ms (lower bound; ignores prefetch + lock contention)",
        ms(cold_total)
    );
    println!();
    println!("R subprocess startup (parallel to scan if Task B not ready):");
    println!("  RSubprocess::new        : {:>7.1}ms", ms(t_rsub));
    println!("  lib.initialize          : {:>7.1}ms", ms(t_init));
    println!();

    // ------------------------------------------------------------------
    // FIX EXPERIMENTS — measure expected speedups
    // ------------------------------------------------------------------
    println!("=== FIX EXPERIMENT A: eliminate redundant parse in scan_directory ===\n");
    println!("Current scan_directory calls Document::new_with_uri (parses) AND");
    println!("extract_metadata (parses AGAIN via parser_pool::with_parser).");
    println!("If we pass the existing tree via extract_metadata_with_tree, we skip");
    println!("the second parse for every file.\n");

    // A/B comparison on a fixed subset
    let mut paths = Vec::new();
    discover(&workspace, &mut paths);
    paths.sort();
    println!("(measuring across {} files)", paths.len());

    fn measure_scan_inner_loop<F>(paths: &[PathBuf], builder: F) -> Duration
    where
        F: Fn(&PathBuf, &str, &Document) -> CrossFileMetadata,
    {
        let t = Instant::now();
        for p in paths {
            let Ok(text) = std::fs::read_to_string(p) else {
                continue;
            };
            let Ok(uri) = Url::from_file_path(p) else {
                continue;
            };
            let doc = Document::new_with_uri(&text, None, &uri);
            let xf = builder(p, &text, &doc);
            let _artifacts = if let Some(tree) = doc.tree.as_ref() {
                scope::compute_artifacts_with_metadata(&uri, tree, &text, Some(&xf))
            } else {
                scope::ScopeArtifacts::default()
            };
        }
        t.elapsed()
    }

    let mut a_runs = Vec::new();
    let mut b_runs = Vec::new();
    for _ in 0..3 {
        let t_a = measure_scan_inner_loop(&paths, |_p, text, _doc| extract_metadata(text));
        let t_b = measure_scan_inner_loop(&paths, |_p, text, doc| {
            extract_metadata_with_tree(text, doc.tree.as_ref())
        });
        a_runs.push(t_a);
        b_runs.push(t_b);
    }
    a_runs.sort();
    b_runs.sort();
    let a = a_runs[a_runs.len() / 2];
    let b = b_runs[b_runs.len() / 2];
    println!(
        "  current   (extract_metadata):              {:>7.1}ms (median of 3)",
        ms(a)
    );
    println!(
        "  fix       (extract_metadata_with_tree):   {:>7.1}ms (median of 3)",
        ms(b)
    );
    let saved = a.saturating_sub(b);
    println!(
        "  saved:                                     {:>7.1}ms ({:.1}%)",
        ms(saved),
        percent(saved, a)
    );
    println!();

    // ------------------------------------------------------------------
    println!("=== FIX EXPERIMENT B: parallel scan via std::thread ===\n");
    println!("Mirrors what rayon::par_iter (or a thread pool) over the file list");
    println!("would do. The dependency-graph rebuild (apply_workspace_index) still");
    println!("runs serially, but the per-file parse + metadata + artifacts (the");
    println!("dominant 1100ms phase) is embarrassingly parallel.\n");

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    println!("(machine has {cores} CPUs reported by available_parallelism)");
    println!();

    fn parallel_scan(paths: &[PathBuf], threads: usize) -> Duration {
        let t = Instant::now();
        let chunk_size = paths.len().div_ceil(threads.max(1));
        let chunks: Vec<&[PathBuf]> = paths.chunks(chunk_size).collect();
        std::thread::scope(|s| {
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    s.spawn(move || {
                        let mut local_new = HashMap::new();
                        for p in chunk {
                            let Ok(text) = std::fs::read_to_string(p) else {
                                continue;
                            };
                            let Ok(uri) = Url::from_file_path(p) else {
                                continue;
                            };
                            let Ok(fs_meta) = std::fs::metadata(p) else {
                                continue;
                            };
                            let doc = Document::new_with_uri(&text, None, &uri);
                            // Use extract_metadata_with_tree (re-parse eliminated).
                            let xf_meta = extract_metadata_with_tree(&text, doc.tree.as_ref());
                            let artifacts = Arc::new(if let Some(tree) = doc.tree.as_ref() {
                                scope::compute_artifacts_with_metadata(
                                    &uri,
                                    tree,
                                    &text,
                                    Some(&xf_meta),
                                )
                            } else {
                                scope::ScopeArtifacts::default()
                            });
                            let snap =
                                raven::cross_file::file_cache::FileSnapshot::with_content_hash(
                                    &fs_meta, &text,
                                );
                            let xf_meta_arc: Arc<CrossFileMetadata> = Arc::new(xf_meta);
                            local_new.insert(
                                uri,
                                new_workspace_index::IndexEntry {
                                    contents: doc.contents.clone(),
                                    tree: doc.tree.clone(),
                                    loaded_packages: doc.loaded_packages.clone(),
                                    data_packages: vec![],
                                    snapshot: snap,
                                    metadata: xf_meta_arc,
                                    artifacts,
                                    indexed_at_version: 0,
                                },
                            );
                        }
                        local_new
                    })
                })
                .collect();
            // Merge results serially after parallel work
            let mut merged_new: HashMap<Url, new_workspace_index::IndexEntry> = HashMap::new();
            for h in handles {
                let entries = h.join().expect("scan thread panicked");
                merged_new.extend(entries);
            }
        });
        t.elapsed()
    }

    for threads in [1usize, 2, 4, 8, cores] {
        let mut runs = Vec::new();
        for _ in 0..3 {
            runs.push(parallel_scan(&paths, threads));
        }
        runs.sort();
        let median = runs[runs.len() / 2];
        let speedup = ms(median).max(0.001);
        let baseline = ms(a).max(0.001);
        let factor = baseline / speedup;
        println!(
            "  threads={:>2}:  {:>7.1}ms ({:.2}x vs serial-with-current-dual-parse)",
            threads, speedup, factor
        );
    }
    println!();

    println!("=== Summary of fix candidates ===\n");
    println!("Cold-start path BEFORE fixes (measured):");
    println!("  scan_workspace                : {:>7.1}ms", ms(median));
    println!(
        "  apply_workspace_index         : {:>7.1}ms",
        ms(median_apply)
    );
    println!(
        "  post-scan diagnostics (data.r): {:>7.1}ms",
        ms(median_build_warm + median_diag_warm)
    );
    println!(
        "  TOTAL                          : {:>7.1}ms",
        ms(cold_total)
    );
}
