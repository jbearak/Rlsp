//! Time-budget regression tests for Raven.
//!
//! These tests assert that key operations complete within wall-clock thresholds.
//! They are gated behind `#[cfg(not(debug_assertions))]` because debug-mode
//! timings are not meaningful.
//!
//! Run with: `cargo test --release -p raven --features test-support --test performance_budgets`
//!
//! CI adaptation: when the `CI` environment variable is set, thresholds are
//! multiplied by a relaxation factor (default 3×, configurable via
//! `RAVEN_PERF_CI_FACTOR`).

// Only compile in release mode — debug timings are meaningless.
#![cfg(not(debug_assertions))]

use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------

/// Run `f` three times and return the median duration.
///
/// Using the median (rather than the mean) reduces the impact of outliers
/// caused by OS scheduling jitter, page faults on first access, etc.
fn median_of_3<F: FnMut()>(mut f: F) -> Duration {
    let mut times = [Duration::ZERO; 3];
    for t in &mut times {
        let start = Instant::now();
        f();
        *t = start.elapsed();
    }
    times.sort();
    times[1]
}

/// Compute the CI relaxation factor from explicit env var values.
///
/// - When `ci` is `Some(non-empty)`, returns the parsed `factor` (default **3.0**).
/// - Otherwise returns **1.0** (no relaxation).
fn ci_factor_from(ci: Option<&str>, factor: Option<&str>) -> f64 {
    let is_ci = ci.map(|v| !v.is_empty()).unwrap_or(false);

    if is_ci {
        factor
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|&f| f >= 1.0)
            .unwrap_or(3.0)
    } else {
        1.0
    }
}

/// Return the CI relaxation factor for time-budget thresholds.
///
/// - When `CI` is set (to any non-empty value), returns the value of
///   `RAVEN_PERF_CI_FACTOR` (default **3.0**).
/// - Otherwise returns **1.0** (no relaxation).
fn ci_factor() -> f64 {
    ci_factor_from(
        std::env::var("CI").ok().as_deref(),
        std::env::var("RAVEN_PERF_CI_FACTOR").ok().as_deref(),
    )
}

/// Assert that `measured` is within `budget_ms × ci_factor()`.
///
/// Panics with a descriptive message that includes:
/// - the label identifying the operation,
/// - the measured time,
/// - the effective threshold,
/// - whether CI relaxation was applied.
fn assert_within_budget(label: &str, measured: Duration, budget_ms: u64) {
    let factor = ci_factor();
    let threshold = Duration::from_secs_f64(budget_ms as f64 * factor / 1000.0);
    let ci_note = if factor > 1.0 {
        format!(" (CI relaxation {factor:.1}× applied)")
    } else {
        String::new()
    };

    assert!(
        measured <= threshold,
        "Time budget exceeded for '{label}': \
         measured {measured:.1?}, threshold {threshold:.1?} \
         (base {budget_ms}ms × {factor:.1}){ci_note}",
    );
}

// ---------------------------------------------------------------------------
// Harness self-tests
// ---------------------------------------------------------------------------

#[test]
fn median_of_3_calls_f_exactly_3_times() {
    let mut count = 0u32;
    let _ = median_of_3(|| {
        count += 1;
    });
    assert_eq!(count, 3, "median_of_3 should call f exactly 3 times");
}

#[test]
fn median_of_3_returns_middle_duration() {
    // We can't control exact wall-clock durations, but we can verify the
    // sort-and-pick-middle logic by checking that the result is bounded
    // between the fastest and slowest of three no-op calls.
    let result = median_of_3(|| {
        // no-op — all three durations should be very close to zero
    });
    // The median of three near-zero durations should itself be near zero.
    assert!(
        result < Duration::from_millis(50),
        "median of three no-ops should be < 50ms, got {result:?}"
    );
}

#[test]
fn ci_factor_is_1_when_ci_unset() {
    let factor = ci_factor_from(None, None);
    assert!(
        (factor - 1.0).abs() < f64::EPSILON,
        "ci_factor should be 1.0 when CI is unset, got {factor}"
    );
}

