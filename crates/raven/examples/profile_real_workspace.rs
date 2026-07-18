// Profile the startup pipeline phases on a real workspace.
//
// Run with:
//   cargo run --release --example profile_real_workspace --features test-support
//   cargo run --release --example profile_real_workspace --features test-support -- /path/to/workspace
//   WORKSPACE=/path/to/workspace cargo run --release --example profile_real_workspace --features test-support
//
// Workspace resolution order:
//   1. WORKSPACE environment variable
//   2. First CLI argument
//   3. Current working directory (fallback)
//   4. ~/repos/worldwide (legacy default)

use std::path::PathBuf;
use std::time::Instant;

use raven::handlers::{DiagCancelToken, diagnostics_via_snapshot_profile};
use raven::state::{Document, WorldState, scan_workspace};
use url::Url;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_millis()
        .init();

    let workspace = if let Ok(ws) = std::env::var("WORKSPACE") {
        PathBuf::from(ws)
    } else if let Some(arg) = std::env::args().nth(1) {
        PathBuf::from(arg)
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd
    } else {
        let home = std::env::var_os("HOME").expect("HOME not set");
        PathBuf::from(home).join("repos/worldwide")
    };
    let workspace = match workspace.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: cannot resolve {}: {}", workspace.display(), e);
            std::process::exit(1);
        }
    };

    let workspace_url = Url::from_file_path(&workspace).expect("canonical path to URL");

    // ── Phase 1: scan_workspace (cold filesystem) ──
    eprintln!("\n=== Phase 1: scan_workspace (cold) ===");
    let t0 = Instant::now();
    let (cross_file_entries, new_index_entries) =
        scan_workspace(std::slice::from_ref(&workspace_url), 20);
    let scan_time = t0.elapsed();
    eprintln!(
        "  {:?} ({} files, {} cross-file entries, {} new index)",
        scan_time,
        new_index_entries.len(),
        cross_file_entries.len(),
        new_index_entries.len(),
    );

    // ── Phase 2: apply_workspace_index ──
    eprintln!("\n=== Phase 2: apply_workspace_index ===");
    let t1 = Instant::now();
    let mut state = WorldState::new();
    state.workspace_folders = vec![workspace_url.clone()];
    state.cross_file_config.undefined_variable_severity =
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING);
    state.cross_file_config.out_of_scope_severity =
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING);
    state.apply_workspace_index(cross_file_entries, new_index_entries);
    let apply_time = t1.elapsed();
    eprintln!("  {:?}", apply_time);
    eprintln!(
        "    workspace_scan_complete = {}",
        state.workspace_scan_complete
    );

    // ── Phase 3: open data.r and build diagnostic snapshot ──
    eprintln!("\n=== Phase 3: open data.r + snapshot + diagnostics ===");
    let data_path = workspace.join("scripts/data.r");
    let (build_dur, diag_dur, _diag_count) = if data_path.exists() {
        let data_uri = Url::from_file_path(&data_path).unwrap();
        let data_content = std::fs::read_to_string(&data_path).expect("read data.r");
        state.open_document(data_uri.clone(), &data_content, None);
        let result = diagnostics_via_snapshot_profile(&state, &data_uri, &DiagCancelToken::never());
        eprintln!("  snapshot build:      {:?}", result.0);
        eprintln!("  diagnostics compute: {:?}", result.1);
        eprintln!("  diagnostic count:    {:?}", result.2);
        result
    } else {
        eprintln!("  Phase 3 skipped: scripts/data.r not found");
        (std::time::Duration::ZERO, std::time::Duration::ZERO, None)
    };

    // ── Phase 3b: profile diagnostics for other representative files ──
    eprintln!("\n=== Phase 3b: diagnostics for other files ===");
    let test_files = [
        "main.r",
        "scripts/functions.r",
        "scripts/folders.r",
        "scripts/data/outcomes/abortions.r",
        "scripts/data/outcomes.r",
        "scripts/analysis/analysis.r",
    ];
    for rel in &test_files {
        let path = workspace.join(rel);
        if !path.exists() {
            eprintln!("  {:50} (not found)", rel);
            continue;
        }
        let uri = Url::from_file_path(&path).unwrap();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  {:50} (read error: {})", rel, e);
                continue;
            }
        };
        state.open_document(uri.clone(), &content, None);
        let (b, d, c) = diagnostics_via_snapshot_profile(&state, &uri, &DiagCancelToken::never());
        eprintln!(
            "  {:50} build={:>7.2}ms  diag={:>7.2}ms  total={:>7.2}ms  diags={}",
            rel,
            b.as_secs_f64() * 1000.0,
            d.as_secs_f64() * 1000.0,
            (b + d).as_secs_f64() * 1000.0,
            c.unwrap_or(0),
        );
    }

    // ── Phase 4: scan_workspace (warm filesystem) ──
    eprintln!("\n=== Phase 4: scan_workspace (warm) ===");
    let t4 = Instant::now();
    let (_, index2) = scan_workspace(std::slice::from_ref(&workspace_url), 20);
    let scan_warm_time = t4.elapsed();
    eprintln!("  {:?} ({} files)", scan_warm_time, index2.len());

    // ── Phase 5: scan_workspace broken down ──
    // Time just the file I/O + tree-sitter parse vs artifact compute
    eprintln!("\n=== Phase 5: per-file breakdown (sample of 10 largest files) ===");
    let mut file_sizes: Vec<(PathBuf, usize)> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<(PathBuf, usize)>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str())
                        && (name.starts_with('.')
                            || name == "node_modules"
                            || name == "output"
                            || name == "abortion_data"
                            || name == "fertility_surveys")
                    {
                        continue;
                    }
                    walk(&path, out);
                } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && matches!(ext, "r" | "R")
                    && let Ok(meta) = std::fs::metadata(&path)
                {
                    out.push((path, meta.len() as usize));
                }
            }
        }
    }
    walk(&workspace, &mut file_sizes);
    file_sizes.sort_by_key(|b| std::cmp::Reverse(b.1));

    for (path, size) in file_sizes.iter().take(10) {
        let Ok(content) = std::fs::read_to_string(path) else {
            eprintln!(
                "  {:50} (non-UTF-8, skipped)",
                path.strip_prefix(&workspace).unwrap_or(path).display()
            );
            continue;
        };
        let uri = Url::from_file_path(path).unwrap();
        let lines = content.lines().count();

        let tp = Instant::now();
        let doc = Document::new_with_uri(&content, None, &uri);
        let parse_time = tp.elapsed();

        let ta = Instant::now();
        if let Some(tree) = doc.tree.as_ref() {
            let meta = raven::cross_file::extract_metadata(&content);
            let _arts = raven::cross_file::scope::compute_artifacts_with_metadata(
                &uri,
                tree,
                &content,
                Some(&meta),
            );
        }
        let artifact_time = ta.elapsed();

        let rel = path.strip_prefix(&workspace).unwrap_or(path);
        eprintln!(
            "  {:50} {:>6} bytes  {:>5} lines  parse={:>6.2}ms  artifacts={:>6.2}ms",
            rel.display(),
            size,
            lines,
            parse_time.as_secs_f64() * 1000.0,
            artifact_time.as_secs_f64() * 1000.0,
        );
    }

    // ── Summary ──
    eprintln!("\n=== SUMMARY ===");
    eprintln!("  scan_workspace (cold):       {:?}", scan_time);
    eprintln!("  scan_workspace (warm):       {:?}", scan_warm_time);
    eprintln!("  apply_workspace_index:       {:?}", apply_time);
    eprintln!("  snapshot build (data.r):     {:?}", build_dur);
    eprintln!("  diagnostics (data.r):        {:?}", diag_dur);
    eprintln!("  ────────────────────────────────────────");
    eprintln!(
        "  total cold-start lag:        {:?}  (scan + apply)",
        scan_time + apply_time
    );
    eprintln!(
        "  total including republish:   {:?}  (+ snapshot + diag)",
        scan_time + apply_time + build_dur + diag_dur
    );
    eprintln!("\n  Note: excludes PackageLibrary init (~75-350ms per R subprocess call)");
    eprintln!("  Note: excludes package prefetch time");
    eprintln!("  Note: excludes revalidation_debounce_ms (default 100ms)");
}
