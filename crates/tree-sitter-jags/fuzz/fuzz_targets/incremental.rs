#![no_main]

use libfuzzer_sys::fuzz_target;
use tree_sitter::{InputEdit, Node, Parser, Point};

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    const VALID_BASES: &[&[u8]] = &[
        b"model { x <- 1 + 2 }\n",
        b"model { x <- f(1, 2) }\n",
        b"model { x <- values[i,j] }\n",
        b"model { for (i in 1:n) { x[i] <- i } }\n",
        b"model { x ~ dfoo() T(,1) }\n",
        b"data { n <- 2 } model { x <- n }\n",
        b"model { logit(p[i]) <- alpha + beta*x[i] }\n",
        b"# heading\r\nmodel {\r\n x <- 1\r\n}\r\n",
    ];
    let mut source = VALID_BASES[usize::from(data[0]) % VALID_BASES.len()].to_vec();
    let replacement = &data[3..];
    let start = usize::from(data[1]) % (source.len() + 1);
    let available = source.len() - start;
    let delete_len = usize::from(data[2]) % (available + 1);
    let end = start + delete_len;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_jags::LANGUAGE.into())
        .expect("load JAGS grammar");
    let Some(mut old_tree) = parser.parse(&source, None) else {
        return;
    };
    assert!(!old_tree.root_node().has_error());

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
        if incremental.root_node().to_sexp() != fresh.root_node().to_sexp() {
            eprintln!(
                "edit={start}..{end} replacement_len={} source={source:?}\nincremental={}\nfresh={}",
                replacement.len(),
                incremental.root_node().to_sexp(),
                fresh.root_node().to_sexp(),
            );
        }
        assert_nodes_identical(incremental.root_node(), fresh.root_node());
    }
});

fn assert_nodes_identical(incremental: Node<'_>, fresh: Node<'_>) {
    assert_eq!(incremental.kind(), fresh.kind());
    assert_eq!(incremental.is_named(), fresh.is_named());
    assert_eq!(incremental.is_missing(), fresh.is_missing());
    assert_eq!(incremental.is_error(), fresh.is_error());
    assert_eq!(incremental.has_error(), fresh.has_error());
    assert_eq!(incremental.byte_range(), fresh.byte_range());
    assert_eq!(incremental.range(), fresh.range());
    assert_eq!(incremental.child_count(), fresh.child_count());
    for index in 0..incremental.child_count() {
        assert_eq!(
            incremental.field_name_for_child(index as u32),
            fresh.field_name_for_child(index as u32)
        );
        assert_nodes_identical(
            incremental.child(index as u32).expect("incremental child"),
            fresh.child(index as u32).expect("fresh child"),
        );
    }
}

fn point_for(source: &[u8], byte: usize) -> Point {
    let prefix = &source[..byte];
    let row = prefix.iter().filter(|value| **value == b'\n').count();
    let column = prefix
        .iter()
        .rposition(|value| *value == b'\n')
        .map_or(prefix.len(), |newline| prefix.len() - newline - 1);
    Point::new(row, column)
}