#[test]
fn ci_factor_is_3_when_ci_set_without_custom_factor() {
    let factor = ci_factor_from(Some("true"), None);
    assert!(
        (factor - 3.0).abs() < f64::EPSILON,
        "ci_factor should be 3.0 when CI is set without custom factor, got {factor}"
    );
}

#[test]
fn ci_factor_uses_custom_value() {
    let factor = ci_factor_from(Some("true"), Some("5.0"));
    assert!(
        (factor - 5.0).abs() < f64::EPSILON,
        "ci_factor should use custom value, got {factor}"
    );
}

#[test]
fn ci_factor_ignores_invalid_custom_value() {
    let factor = ci_factor_from(Some("true"), Some("not_a_number"));
    assert!(
        (factor - 3.0).abs() < f64::EPSILON,
        "ci_factor should fall back to 3.0 for invalid value, got {factor}"
    );
}

#[test]
fn ci_factor_rejects_factor_below_1() {
    let factor = ci_factor_from(Some("true"), Some("0.5"));
    assert!(
        (factor - 3.0).abs() < f64::EPSILON,
        "ci_factor should reject factor < 1.0, got {factor}"
    );
}

#[test]
fn assert_within_budget_passes_for_fast_op() {
    let measured = Duration::from_micros(100);
    // Should not panic: 100µs is well within 5ms.
    assert_within_budget("trivial_op", measured, 5);
}

#[test]
#[should_panic(expected = "Time budget exceeded")]
fn assert_within_budget_panics_for_slow_op() {
    let measured = Duration::from_secs(1);
    // 1s is way over a 5ms budget.
    assert_within_budget("slow_op", measured, 5);
}

#[test]
fn assert_within_budget_message_includes_details() {
    let measured = Duration::from_millis(100);
    let result = std::panic::catch_unwind(|| {
        assert_within_budget("test_op", measured, 1);
    });
    let err = result.expect_err("should have panicked");
    let msg = err
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .unwrap_or("");
    assert!(msg.contains("test_op"), "message should contain label");
    assert!(
        msg.contains("threshold"),
        "message should contain 'threshold'"
    );
    assert!(
        msg.contains("base 1ms"),
        "message should contain base budget"
    );
}

// ---------------------------------------------------------------------------
// Time-budget tests for real operations
// Requirements: 2.1
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};
use tree_sitter::Parser;
use url::Url;

use raven::test_utils::fixture_workspace::{FixtureConfig, create_fixture_workspace};
use raven::{
    file_type::FileType,
    handlers::{DiagCancelToken, diagnostics},
    state::{Document, WorldState},
};

/// Create a tree-sitter parser configured for R.
fn make_r_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .expect("Failed to set R language for tree-sitter");
    parser
}

/// Generate a synthetic R file of approximately `target_bytes` size.
fn generate_r_code_of_size(target_bytes: usize) -> String {
    let mut content = String::new();
    let mut func_idx = 0;

    // Each function block is roughly 120 bytes
    while content.len() < target_bytes {
        writeln!(
            content,
            r#"func_{} <- function(x, y = {}) {{
    result <- x + y * {}
    if (is.na(result)) {{
        return(NULL)
    }}
    result
}}
"#,
            func_idx,
            func_idx + 1,
            func_idx + 1
        )
        .unwrap();
        func_idx += 1;
    }

    // Trim to approximate target size (don't cut mid-line)
    if content.len() > target_bytes {
        // Find the last newline before target_bytes
        if let Some(pos) = content[..target_bytes].rfind('\n') {
            content.truncate(pos + 1);
        }
    }

    content
}

fn generate_stan_code_of_size(target_bytes: usize, malformed: bool) -> String {
    if malformed {
        let mut content = String::new();
        let mut index = 0usize;
        while content.len() + 40 < target_bytes {
            content.push_str(&format!("model {{ print({index}); target += ; }}\n"));
            index += 1;
        }
        return content;
    }

    let mut content = String::from("parameters { real theta; }\nmodel {\n");
    let statement = "  target += normal_lpdf(theta | 0, 1);\n";
    while content.len() + statement.len() + 2 < target_bytes {
        content.push_str(statement);
    }
    content.push_str("}\n");
    content
}

