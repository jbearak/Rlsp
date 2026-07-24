use std::path::Path;

use serde_json::Value;
use tree_sitter::Parser;

fn parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_jags::LANGUAGE.into())
        .expect("load JAGS grammar");
    parser
}

#[test]
fn matches_black_box_syntax_matrix() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("oracle")
        .join("syntax-matrix.json");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read syntax matrix"))
            .expect("parse syntax matrix");

    let mut parser = parser();
    let mut mismatches = Vec::new();
    for probe in manifest["probes"].as_array().expect("probe array") {
        let name = probe["name"].as_str().expect("probe name");
        let source = probe["source"].as_str().expect("probe source");
        let expected_acceptance = probe["expect_parse"] == "accepted";
        let source = if probe["encoding"] == "utf-8-bom" {
            format!("\u{feff}{source}")
        } else {
            source.to_owned()
        };
        let tree = parser.parse(&source, None).expect("parse completes");
        // Tree-sitter's lexer consumes a leading UTF-8 BOM before invoking any
        // grammar. JAGS rejects that byte sequence. Requiring the root to cover
        // the complete input makes the differential classifier preserve the
        // JAGS result without pretending the grammar can override core lexer
        // normalization.
        let root = tree.root_node();
        let accepted =
            !root.has_error() && root.start_byte() == 0 && root.end_byte() == source.len();
        if accepted != expected_acceptance {
            mismatches.push(format!(
                "{name}: JAGS={}, tree-sitter={}, root_bytes={:?}\n{}",
                if expected_acceptance {
                    "accepted"
                } else {
                    "rejected"
                },
                if accepted { "accepted" } else { "rejected" },
                root.byte_range(),
                root.to_sexp(),
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "black-box syntax mismatches:\n{}",
        mismatches.join("\n")
    );
}

#[test]
fn unknown_semantic_names_have_clean_trees() {
    let mut parser = parser();
    for source in [
        "model { x ~ package_distribution(theta) }",
        "model { x <- package_function(argument) }",
    ] {
        let tree = parser.parse(source, None).expect("parse completes");
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
    }
}
