//! End-to-end benchmark for judge-backed on-type indentation.
//!
//! Run with:
//! `cargo bench -p raven --bench indentation --features test-support`

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use raven::indentation::{IndentationConfig, on_type_indentation};
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

criterion_group!(benches, bench_on_type_indentation);
criterion_main!(benches);