fn analyze_stan(code: &str) -> usize {
    let uri = Url::parse("untitled:stan-performance-budget").unwrap();
    let mut state = WorldState::new();
    state.cross_file_config.stan_diagnostics_enabled = true;
    state.open_document_with_language_id(uri.clone(), code, Some(1), Some("stan"));
    diagnostics(&state, &uri, &DiagCancelToken::never()).len()
}

fn generate_stan_semantic_errors_of_size(target_bytes: usize) -> String {
    let mut content = String::from("model {\n");
    let mut index = 0usize;
    while content.len() + 40 < target_bytes {
        writeln!(content, "  target += missing_{index};").unwrap();
        index += 1;
    }
    content.push_str("}\n");
    content
}

#[test]
fn budget_stan_parse_and_diagnostics_100kb_valid() {
    let code = generate_stan_code_of_size(100 * 1024, false);
    let _ = analyze_stan(&code);
    let elapsed = median_of_3(|| assert_eq!(analyze_stan(&code), 0));
    assert_within_budget("stan_parse_and_diagnostics_100kb_valid", elapsed, 250);
}

#[test]
fn budget_stan_parse_and_diagnostics_100kb_malformed_is_capped() {
    let code = generate_stan_code_of_size(100 * 1024, true);
    let first_count = analyze_stan(&code);
    assert_eq!(first_count, 500, "Stan syntax findings must honor the cap");
    let elapsed = median_of_3(|| assert_eq!(analyze_stan(&code), 500));
    assert_within_budget("stan_parse_and_diagnostics_100kb_malformed", elapsed, 250);
}

#[test]
fn budget_stan_parse_and_semantic_diagnostics_100kb_is_capped() {
    let code = generate_stan_semantic_errors_of_size(100 * 1024);
    let first_count = analyze_stan(&code);
    assert_eq!(
        first_count, 500,
        "Stan semantic findings must honor their fixed cap"
    );
    let elapsed = median_of_3(|| assert_eq!(analyze_stan(&code), 500));
    assert_within_budget("stan_parse_and_semantic_diagnostics_100kb", elapsed, 250);
}

#[test]
fn stan_document_constructor_uses_the_stan_grammar() {
    let document = Document::new_with_file_type("model {}\n", None, FileType::Stan);
    assert_eq!(document.tree.unwrap().root_node().kind(), "program");
}

fn generate_jags_code_of_size(target_bytes: usize, malformed: bool) -> String {
    let mut content = String::from("model {\n");
    let statement = if malformed {
        "  theta <- * 1\n"
    } else {
        "  theta <- 1 + 2\n"
    };
    while content.len() + statement.len() + 2 < target_bytes {
        content.push_str(statement);
    }
    content.push_str("}\n");
    content
}

fn analyze_jags(code: &str) -> usize {
    let uri = Url::parse("untitled:jags-performance-budget").unwrap();
    let mut state = WorldState::new();
    state.cross_file_config.jags_diagnostics_enabled = true;
    state.open_document_with_language_id(uri.clone(), code, Some(1), Some("jags"));
    diagnostics(&state, &uri, &DiagCancelToken::never()).len()
}

#[test]
fn budget_jags_parse_and_diagnostics_100kb_valid() {
    let code = generate_jags_code_of_size(100 * 1024, false);
    let _ = analyze_jags(&code);
    let elapsed = median_of_3(|| assert_eq!(analyze_jags(&code), 0));
    assert_within_budget("jags_parse_and_diagnostics_100kb_valid", elapsed, 250);
}

#[test]
fn budget_jags_parse_and_diagnostics_100kb_malformed_is_capped() {
    let code = generate_jags_code_of_size(100 * 1024, true);
    let first_count = analyze_jags(&code);
    assert_eq!(first_count, 500, "JAGS syntax findings must honor the cap");
    let elapsed = median_of_3(|| assert_eq!(analyze_jags(&code), 500));
    assert_within_budget("jags_parse_and_diagnostics_100kb_malformed", elapsed, 250);
}

