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
    for _ in 0..5 {
        let start = Instant::now();
        let tree = parser.parse(source, None).expect("parse completes");
        samples.push(start.elapsed());
        std::hint::black_box(tree);
    }
    samples.sort_unstable();
    samples[2]
}

#[test]
#[ignore = "release-mode performance budget; run explicitly before publishing"]
fn full_parse_budgets() {
    let cases = [
        (
            "1 KiB valid",
            valid_model_at_least(1 << 10),
            Duration::from_millis(3),
        ),
        (
            "10 KiB valid",
            valid_model_at_least(10 << 10),
            Duration::from_millis(8),
        ),
        (
            "100 KiB valid",
            valid_model_at_least(100 << 10),
            Duration::from_millis(30),
        ),
        (
            "100 KiB malformed",
            malformed_model_at_least(100 << 10),
            Duration::from_millis(40),
        ),
        (
            "1 MiB valid",
            valid_model_at_least(1 << 20),
            Duration::from_millis(300),
        ),
    ];

    for (name, source, limit) in cases {
        let elapsed = median_full_parse(&source);
        eprintln!("{name}: {elapsed:?}, {} bytes", source.len());
        assert!(elapsed < limit, "{name}: {elapsed:?} >= {limit:?}");
    }
}

#[test]
#[ignore = "release-mode performance budget; run explicitly before publishing"]
fn incremental_100_kib_budget_at_multiple_locations_and_shapes() {
    let source = valid_model_at_least(100 << 10);
    let midpoint = source.len() / 2;
    let middle_line = source[midpoint..]
        .find(" + 2 * 3")
        .map(|offset| midpoint + offset)
        .expect("middle expression");
    let late_line = source.rfind(" + 2 * 3").expect("last complete expression");
    let edits = [
        (
            "early one-byte replacement",
            source.find("0 + 2").expect("first literal"),
            1,
            "9",
        ),
        (
            "middle expression replacement",
            middle_line,
            " + 2 * 3".len(),
            " + f(2, 3)",
        ),
        (
            "late expression insertion",
            late_line + " + 2 * 3".len(),
            0,
            " + 4",
        ),
    ];

    for (name, edit_at, delete_len, replacement) in edits {
        let mut edited = source.clone();
        edited.replace_range(edit_at..edit_at + delete_len, replacement);
        let mut samples = Vec::new();
        for _ in 0..5 {
            let mut parser = parser();
            let mut old_tree = parser.parse(&source, None).expect("initial parse");
            let start_position = point_for(&source, edit_at);
            let old_end_position = point_for(&source, edit_at + delete_len);
            let new_end_position = point_for(&edited, edit_at + replacement.len());
            old_tree.edit(&InputEdit {
                start_byte: edit_at,
                old_end_byte: edit_at + delete_len,
                new_end_byte: edit_at + replacement.len(),
                start_position,
                old_end_position,
                new_end_position,
            });
            let start = Instant::now();
            let tree = parser
                .parse(&edited, Some(&old_tree))
                .expect("incremental parse");
            samples.push(start.elapsed());
            std::hint::black_box(tree);
        }
        samples.sort_unstable();
        let elapsed = samples[2];
        eprintln!("100 KiB {name}: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(20), "{name}: {elapsed:?}");
    }
}

fn point_for(source: &str, byte: usize) -> Point {
    let prefix = &source[..byte];
    let row = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let column = prefix
        .rfind('\n')
        .map_or(prefix.len(), |newline| prefix.len() - newline - 1);
    Point::new(row, column)
}
