#![no_main]

use libfuzzer_sys::fuzz_target;
use tree_sitter::{InputEdit, Parser, Point};

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let source_len = data.len() / 2;
    let mut source = data[..source_len].to_vec();
    let replacement = &data[source_len..];
    let start = usize::from(data[0]) % (source.len() + 1);
    let available = source.len() - start;
    let delete_len = usize::from(data[1]) % (available + 1);
    let end = start + delete_len;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_jags::LANGUAGE.into())
        .expect("load JAGS grammar");
    let Some(mut old_tree) = parser.parse(&source, None) else {
        return;
    };

    let start_position = point_for(&source, start);
    let old_end_position = point_for(&source, end);
    source.splice(start..end, replacement.iter().copied());
    let new_end = start + replacement.len();
    let new_end_position = point_for(&source, new_end);
    old_tree.edit(&InputEdit {
        start_byte: start,
        old_end_byte: end,
        new_end_byte: new_end,
        start_position,
        old_end_position,
        new_end_position,
    });

    let incremental = parser.parse(&source, Some(&old_tree));
    let fresh = parser.parse(&source, None);
    if let (Some(incremental), Some(fresh)) = (incremental, fresh) {
        assert_eq!(
            incremental.root_node().to_sexp(),
            fresh.root_node().to_sexp()
        );
    }
});

fn point_for(source: &[u8], byte: usize) -> Point {
    let prefix = &source[..byte];
    let row = prefix.iter().filter(|value| **value == b'\n').count();
    let column = prefix
        .iter()
        .rposition(|value| *value == b'\n')
        .map_or(prefix.len(), |newline| prefix.len() - newline - 1);
    Point::new(row, column)
}