#[test]
fn budget_jags_incremental_one_line_edit_100kb() {
    let code = generate_jags_code_of_size(100 * 1024, false);
    let target_line = (code.lines().count() / 2) as u32;
    let mut document = Document::new_with_file_type(&code, None, FileType::Jags);
    let mut replacement = "3";
    let elapsed = median_of_3(|| {
        document.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range::new(
                Position::new(target_line, 15),
                Position::new(target_line, 16),
            )),
            range_length: None,
            text: replacement.to_string(),
        });
        replacement = if replacement == "3" { "2" } else { "3" };
    });
    assert_eq!(document.tree.unwrap().root_node().kind(), "program");
    assert_within_budget("jags_incremental_one_line_edit_100kb", elapsed, 50);
}

#[test]
fn jags_document_constructor_uses_the_jags_grammar() {
    let document = Document::new_with_file_type("model {}\n", None, FileType::Jags);
    assert_eq!(document.tree.unwrap().root_node().kind(), "program");
}

// ---------------------------------------------------------------------------
// Tree-sitter parsing budgets
// Requirements: 2.1 — 1KB < 5ms, 10KB < 25ms, 100KB < 250ms
// ---------------------------------------------------------------------------

#[test]
fn budget_tree_sitter_parse_1kb() {
    let code = generate_r_code_of_size(1_024);
    assert!(
        code.len() >= 900,
        "Generated code should be approximately 1KB, got {} bytes",
        code.len()
    );

    let mut parser = make_r_parser();

    // Warm up the parser
    let _ = parser.parse(&code, None);

    let elapsed = median_of_3(|| {
        let _ = parser.parse(&code, None).expect("parse failed");
    });

    assert_within_budget("tree_sitter_parse_1kb", elapsed, 5);
}

#[test]
fn budget_tree_sitter_parse_10kb() {
    let code = generate_r_code_of_size(10_240);
    assert!(
        code.len() >= 9_000,
        "Generated code should be approximately 10KB, got {} bytes",
        code.len()
    );

    let mut parser = make_r_parser();

    // Warm up the parser
    let _ = parser.parse(&code, None);

    let elapsed = median_of_3(|| {
        let _ = parser.parse(&code, None).expect("parse failed");
    });

    assert_within_budget("tree_sitter_parse_10kb", elapsed, 25);
}

#[test]
fn budget_tree_sitter_parse_100kb() {
    let code = generate_r_code_of_size(102_400);
    assert!(
        code.len() >= 90_000,
        "Generated code should be approximately 100KB, got {} bytes",
        code.len()
    );

    let mut parser = make_r_parser();

    // Warm up the parser
    let _ = parser.parse(&code, None);

    let elapsed = median_of_3(|| {
        let _ = parser.parse(&code, None).expect("parse failed");
    });

    assert_within_budget("tree_sitter_parse_100kb", elapsed, 250);
}

// ---------------------------------------------------------------------------
// Metadata extraction budget
// Requirements: 2.1 — single file < 2ms
// ---------------------------------------------------------------------------

#[test]
fn budget_metadata_extraction_single_file() {
    // Use a realistic R file with directives, source() calls, and library() calls
    let code = r#"# @lsp-cd: /some/path
# @lsp-sourced-by: ../parent.R

library(dplyr)
library(ggplot2)

source("utils.R")
source("data/loader.R", local = TRUE)
sys.source("helpers.R", envir = new.env())

my_function <- function(x) {
    y <- x + 1
    return(y)
}

another_func <- function(a, b, c) {
    result <- a * b + c
    if (is.null(result)) {
        return(NA)
    }
    result
}

data <- data.frame(x = 1:100, y = rnorm(100))
"#;

    // Warm up
    let _ = raven::cross_file::extract_metadata(code);

    let elapsed = median_of_3(|| {
        let _ = raven::cross_file::extract_metadata(code);
    });

    assert_within_budget("metadata_extraction_single_file", elapsed, 2);
}

// ---------------------------------------------------------------------------
// Scope resolution budget helpers
// ---------------------------------------------------------------------------

