#![cfg(not(debug_assertions))]

use std::time::{Duration, Instant};

use tree_sitter::{InputEdit, Parser, Point};

fn parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_jags::LANGUAGE.into())
        .expect("load JAGS grammar");
    parser
}

fn valid_model_at_least(target_bytes: usize) -> String {
    let mut source = String::with_capacity(target_bytes + 64);
    source.push_str("model {\n");
    let mut index = 0;
    while source.len() < target_bytes {
        source.push_str(&format!("  x_{index} <- {index} + 2 * 3;\n"));
        index += 1;
    }
    source.push_str("}\n");
    source
}

fn malformed_model_at_least(target_bytes: usize) -> String {
    let mut source = String::with_capacity(target_bytes + 64);
    source.push_str("model {\n");
    let mut index = 0;
    while source.len() < target_bytes {
        if index % 17 == 0 {
            source.push_str(&format!("  broken_{index} <- f(1, 2;\n"));
        } else {
            source.push_str(&format!("  x_{index} <- {index} + 2 * 3;\n"));
        }
        index += 1;
    }
    source.push_str("}\n");
    source
}

fn median_full_parse(source: &str) -> Duration {
    let mut samples = Vec::new();
    let mut parser = parser();
    for _ in 0..3 {
        let start = Instant::now();
        let tree = parser.parse(source, None).expect("parse completes");
        samples.push(start.elapsed());
        std::hint::black_box(tree);
    }
    samples.sort_unstable();
    samples[1]
}

fn budget(base: Duration) -> Duration {
    if std::env::var_os("CI").is_some() {
        base.saturating_mul(3)
    } else {
        base
    }
}

#[test]
#[ignore = "release-mode performance budget; run explicitly before publishing"]
fn full_parse_budgets() {
    let cases = [
        (
            "1 KiB valid",
            valid_model_at_least(1 << 10),
            Duration::from_millis(5),
        ),
        (
            "10 KiB valid",
            valid_model_at_least(10 << 10),
            Duration::from_millis(25),
        ),
        (
            "100 KiB valid",
            valid_model_at_least(100 << 10),
            Duration::from_millis(250),
        ),
        (
            "100 KiB malformed",
            malformed_model_at_least(100 << 10),
            Duration::from_millis(250),
        ),
        (
            "1 MiB valid",
            valid_model_at_least(1 << 20),
            Duration::from_secs(2),
        ),
    ];

    for (name, source, limit) in cases {
        let elapsed = median_full_parse(&source);
        eprintln!("{name}: {elapsed:?}, {} bytes", source.len());
        assert!(
            elapsed < budget(limit),
            "{name}: {elapsed:?} >= {:?}",
            budget(limit)
        );
    }
}

#[test]
#[ignore = "release-mode performance budget; run explicitly before publishing"]
fn incremental_100_kib_budget() {
    let source = valid_model_at_least(100 << 10);
    let edit_at = source.find("0 + 2").expect("first literal");
    let mut edited = source.clone();
    edited.replace_range(edit_at..edit_at + 1, "9");

    let mut samples = Vec::new();
    for _ in 0..3 {
        let mut parser = parser();
        let mut old_tree = parser.parse(&source, None).expect("initial parse");
        let point = point_for(&source, edit_at);
        old_tree.edit(&InputEdit {
            start_byte: edit_at,
            old_end_byte: edit_at + 1,
            new_end_byte: edit_at + 1,
            start_position: point,
            old_end_position: Point::new(point.row, point.column + 1),
            new_end_position: Point::new(point.row, point.column + 1),
        });
        let start = Instant::now();
        let tree = parser
            .parse(&edited, Some(&old_tree))
            .expect("incremental parse");
        samples.push(start.elapsed());
        std::hint::black_box(tree);
    }
    samples.sort_unstable();
    let elapsed = samples[1];
    eprintln!("100 KiB one-byte incremental edit: {elapsed:?}");
    assert!(
        elapsed < budget(Duration::from_millis(50)),
        "incremental parse: {elapsed:?}"
    );
}

fn point_for(source: &str, byte: usize) -> Point {
    let prefix = &source[..byte];
    let row = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let column = prefix
        .rfind('\n')
        .map_or(prefix.len(), |newline| prefix.len() - newline - 1);
    Point::new(row, column)
}
