#![no_main]

use libfuzzer_sys::fuzz_target;
use tree_sitter::{Node, Parser};

fuzz_target!(|source: &[u8]| {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_jags::LANGUAGE.into())
        .expect("load JAGS grammar");
    if let Some(tree) = parser.parse(source, None) {
        assert_ranges(tree.root_node(), source.len());
    }
});

fn assert_ranges(node: Node<'_>, source_len: usize) {
    assert!(node.start_byte() <= node.end_byte());
    assert!(node.end_byte() <= source_len);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        assert_ranges(child, source_len);
    }
}