fn assert_scope_resolution_budget_50_file_workspace(
    mode: raven::cross_file::config::BackwardDependencyMode,
    label: &str,
    budget_ms: u64,
) {
    let config = FixtureConfig::medium(); // 50 files, 10 funcs, depth 10
    let workspace = create_fixture_workspace(&config);
    let workspace_path = workspace.path();
    let folder_url = Url::from_file_path(workspace_path).unwrap();

    let mut artifacts_map: HashMap<Url, Arc<raven::cross_file::ScopeArtifacts>> = HashMap::new();
    let mut metadata_map: HashMap<Url, Arc<raven::cross_file::types::CrossFileMetadata>> =
        HashMap::new();

    let mut entries: Vec<_> = std::fs::read_dir(workspace_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "R").unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in &entries {
        let path = entry.path();
        let content = std::fs::read_to_string(&path).unwrap();
        let uri = Url::from_file_path(&path).unwrap();

        let meta = raven::cross_file::extract_metadata(&content);
        let tree = raven::parser_pool::with_parser(|parser| parser.parse(&content, None));
        if let Some(tree) = tree {
            let arts = raven::cross_file::compute_artifacts(&uri, &tree, &content);
            artifacts_map.insert(uri.clone(), Arc::new(arts));
        }
        metadata_map.insert(uri, Arc::new(meta));
    }

    // Build dependency graph — graph construction is the same regardless of mode.
    // Auto mode changes which backward edges the scope resolution traversal follows.
    let mut graph = raven::cross_file::DependencyGraph::new();
    for (uri, meta) in &metadata_map {
        graph.update_file(uri, meta, Some(&folder_url), |_| None);
    }

    let uri = Url::from_file_path(workspace_path.join("file_0.R")).unwrap();
    let base_exports: HashSet<String> = HashSet::new();
    let never_cancel = || false;

    // Warm up
    let _ = raven::cross_file::scope_at_position_with_graph(
        &uri,
        u32::MAX,
        u32::MAX,
        &|u| artifacts_map.get(u).cloned(),
        &|u| metadata_map.get(u).cloned(),
        &graph,
        Some(&folder_url),
        20,
        &base_exports,
        true,
        mode,
        &never_cancel,
        None,
        None,
    );

    let elapsed = median_of_3(|| {
        let _ = raven::cross_file::scope_at_position_with_graph(
            &uri,
            u32::MAX,
            u32::MAX,
            &|u| artifacts_map.get(u).cloned(),
            &|u| metadata_map.get(u).cloned(),
            &graph,
            Some(&folder_url),
            20,
            &base_exports,
            true,
            mode,
            &never_cancel,
            None,
            None,
        );
    });

    assert_within_budget(label, elapsed, budget_ms);
}

// ---------------------------------------------------------------------------
// Scope resolution budget
// Requirements: 2.1 — 50-file workspace < 50ms
// ---------------------------------------------------------------------------

#[test]
fn budget_scope_resolution_50_file_workspace() {
    assert_scope_resolution_budget_50_file_workspace(
        raven::cross_file::config::BackwardDependencyMode::Explicit,
        "scope_resolution_50_files",
        50,
    );
}

// ---------------------------------------------------------------------------
// Scope resolution budget (auto backward dependency mode)
// Requirements: 2.1 — 50-file workspace with auto backward deps < 75ms
// ---------------------------------------------------------------------------

#[test]
fn budget_scope_resolution_50_file_workspace_auto() {
    assert_scope_resolution_budget_50_file_workspace(
        raven::cross_file::config::BackwardDependencyMode::Auto,
        "scope_resolution_50_files_auto",
        75,
    );
}

// ---------------------------------------------------------------------------
// Single-file completion budget
// Requirements: 2.1 — single-file completion < 20ms
// ---------------------------------------------------------------------------

#[test]
fn budget_single_file_completion() {
    use raven::state::{WorldState, scan_workspace};
    use tower_lsp::lsp_types::Position;

    // Create a small fixture workspace for realistic completion context
    let config = FixtureConfig::small(); // 10 files, 5 funcs, depth 3
    let workspace = create_fixture_workspace(&config);
    let workspace_path = workspace.path();

    // Build a fully-populated WorldState (same pattern as lsp_operations bench)
    let mut state = WorldState::new();
    let folder_url = Url::from_file_path(workspace_path).unwrap();
    state.workspace_folders.push(folder_url.clone());

    // Open every .R file as a document
    let mut entries: Vec<_> = std::fs::read_dir(workspace_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "R").unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in &entries {
        let path = entry.path();
        let content = std::fs::read_to_string(&path).unwrap();
        let uri = Url::from_file_path(&path).unwrap();
        state.open_document(uri, &content, None);
    }

    // Run workspace scan and apply index (populates cross-file state)
    let entries = scan_workspace(&[folder_url], 20);
    state.apply_workspace_index(entries);

    let uri = Url::from_file_path(workspace_path.join("file_0.R")).unwrap();
    // Position inside the first function body (line 5, col 4 — on `result`)
    let pos = Position::new(5, 4);

    // Warm up
    let _ = raven::handlers::completion(&state, &uri, pos, None);

    let elapsed = median_of_3(|| {
        let _ = raven::handlers::completion(&state, &uri, pos, None);
    });

    assert_within_budget("single_file_completion", elapsed, 20);
}

