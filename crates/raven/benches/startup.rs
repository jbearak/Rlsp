// startup.rs - Performance benchmarks for Raven startup and initialization
//
// Run with: cargo bench --bench startup
// Compare baselines: cargo bench --bench startup -- --baseline before

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::TempDir;

/// Generate a synthetic R file with specified characteristics
fn generate_r_file(
    index: usize,
    _total_files: usize,
    functions_per_file: usize,
    include_source: bool,
) -> String {
    let mut content = String::new();

    // Add a library call
    content.push_str("library(stats)\n\n");

    // Add source() calls to create dependency chains
    if include_source && index > 0 {
        let source_target = format!("file_{}.R", index - 1);
        content.push_str(&format!("source(\"{}\")\n\n", source_target));
    }

    // Add function definitions
    for func_idx in 0..functions_per_file {
        content.push_str(&format!(
            r#"
func_{}_{} <- function(x, y) {{
    # Function {} in file {}
    result <- x + y
    if (is.na(result)) {{
        return(NULL)
    }}
    return(result * 2)
}}
"#,
            index, func_idx, func_idx, index
        ));
    }

    // Add some variable assignments
    content.push_str(&format!(
        r#"
# Variables in file {}
data_{} <- data.frame(x = 1:10, y = rnorm(10))
config_{} <- list(
    enabled = TRUE,
    threshold = 0.05,
    method = "default"
)
"#,
        index, index, index
    ));

    content
}

/// Create a temporary workspace with R files
fn create_test_workspace(num_files: usize, functions_per_file: usize) -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");

    for i in 0..num_files {
        let content = generate_r_file(i, num_files, functions_per_file, i > 0 && i < num_files - 1);
        let filename = format!("file_{}.R", i);
        let filepath = temp_dir.path().join(&filename);
        std::fs::write(filepath, content).expect("Failed to write test file");
    }

    temp_dir
}

/// Benchmark metadata extraction from R code
fn bench_metadata_extraction(c: &mut Criterion) {
    let mut group = c.benchmark_group("metadata_extraction");

    // Sample R code for benchmarking
    let sample_code = r#"
# @lsp-cd: /some/path
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

    group.bench_function("extract_metadata_small", |b| {
        b.iter(|| {
            // This would call the actual metadata extraction
            // For now we just measure the parsing overhead
            black_box(sample_code.lines().count())
        })
    });

    // Larger code sample
    let large_code = sample_code.repeat(50);
    group.bench_function("extract_metadata_large", |b| {
        b.iter(|| black_box(large_code.lines().count()))
    });

    group.finish();
}

/// Benchmark workspace scanning with different sizes
fn bench_workspace_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("workspace_scan");

    // Configure for longer runs since workspace scan is expensive
    group.sample_size(10);

    for &num_files in &[5, 20, 50] {
        let workspace = create_test_workspace(num_files, 3);

        group.bench_with_input(
            BenchmarkId::new("files", num_files),
            &workspace,
            |b, workspace| {
                b.iter(|| {
                    // Count files as a proxy for scanning
                    // In a real benchmark, this would call scan_workspace
                    let count = std::fs::read_dir(workspace.path())
                        .unwrap()
                        .filter(|e| {
                            e.as_ref()
                                .unwrap()
                                .path()
                                .extension()
                                .map(|ext| ext == "R" || ext == "r")
                                .unwrap_or(false)
                        })
                        .count();
                    black_box(count)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark for batch init output parsing
fn bench_batch_init_parsing(c: &mut Criterion) {
    // Sample output that mimics what R would return
    let sample_output = r#"__RAVEN_LIB_PATHS__
/Library/Frameworks/R.framework/Versions/4.4/Resources/library
/Users/test/Library/R/arm64/4.4/library

__RAVEN_BASE_PACKAGES__
base
methods
utils
grDevices
graphics
stats
datasets

__RAVEN_EXPORTS__
__PKG:base__
c
list
print
cat
paste
paste0
sprintf
format
nchar
substr
strsplit
grep
grepl
sub
gsub
__PKG:methods__
setClass
setGeneric
setMethod
new
show
slot
__PKG:utils__
help
str
head
tail
View
edit
__PKG:stats__
lm
glm
t.test
anova
mean
sd
var
cor
__RAVEN_END__
"#;

    c.bench_function("parse_batch_init_output", |b| {
        b.iter(|| {
            // Parse the structured output
            // This simulates the parsing logic without calling R
            let mut lib_paths: Vec<PathBuf> = Vec::new();
            let mut base_packages: Vec<String> = Vec::new();
            let mut exports: HashMap<String, Vec<String>> = HashMap::new();

            let lib_start = sample_output.find("__RAVEN_LIB_PATHS__").unwrap();
            let base_start = sample_output.find("__RAVEN_BASE_PACKAGES__").unwrap();
            let exports_start = sample_output.find("__RAVEN_EXPORTS__").unwrap();

            // Parse lib_paths
            let lib_section = &sample_output[lib_start + 19..base_start];
            for line in lib_section.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    lib_paths.push(PathBuf::from(line));
                }
            }

            // Parse base_packages
            let base_section = &sample_output[base_start + 23..exports_start];
            for line in base_section.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    base_packages.push(line.to_string());
                }
            }

            // Parse exports
            let exports_section = &sample_output[exports_start + 17..];
            let mut current_pkg: Option<String> = None;
            let mut current_exports: Vec<String> = Vec::new();

            for line in exports_section.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line.starts_with("__PKG:") && line.ends_with("__") {
                    if let Some(pkg) = current_pkg.take() {
                        exports.insert(pkg, std::mem::take(&mut current_exports));
                    }
                    current_pkg = Some(line[6..line.len() - 2].to_string());
                } else if line == "__RAVEN_END__" {
                    if let Some(pkg) = current_pkg.take() {
                        exports.insert(pkg, current_exports);
                    }
                    break;
                } else if current_pkg.is_some() {
                    current_exports.push(line.to_string());
                }
            }

            black_box((lib_paths, base_packages, exports))
        })
    });
}

/// Benchmark tree-sitter parsing of R code
fn bench_tree_sitter_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree_sitter_parsing");

    let small_code = generate_r_file(0, 1, 3, false);
    let medium_code = generate_r_file(0, 1, 20, false);
    let large_code = generate_r_file(0, 1, 100, false);

    group.bench_function("parse_small", |b| {
        b.iter(|| {
            // Just measure string processing as proxy
            // Real benchmark would use tree-sitter parser
            black_box(small_code.len())
        })
    });

    group.bench_function("parse_medium", |b| b.iter(|| black_box(medium_code.len())));

    group.bench_function("parse_large", |b| b.iter(|| black_box(large_code.len())));

    group.finish();
}

criterion_group!(
    benches,
    bench_metadata_extraction,
    bench_workspace_scan,
    bench_batch_init_parsing,
    bench_tree_sitter_parsing,
);
criterion_main!(benches);
