use tree_sitter::Parser;

fn main() {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_r::LANGUAGE.into())
        .expect("Failed to set language");

    let code = r#"x <- readLines("file.txt", n = 1)"#;
    let tree = parser.parse(code, None).expect("Failed to parse");

    eprintln!("Code: {:?}", code);
    eprintln!("Tree: {}", tree.root_node().to_sexp());

    fn print_node(node: tree_sitter::Node, text: &str, depth: usize) {
        let indent = "  ".repeat(depth);
        eprintln!(
            "{}kind: '{}', text: '{}'",
            indent,
            node.kind(),
            &text[node.byte_range()]
        );

        if node.kind() == "argument" {
            if let Some(name) = node.child_by_field_name("name") {
                eprintln!(
                    "{}  -> has name field: '{}'",
                    indent,
                    &text[name.byte_range()]
                );
            } else {
                eprintln!("{}  -> NO name field", indent);
            }
            if let Some(value) = node.child_by_field_name("value") {
                eprintln!(
                    "{}  -> has value field: '{}'",
                    indent,
                    &text[value.byte_range()]
                );
            }
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                print_node(child, text, depth + 1);
            }
        }
    }

    print_node(tree.root_node(), code, 0);
}