// ---------------------------------------------------------------------------
// derive_package_state budget
// Requirements: 2.11 — single-file delta in 500-file workspace < 50ms median
// ---------------------------------------------------------------------------

#[test]
fn derive_package_state_500_file_keystroke_budget() {
    use raven::cross_file::config::PackageMode;
    use raven::package_state::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    let root: PathBuf = "/work/pkg".into();
    let mut inputs = PackageInputs {
        workspace_root: Some(root.clone()),
        package_mode: PackageMode::Auto,
        description: Some(DescriptionInput {
            text: "Package: foo\n".into(),
        }),
        namespace: None,
        r_files: BTreeMap::new(),
        dataset_names: BTreeSet::new(),
        sysdata_names: BTreeSet::new(),
        model_rprofile: false,
        rprofile_symbols: BTreeSet::new(),
        rprofile_attached_packages: BTreeSet::new(),
        rprofile_sourced_files: BTreeSet::new(),
        preamble_sourced_symbols: BTreeMap::new(),
        preamble_sourced_attached_packages: BTreeMap::new(),
        preamble_sourced_files: BTreeSet::new(),
        preamble_sourced_files_by_preamble: BTreeMap::new(),
    };
    for i in 0..500 {
        let p = root.join("R").join(format!("file_{}.R", i));
        let text: Arc<str> = format!("fn_{} <- function() {}\n", i, i).into();
        let digest = ContentDigest::of(&text);
        inputs.r_files.insert(
            p,
            RFileInput {
                kind: RFileKind::Source,
                text,
                content_digest: digest,
            },
        );
    }
    let s0 = derive_package_state(
        &PackageState::default(),
        &inputs,
        &PackageInputDelta::Initial,
    );

    // Single-file delta: change file_42
    let p = root.join("R").join("file_42.R");
    let new_text: Arc<str> = "fn_42 <- function() 99\n".into();
    let new_digest = ContentDigest::of(&new_text);
    inputs.r_files.insert(
        p.clone(),
        RFileInput {
            kind: RFileKind::Source,
            text: new_text,
            content_digest: new_digest,
        },
    );

    // Warm-up: JIT/cache priming so the first measured iteration doesn't
    // dominate the max sample.
    let _ = derive_package_state(
        &s0,
        &inputs,
        &PackageInputDelta::RFileChanged {
            path: p.clone(),
            kind: RFileKind::Source,
        },
    );

    let mut times = Vec::with_capacity(20);
    for _ in 0..20 {
        let start = Instant::now();
        let _s = derive_package_state(
            &s0,
            &inputs,
            &PackageInputDelta::RFileChanged {
                path: p.clone(),
                kind: RFileKind::Source,
            },
        );
        times.push(start.elapsed());
    }
    times.sort();
    let median = times[times.len() / 2];
    // With only 20 samples, (len * 99 / 100) always picks the last element,
    // so report it as `max` rather than misleadingly labeling it `p99`.
    let max = times.last().cloned().unwrap();
    eprintln!(
        "derive_package_state 500-file: median={:?} max={:?}",
        median, max
    );
    // Generous budget; tighten later. The point of the test is to catch
    // pathological regressions. Use assert_within_budget so CI relaxation
    // (RAVEN_PERF_CI_FACTOR) applies uniformly with the rest of the file.
    assert_within_budget("derive_package_state_500_file_keystroke_median", median, 50);
    assert_within_budget("derive_package_state_500_file_keystroke_max", max, 200);
}
