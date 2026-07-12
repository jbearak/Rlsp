//! End-to-end benchmark for judge-backed on-type indentation.
//!
//! Run with:
//! `cargo bench -p raven --bench indentation --features test-support`

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use raven::indentation::{IndentationConfig, on_type_indentation};
use raven::linting::{InfixContinuationStyle, LintConfig, run_lints};
use tower_lsp::lsp_types::Position;

/// Build 10,000 lines containing calls, pipe chains, and nested braces.
fn synthetic_r_document() -> String {
    let mut source = String::with_capacity(400_000);
    for i in 0..1_000 {
        source.push_str(&format!(
            "f_{i} <- function(x) {{\n\
             \x20\x20result <- x |>\n\
             \x20\x20\x20\x20transform(\n\
             \x20\x20\x20\x20\x20\x20y = x + {i}\n\
             \x20\x20\x20\x20) |>\n\
             \x20\x20\x20\x20identity()\n\
             \x20\x20if (result > 0) {{\n\
             \x20\x20\x20\x20result\n\
             \x20\x20}}\n\
             }}\n"
        ));
    }
    assert_eq!(source.lines().count(), 10_000);
    source
}

/// Build 10,000 lines whose continuations use the aligned-only infix form, so
/// the default `Either` lint exercises both strict folds rather than its clean
/// `Indented` fast path.
fn synthetic_aligned_chain_document() -> String {
    let mut source = String::with_capacity(300_000);
    for i in 0..5_000 {
        let prefix = format!("value_{i} <- ");
        source.push_str(&prefix);
        source.push_str("input |>\n");
        source.push_str(&" ".repeat(prefix.len()));
        source.push_str("identity()\n");
    }
    assert_eq!(source.lines().count(), 10_000);
    source
}

fn indentation_only_lint_config() -> LintConfig {
    let mut config = LintConfig::default();
    config.enabled = true;
    config.line_length_severity = None;
    config.trailing_whitespace_severity = None;
    config.no_tab_severity = None;
    config.trailing_blank_lines_severity = None;
    config.assignment_operator_severity = None;
    config.object_name_severity = None;
    config.infix_spaces_severity = None;
    config.commented_code_severity = None;
    config.quotes_severity = None;
    config.commas_severity = None;
    config.t_and_f_symbol_severity = None;
    config.semicolon_severity = None;
    config.equals_na_severity = None;
    config.object_length_severity = None;
    config.vector_logic_severity = None;
    config.function_left_parentheses_severity = None;
    config.spaces_inside_severity = None;
    config
}

fn bench_on_type_indentation(c: &mut Criterion) {
    let source = synthetic_r_document();
    let tree = raven::parser_pool::with_parser(|parser| parser.parse(&source, None))
        .expect("benchmark source must parse");
    let config = IndentationConfig::default();
    let position = Position::new(10_000, 0);

    let mut group = c.benchmark_group("on_type_indentation");
    group.bench_function("enter_bottom_10000_lines", |b| {
        b.iter(|| {
            black_box(on_type_indentation(
                black_box(&tree),
                black_box(&source),
                black_box(position),
                black_box(&config),
            ))
        })
    });
    group.finish();
}

fn bench_indentation_lint(c: &mut Criterion) {
    let indented = synthetic_r_document();
    let indented_tree = raven::parser_pool::with_parser(|parser| parser.parse(&indented, None))
        .expect("benchmark source must parse");
    let aligned = synthetic_aligned_chain_document();
    let aligned_tree = raven::parser_pool::with_parser(|parser| parser.parse(&aligned, None))
        .expect("benchmark source must parse");
    let config = indentation_only_lint_config();
    let mut strict_indented = config.clone();
    strict_indented.infix_continuation_style = InfixContinuationStyle::Indented;
    let mut strict_aligned = config.clone();
    strict_aligned.infix_continuation_style = InfixContinuationStyle::Aligned;

    assert!(
        run_lints(&indented, indented_tree.root_node(), &strict_indented).is_empty(),
        "indented benchmark fixture must be clean under strict Indented"
    );
    assert!(
        !run_lints(&aligned, aligned_tree.root_node(), &strict_indented).is_empty(),
        "aligned benchmark fixture must exercise Either's second fold"
    );
    assert!(
        run_lints(&aligned, aligned_tree.root_node(), &strict_aligned).is_empty(),
        "aligned benchmark fixture must be clean under strict Aligned"
    );
    assert!(
        run_lints(&indented, indented_tree.root_node(), &config).is_empty()
            && run_lints(&aligned, aligned_tree.root_node(), &config).is_empty(),
        "both benchmark fixtures must be clean under default Either"
    );

    let mut group = c.benchmark_group("indentation_lint");
    group.bench_function("default_either_indented_10000_lines", |b| {
        b.iter(|| {
            black_box(run_lints(
                black_box(&indented),
                black_box(indented_tree.root_node()),
                black_box(&config),
            ))
        })
    });
    group.bench_function("default_either_aligned_10000_lines", |b| {
        b.iter(|| {
            black_box(run_lints(
                black_box(&aligned),
                black_box(aligned_tree.root_node()),
                black_box(&config),
            ))
        })
    });
    group.finish();
}

criterion_group!(benches, bench_on_type_indentation, bench_indentation_lint);
criterion_main!(benches);
