use std::collections::{BTreeMap, BTreeSet};
use std::ops::{ControlFlow, Range};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;
use tree_sitter::{InputEdit, Node, ParseOptions, Parser, Point, Tree};

#[derive(Debug, Clone)]
struct CorpusCase {
    id: String,
    group: String,
    family: String,
    template: String,
    source: String,
    expect_parse: String,
}

fn parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_jags::LANGUAGE.into())
        .expect("load JAGS grammar");
    parser
}

fn parse(parser: &mut Parser, source: &str, old_tree: Option<&Tree>) -> Tree {
    parser.parse(source, old_tree).expect("parse completes")
}

fn is_clean(source: &str, tree: &Tree) -> bool {
    let root = tree.root_node();
    !root.has_error() && root.start_byte() == 0 && root.end_byte() == source.len()
}

fn oracle_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("oracle")
}

fn load_json(name: &str) -> Value {
    let path = oracle_dir().join(name);
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|error| {
        panic!("read {}: {error}", path.display());
    }))
    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn quality_cases() -> Vec<CorpusCase> {
    load_json("quality-corpus.json")["cases"]
        .as_array()
        .expect("quality corpus cases")
        .iter()
        .map(|case| CorpusCase {
            id: case["id"].as_str().expect("case id").to_owned(),
            group: case["group"].as_str().expect("case group").to_owned(),
            family: case["family"].as_str().expect("case family").to_owned(),
            template: case["template"].as_str().expect("case template").to_owned(),
            source: case["source"].as_str().expect("case source").to_owned(),
            expect_parse: case["expect_parse"]
                .as_str()
                .expect("case expectation")
                .to_owned(),
        })
        .collect()
}

fn oracle_outcomes() -> BTreeMap<String, (bool, bool)> {
    load_json("oracle-results.json")["results"]
        .as_array()
        .expect("oracle result records")
        .iter()
        .filter(|record| record["group"].as_str() != Some("syntax-matrix"))
        .map(|record| {
            (
                record["id"].as_str().expect("result id").to_owned(),
                (
                    record["syntax_accepted"].as_bool().expect("syntax outcome"),
                    record["semantic_error"]
                        .as_bool()
                        .expect("semantic outcome"),
                ),
            )
        })
        .collect()
}

fn shape_fingerprint(node: Node<'_>, output: &mut String) {
    use std::fmt::Write as _;
    write!(
        output,
        "({}:{}:{}:{}",
        node.kind(),
        u8::from(node.is_named()),
        u8::from(node.is_missing()),
        u8::from(node.is_error())
    )
    .expect("write shape");
    for index in 0..node.child_count() {
        output.push(' ');
        if let Some(field) = node.field_name_for_child(index as u32) {
            output.push_str(field);
            output.push('=');
        }
        shape_fingerprint(node.child(index as u32).expect("child exists"), output);
    }
    output.push(')');
}

fn structural_shape(tree: &Tree) -> String {
    let mut output = String::new();
    shape_fingerprint(tree.root_node(), &mut output);
    output
}

fn issue_nodes<'tree>(node: Node<'tree>, issues: &mut Vec<Node<'tree>>) {
    if node.is_error() || node.is_missing() {
        issues.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        issue_nodes(child, issues);
    }
}

fn assert_recursive_ranges(node: Node<'_>, source_len: usize) {
    assert!(node.start_byte() <= node.end_byte(), "{node:?}");
    assert!(node.end_byte() <= source_len, "{node:?}");
    assert!(node.start_position() <= node.end_position(), "{node:?}");
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        assert!(child.start_byte() >= node.start_byte(), "{child:?}");
        assert!(child.end_byte() <= node.end_byte(), "{child:?}");
        assert_recursive_ranges(child, source_len);
    }
}

#[test]
fn committed_oracle_corpus_matches_grammar_with_honest_counts() {
    let cases = quality_cases();
    let outcomes = oracle_outcomes();
    let expected_counts = BTreeMap::from([
        ("mutation", (200, 200)),
        ("semantic-invalid", (50, 10)),
        ("syntax-invalid", (75, 35)),
        ("syntax-valid", (358, 295)),
    ]);
    let mut parser = parser();
    let mut observed_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut authored_templates: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut structural_shapes: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut pairwise_shapes = BTreeSet::new();
    let mut mutation_family_counts: BTreeMap<&str, usize> = BTreeMap::new();

    for case in &cases {
        *observed_counts.entry(&case.group).or_default() += 1;
        authored_templates
            .entry(&case.group)
            .or_default()
            .insert(&case.template);
        let tree = parse(&mut parser, &case.source, None);
        let clean = is_clean(&case.source, &tree);
        let expected_clean = case.expect_parse == "accepted";
        assert_eq!(
            clean,
            expected_clean,
            "{} ({}/{}): {}\n{}",
            case.id,
            case.group,
            case.family,
            case.source.escape_debug(),
            tree.root_node().to_sexp(),
        );
        let (oracle_clean, oracle_semantic_error) = outcomes
            .get(&case.id)
            .unwrap_or_else(|| panic!("missing committed oracle outcome for {}", case.id));
        assert_eq!(*oracle_clean, expected_clean, "{} oracle drift", case.id);
        if case.group == "semantic-invalid" {
            assert!(
                *oracle_semantic_error,
                "{} must fail only after syntax acceptance",
                case.id
            );
        }
        let shape = structural_shape(&tree);
        structural_shapes
            .entry(&case.group)
            .or_default()
            .insert(shape.clone());
        if case.family == "pairwise-features" {
            assert!(
                pairwise_shapes.insert(shape),
                "{} duplicates a pairwise structural fingerprint",
                case.id
            );
        }
        if case.group == "mutation" {
            *mutation_family_counts.entry(&case.family).or_default() += 1;
        }
        assert_recursive_ranges(tree.root_node(), case.source.len());

        if !expected_clean {
            let mut issues = Vec::new();
            issue_nodes(tree.root_node(), &mut issues);
            if issues.is_empty() {
                assert_eq!(case.family, "bom", "{} has no recovery node", case.id);
                assert_eq!(tree.root_node().start_byte(), 3);
            }
            assert!(
                issues.len() <= 8,
                "{} cascaded to {} ERROR/MISSING nodes: {}",
                case.id,
                issues.len(),
                tree.root_node().to_sexp(),
            );
        }
    }

    assert_eq!(
        outcomes.len(),
        cases.len(),
        "oracle result completeness drift"
    );
    assert_eq!(pairwise_shapes.len(), 276, "pairwise feature matrix drift");
    assert_eq!(mutation_family_counts.len(), 10);
    assert!(
        mutation_family_counts.values().all(|count| *count == 20),
        "mutation context floor drift: {mutation_family_counts:?}"
    );

    for (group, (total, templates)) in expected_counts {
        assert_eq!(observed_counts[group], total, "{group} total drift");
        assert_eq!(
            authored_templates[group].len(),
            templates,
            "{group} authored-template drift"
        );
    }

    let shape_counts = BTreeMap::from([
        ("syntax-valid", structural_shapes["syntax-valid"].len()),
        (
            "semantic-invalid",
            structural_shapes["semantic-invalid"].len(),
        ),
        ("syntax-invalid", structural_shapes["syntax-invalid"].len()),
        ("mutation", structural_shapes["mutation"].len()),
    ]);
    eprintln!("unique recursive structural fingerprints: {shape_counts:?}");
    assert!(shape_counts["syntax-valid"] >= 276, "{shape_counts:?}");
    assert!(shape_counts["semantic-invalid"] >= 12, "{shape_counts:?}");
    assert!(shape_counts["syntax-invalid"] >= 45, "{shape_counts:?}");
    assert!(shape_counts["mutation"] >= 45, "{shape_counts:?}");
}

fn collect_kinds(node: Node<'_>, named: &mut BTreeSet<String>, anonymous: &mut BTreeSet<String>) {
    if node.is_named() {
        named.insert(node.kind().to_owned());
    } else {
        anonymous.insert(node.kind().to_owned());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_kinds(child, named, anonymous);
    }
}

#[test]
fn every_named_production_and_literal_token_is_exercised() {
    let source = r#"var arr[2, 3], x;
data { n = 3; }
model {
  # line comment
  /* block comment */
  for (i in 1:n) { custom_link(p[i]) <- -(arr[, i] + f(i)); }
  a <- (1 || 0) && (2 < 3);
  b <- (1 <= 2) + (3 > 2) - (4 >= 3);
  c <- (1 == 1) * (2 != 3) / 4;
  d <- 9 %% 4 + 9 %/% 4;
  e <- 2 ^ 3 ** 2;
  g <- left %custom% right;
  y ~ extension_distribution() T(0,);
  z ~ extension_distribution(0, 1) I(, 10);
}
"#;
    let mut parser = parser();
    let tree = parse(&mut parser, source, None);
    assert!(is_clean(source, &tree), "{}", tree.root_node().to_sexp());

    let mut named = BTreeSet::new();
    let mut anonymous = BTreeSet::new();
    collect_kinds(tree.root_node(), &mut named, &mut anonymous);
    assert_eq!(
        named,
        BTreeSet::from([
            "binary_operator".to_owned(),
            "block_statement".to_owned(),
            "bounds_clause".to_owned(),
            "call".to_owned(),
            "call_arguments".to_owned(),
            "comment".to_owned(),
            "data_block".to_owned(),
            "declared_variable".to_owned(),
            "deterministic_relation".to_owned(),
            "dimensions".to_owned(),
            "for_statement".to_owned(),
            "identifier".to_owned(),
            "link_call".to_owned(),
            "model_block".to_owned(),
            "number".to_owned(),
            "parenthesized_expression".to_owned(),
            "program".to_owned(),
            "special_operator".to_owned(),
            "stochastic_relation".to_owned(),
            "subset".to_owned(),
            "subset_arguments".to_owned(),
            "unary_operator".to_owned(),
            "variable_declaration".to_owned(),
        ]),
        "named production coverage drift"
    );
    assert_eq!(
        anonymous,
        BTreeSet::from([
            "(".to_owned(),
            ")".to_owned(),
            "*".to_owned(),
            "**".to_owned(),
            "+".to_owned(),
            ",".to_owned(),
            "-".to_owned(),
            "/".to_owned(),
            ":".to_owned(),
            ";".to_owned(),
            "<".to_owned(),
            "<-".to_owned(),
            "<=".to_owned(),
            "=".to_owned(),
            "==".to_owned(),
            ">".to_owned(),
            ">=".to_owned(),
            "I".to_owned(),
            "T".to_owned(),
            "[".to_owned(),
            "]".to_owned(),
            "^".to_owned(),
            "data".to_owned(),
            "for".to_owned(),
            "in".to_owned(),
            "model".to_owned(),
            "var".to_owned(),
            "{".to_owned(),
            "||".to_owned(),
            "}".to_owned(),
            "~".to_owned(),
            "&&".to_owned(),
            "!=".to_owned(),
            "%%".to_owned(),
            "%/%".to_owned(),
        ]),
        "literal token coverage drift"
    );
}

#[test]
fn exact_ast_shapes_are_stable() {
    let cases = [
        (
            "model { x ~ dnorm(0, 1) }",
            "(program (model_block body: (block_statement (stochastic_relation lhs: (identifier) distribution: (call function: (identifier) arguments: (call_arguments argument: (number) argument: (number)))))))",
        ),
        (
            "model { x ~ dfoo() }",
            "(program (model_block body: (block_statement (stochastic_relation lhs: (identifier) distribution: (call function: (identifier) arguments: (call_arguments))))))",
        ),
        (
            "var x[3]; data { n <- 3 } model { for (i in 1:n) { x[i] <- f(i) } }",
            "(program (variable_declaration (declared_variable name: (identifier) dimensions: (dimensions dimension: (number)))) (data_block body: (block_statement (deterministic_relation lhs: (identifier) rhs: (number)))) (model_block body: (block_statement (for_statement variable: (identifier) sequence: (binary_operator lhs: (number) rhs: (identifier)) body: (block_statement (deterministic_relation lhs: (subset function: (identifier) arguments: (subset_arguments argument: (identifier))) rhs: (call function: (identifier) arguments: (call_arguments argument: (identifier)))))))))",
        ),
        (
            "model { logit(p[i]) <- alpha + beta * x[i] }",
            "(program (model_block body: (block_statement (deterministic_relation lhs: (link_call function: (identifier) arguments: (call_arguments argument: (subset function: (identifier) arguments: (subset_arguments argument: (identifier))))) rhs: (binary_operator lhs: (identifier) rhs: (binary_operator lhs: (identifier) rhs: (subset function: (identifier) arguments: (subset_arguments argument: (identifier)))))))))",
        ),
        (
            "model { x <- a %foo% b + c }",
            "(program (model_block body: (block_statement (deterministic_relation lhs: (identifier) rhs: (binary_operator lhs: (binary_operator lhs: (identifier) operator: (special_operator) rhs: (identifier)) rhs: (identifier))))))",
        ),
    ];
    let mut parser = parser();
    for (source, expected) in cases {
        let tree = parse(&mut parser, source, None);
        assert!(is_clean(source, &tree));
        assert_eq!(tree.root_node().to_sexp(), expected, "{source}");
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

fn point_for_bytes(source: &[u8], byte: usize) -> Point {
    let prefix = &source[..byte];
    let row = prefix.iter().filter(|value| **value == b'\n').count();
    let column = prefix
        .iter()
        .rposition(|value| *value == b'\n')
        .map_or(prefix.len(), |newline| prefix.len() - newline - 1);
    Point::new(row, column)
}

fn decode_hex_fixture(source: &str) -> Vec<u8> {
    let compact = source.trim();
    assert_eq!(compact.len() % 2, 0);
    (0..compact.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&compact[index..index + 2], 16).expect("fixture hex"))
        .collect()
}

#[test]
fn arbitrary_invalid_pre_edit_reproducer_preserves_error_and_range_safety() {
    // Tree-sitter does not promise a canonical recovery tree when reusing an
    // already-invalid arbitrary tree. This minimized campaign input preserves
    // the boundary: both parses must remain safely classified as erroneous and
    // all recursive ranges must be valid, but their recovery shapes may differ.
    let data = decode_hex_fixture(include_str!("fixtures/arbitrary-invalid-pre-edit.hex"));
    let source_len = data.len() / 2;
    let mut source = data[..source_len].to_vec();
    let replacement = &data[source_len..];
    let start = usize::from(data[0]) % (source.len() + 1);
    let end = start + usize::from(data[1]) % (source.len() - start + 1);

    let mut parser = parser();
    let mut old_tree = parser.parse(&source, None).expect("old parse");
    assert!(old_tree.root_node().has_error());
    let start_position = point_for_bytes(&source, start);
    let old_end_position = point_for_bytes(&source, end);
    source.splice(start..end, replacement.iter().copied());
    let new_end = start + replacement.len();
    let new_end_position = point_for_bytes(&source, new_end);
    old_tree.edit(&InputEdit {
        start_byte: start,
        old_end_byte: end,
        new_end_byte: new_end,
        start_position,
        old_end_position,
        new_end_position,
    });
    let incremental = parser
        .parse(&source, Some(&old_tree))
        .expect("incremental parse");
    let fresh = parser.parse(&source, None).expect("fresh parse");
    for tree in [&incremental, &fresh] {
        assert!(tree.root_node().has_error());
        assert_eq!(tree.root_node().start_byte(), 0);
        assert_eq!(tree.root_node().end_byte(), source.len());
        assert_recursive_ranges(tree.root_node(), source.len());
        let mut issues = Vec::new();
        issue_nodes(tree.root_node(), &mut issues);
        assert!(!issues.is_empty());
    }
}

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

fn edit_and_reparse(
    parser: &mut Parser,
    tree: &mut Tree,
    source: &mut String,
    old_range: Range<usize>,
    replacement: &str,
    expected_clean: bool,
) {
    let start_position = point_for(source, old_range.start);
    let old_end_position = point_for(source, old_range.end);
    let mut new_source = source.clone();
    new_source.replace_range(old_range.clone(), replacement);
    let new_end_byte = old_range.start + replacement.len();
    let new_end_position = point_for(&new_source, new_end_byte);
    tree.edit(&InputEdit {
        start_byte: old_range.start,
        old_end_byte: old_range.end,
        new_end_byte,
        start_position,
        old_end_position,
        new_end_position,
    });
    let incremental = parse(parser, &new_source, Some(tree));
    let fresh = parse(parser, &new_source, None);
    assert_nodes_identical(incremental.root_node(), fresh.root_node());
    assert_eq!(is_clean(&new_source, &incremental), expected_clean);
    assert_recursive_ranges(incremental.root_node(), new_source.len());
    *source = new_source;
    *tree = incremental;
}

type EditStep<'a> = (&'a str, &'a str, bool);
type EditSequence<'a> = (&'a str, &'a str, &'a [EditStep<'a>]);

#[test]
fn structurally_diverse_incremental_families_match_fresh_ranges() {
    let sequences: [EditSequence<'_>; 25] = [
        (
            "relation-operator",
            "model { x <- 1 }\n",
            &[("<-", "", false), ("x  1", "x <- 1", true)],
        ),
        (
            "binary-operand",
            "model { x <- a + b }\n",
            &[("b", "", false), ("+  }", "+ b }", true)],
        ),
        (
            "call-comma",
            "model { x <- f(1, 2) }\n",
            &[(",", "", false), ("1 2", "1, 2", true)],
        ),
        (
            "call-closer",
            "model { x <- f(1, 2) }\n",
            &[(")", "", false), ("2 ", "2) ", true)],
        ),
        (
            "subset-closer",
            "model { x <- values[i,j] }\n",
            &[("]", "", false), ("j ", "j] ", true)],
        ),
        (
            "subset-dimension",
            "model { x <- values[i,j] }\n",
            &[("i,j", "i,,j", true), ("i,,j", "i,j", true)],
        ),
        (
            "loop-in",
            "model { for (i in 1:n) { x[i] <- i } }\n",
            &[(" in ", " ", false), ("i 1:n", "i in 1:n", true)],
        ),
        (
            "loop-brace",
            "model { for (i in 1:n) { x[i] <- i } }\n",
            &[("{ x", "x", false), ("n) x", "n) { x", true)],
        ),
        (
            "bounds-comma",
            "model { x ~ dfoo(0,1) T(0,1) }\n",
            &[("T(0,1)", "T(0 1)", false), ("T(0 1)", "T(0,1)", true)],
        ),
        (
            "model-keyword",
            "model { x <- 1 }\n",
            &[("model", "mode", false), ("mode", "model", true)],
        ),
        (
            "special-operator",
            "model { x <- a %foo% b }\n",
            &[("%foo%", "%foo", false), ("%foo", "%foo%", true)],
        ),
        (
            "comparison-parentheses",
            "model { x <- (a < b) < c }\n",
            &[
                ("(a < b)", "a < b", false),
                ("a < b < c", "(a < b) < c", true),
            ],
        ),
        (
            "whole-expression",
            "model { x <- a + b * c }\n",
            &[
                ("a + b * c", "outer(f(1), g(2,3))", true),
                ("outer(f(1), g(2,3))", "a + b * c", true),
            ],
        ),
        (
            "program-block",
            "model { x <- 1 }\n",
            &[
                ("model", "data { n <- 1 } model", true),
                ("data { n <- 1 } ", "", true),
            ],
        ),
        (
            "unicode-crlf-comment",
            "# λ 💥\r\nmodel {\r\n x <- 1\r\n}\r\n",
            &[("💥", "星", true), ("星", "💥", true)],
        ),
        (
            "line-comment-eof",
            "model { x <- 1 }\n# tail\n",
            &[("# tail\n", "# tail", false), ("# tail", "# tail\n", true)],
        ),
        (
            "stochastic-operator",
            "model { x ~ dfoo(0, 1) }\n",
            &[(" ~ ", " ", false), ("x dfoo", "x ~ dfoo", true)],
        ),
        (
            "distribution-call-delimiters",
            "model { x ~ dfoo() }\n",
            &[("()", "", false), ("dfoo ", "dfoo() ", true)],
        ),
        (
            "bounds-keyword",
            "model { x ~ dfoo(0, 1) T(0, 1) }\n",
            &[("T(0", "Q(0", false), ("Q(0", "T(0", true)],
        ),
        (
            "declaration-comma",
            "var x[2], y[3]; model { x[1] <- y[1] }\n",
            &[("], y", "] y", false), ("] y", "], y", true)],
        ),
        (
            "declaration-dimension-closer",
            "var x[2]; model { x[1] <- 1 }\n",
            &[("];", ";", false), ("2;", "2];", true)],
        ),
        (
            "link-call-closer",
            "model { logit(p[i]) <- eta[i] }\n",
            &[(") <-", " <-", false), ("p[i] <-", "p[i]) <-", true)],
        ),
        (
            "unary-operator",
            "model { x <- -(a + b) }\n",
            &[("-(a + b)", "(a + b)", true), ("(a + b)", "-(a + b)", true)],
        ),
        (
            "block-comment-closer",
            "model { /* note */ x <- 1 }\n",
            &[("*/", "", false), ("note  x", "note */ x", true)],
        ),
        (
            "nested-loop-outer-closer",
            "model { for (i in 1:n) { for (j in 1:m) { x[i,j] <- i+j } } }\n",
            &[
                ("i+j } } }", "i+j } }", false),
                ("i+j } }", "i+j } } }", true),
            ],
        ),
    ];

    let mut parser = parser();
    for (family, initial, steps) in sequences {
        let mut source = initial.to_owned();
        let mut tree = parse(&mut parser, &source, None);
        assert!(is_clean(&source, &tree), "initial {family}");
        for (needle, replacement, expected_clean) in steps {
            let start = source
                .find(needle)
                .unwrap_or_else(|| panic!("{family}: missing edit needle {needle:?}"));
            edit_and_reparse(
                &mut parser,
                &mut tree,
                &mut source,
                start..start + needle.len(),
                replacement,
                *expected_clean,
            );
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RecoveryIssue {
    kind: String,
    missing: bool,
    bytes: Range<usize>,
    points: (Point, Point),
}

fn recovery_issues(tree: &Tree) -> Vec<RecoveryIssue> {
    let mut nodes = Vec::new();
    issue_nodes(tree.root_node(), &mut nodes);
    nodes
        .into_iter()
        .map(|node| RecoveryIssue {
            kind: node.kind().to_owned(),
            missing: node.is_missing(),
            bytes: node.byte_range(),
            points: (node.start_position(), node.end_position()),
        })
        .collect()
}

#[test]
fn representative_recovery_trees_and_ranges_are_exact() {
    let cases: [(&str, &str, &[RecoveryIssue]); 17] = [
        (
            "missing-operand",
            "model { x <- * 1 }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 13..14,
                points: (Point::new(0, 13), Point::new(0, 14)),
            }],
        ),
        (
            "unclosed-call",
            "model { x <- f(1, 2 }\n",
            &[RecoveryIssue {
                kind: ")".to_owned(),
                missing: true,
                bytes: 19..19,
                points: (Point::new(0, 19), Point::new(0, 19)),
            }],
        ),
        (
            "missing-call-comma",
            "model { x <- f(1 2) }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 17..18,
                points: (Point::new(0, 17), Point::new(0, 18)),
            }],
        ),
        (
            "chained-comparison",
            "model { x <- a < b < c }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 15..18,
                points: (Point::new(0, 15), Point::new(0, 18)),
            }],
        ),
        (
            "semicolon-after-loop",
            "model { for (i in 1:2) { x[i] <- i }; y <- 1 }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 36..37,
                points: (Point::new(0, 36), Point::new(0, 37)),
            }],
        ),
        (
            "eof-line-comment",
            "model { x <- 1 }\n# tail",
            &[
                RecoveryIssue {
                    kind: "ERROR".to_owned(),
                    missing: false,
                    bytes: 17..23,
                    points: (Point::new(1, 0), Point::new(1, 6)),
                },
                RecoveryIssue {
                    kind: "ERROR".to_owned(),
                    missing: false,
                    bytes: 17..23,
                    points: (Point::new(1, 0), Point::new(1, 6)),
                },
            ],
        ),
        (
            "top-level-relation",
            "x <- 1\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 0..6,
                points: (Point::new(0, 0), Point::new(0, 6)),
            }],
        ),
        (
            "missing-model-after-data",
            "data { x <- 1 }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 0..15,
                points: (Point::new(0, 0), Point::new(0, 15)),
            }],
        ),
        (
            "missing-model-after-var",
            "var x;\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 0..6,
                points: (Point::new(0, 0), Point::new(0, 6)),
            }],
        ),
        (
            "data-after-model",
            "model { x <- 1 } data { y <- 2 }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 6..21,
                points: (Point::new(0, 6), Point::new(0, 21)),
            }],
        ),
        (
            "var-after-data",
            "data { y <- 2 } var x; model { x <- y }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 16..22,
                points: (Point::new(0, 16), Point::new(0, 22)),
            }],
        ),
        (
            "duplicate-model",
            "model { x <- 1 } model { y <- 2 }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 0..16,
                points: (Point::new(0, 0), Point::new(0, 16)),
            }],
        ),
        (
            "duplicate-data",
            "data { x <- 1 } data { y <- 2 } model { z <- 3 }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 16..31,
                points: (Point::new(0, 16), Point::new(0, 31)),
            }],
        ),
        (
            "empty-model",
            "model {}\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 0..8,
                points: (Point::new(0, 0), Point::new(0, 8)),
            }],
        ),
        (
            "nested-unclosed-call",
            "model { x <- f(g(1, 2) }\n",
            &[RecoveryIssue {
                kind: ")".to_owned(),
                missing: true,
                bytes: 22..22,
                points: (Point::new(0, 22), Point::new(0, 22)),
            }],
        ),
        (
            "nested-unclosed-subset",
            "model { x <- a[b[c] }\n",
            &[RecoveryIssue {
                kind: "]".to_owned(),
                missing: true,
                bytes: 19..19,
                points: (Point::new(0, 19), Point::new(0, 19)),
            }],
        ),
        (
            "balanced-malformed-delimiters",
            "model { x <- f(]1) }\n",
            &[RecoveryIssue {
                kind: "ERROR".to_owned(),
                missing: false,
                bytes: 15..16,
                points: (Point::new(0, 15), Point::new(0, 16)),
            }],
        ),
    ];
    let mut parser = parser();
    for (name, source, expected) in cases {
        let first = parse(&mut parser, source, None);
        let second = parse(&mut parser, source, None);
        assert_eq!(first.root_node().to_sexp(), second.root_node().to_sexp());
        assert_eq!(
            recovery_issues(&first),
            expected,
            "{name}: {}",
            first.root_node().to_sexp()
        );
        assert_recursive_ranges(first.root_node(), source.len());
    }
}

#[test]
fn program_recovery_fingerprints_are_exact() {
    let cases = [
        (
            "top-level-relation",
            "x <- 1\n",
            r#"(program:1:0:0 (ERROR:1:0:1 (identifier:1:0:0) (<:0:0:0) (-:0:0:0) (number:1:0:0)))"#,
        ),
        (
            "missing-model-after-data",
            "data { x <- 1 }\n",
            r#"(program:1:0:0 (ERROR:1:0:1 (data_block:1:0:0 (data:0:0:0) body=(block_statement:1:0:0 ({:0:0:0) (deterministic_relation:1:0:0 lhs=(identifier:1:0:0) operator=(<-:0:0:0) rhs=(number:1:0:0)) (}:0:0:0)))))"#,
        ),
        (
            "data-after-model",
            "model { x <- 1 } data { y <- 2 }\n",
            r#"(program:1:0:0 (model_block:1:0:0 (model:0:0:0) (ERROR:1:0:1 (block_statement:1:0:0 ({:0:0:0) (deterministic_relation:1:0:0 lhs=(identifier:1:0:0) operator=(<-:0:0:0) rhs=(number:1:0:0)) (}:0:0:0)) (identifier:1:0:0)) body=(block_statement:1:0:0 ({:0:0:0) (deterministic_relation:1:0:0 lhs=(identifier:1:0:0) operator=(<-:0:0:0) rhs=(number:1:0:0)) (}:0:0:0))))"#,
        ),
        (
            "duplicate-model",
            "model { x <- 1 } model { y <- 2 }\n",
            r#"(program:1:0:0 (ERROR:1:0:1 (model:0:0:0) (block_statement:1:0:0 ({:0:0:0) (deterministic_relation:1:0:0 lhs=(identifier:1:0:0) operator=(<-:0:0:0) rhs=(number:1:0:0)) (}:0:0:0))) (model_block:1:0:0 (model:0:0:0) body=(block_statement:1:0:0 ({:0:0:0) (deterministic_relation:1:0:0 lhs=(identifier:1:0:0) operator=(<-:0:0:0) rhs=(number:1:0:0)) (}:0:0:0))))"#,
        ),
        (
            "empty-model",
            "model {}\n",
            r#"(program:1:0:0 (ERROR:1:0:1 (model:0:0:0) ({:0:0:0) (}:0:0:0)))"#,
        ),
        (
            "nested-unclosed-call",
            "model { x <- f(g(1, 2) }\n",
            r#"(program:1:0:0 (model_block:1:0:0 (model:0:0:0) body=(block_statement:1:0:0 ({:0:0:0) (deterministic_relation:1:0:0 lhs=(identifier:1:0:0) operator=(<-:0:0:0) rhs=(call:1:0:0 function=(identifier:1:0:0) arguments=(call_arguments:1:0:0 ((:0:0:0) argument=(call:1:0:0 function=(identifier:1:0:0) arguments=(call_arguments:1:0:0 ((:0:0:0) argument=(number:1:0:0) (,:0:0:0) argument=(number:1:0:0) ():0:0:0))) ():0:1:0)))) (}:0:0:0))))"#,
        ),
        (
            "nested-unclosed-subset",
            "model { x <- a[b[c] }\n",
            r#"(program:1:0:0 (model_block:1:0:0 (model:0:0:0) body=(block_statement:1:0:0 ({:0:0:0) (deterministic_relation:1:0:0 lhs=(identifier:1:0:0) operator=(<-:0:0:0) rhs=(subset:1:0:0 function=(identifier:1:0:0) arguments=(subset_arguments:1:0:0 ([:0:0:0) argument=(subset:1:0:0 function=(identifier:1:0:0) arguments=(subset_arguments:1:0:0 ([:0:0:0) argument=(identifier:1:0:0) (]:0:0:0))) (]:0:1:0)))) (}:0:0:0))))"#,
        ),
    ];

    let mut parser = parser();
    for (name, source, expected) in cases {
        let tree = parse(&mut parser, source, None);
        assert_eq!(structural_shape(&tree), expected, "{name}");
        assert_recursive_ranges(tree.root_node(), source.len());
    }
}

#[test]
fn separated_faults_recover_locally_without_a_cascade() {
    let source = "model {\n x <- * 1\n y <- f(1 2)\n z <- 3\n}\n";
    let mut parser = parser();
    let tree = parse(&mut parser, source, None);
    let issues = recovery_issues(&tree);
    assert_eq!(issues.len(), 2, "{}", tree.root_node().to_sexp());
    assert_eq!(issues[0].points.0.row, 1);
    assert_eq!(issues[1].points.0.row, 2);
    assert_recursive_ranges(tree.root_node(), source.len());
}

#[test]
fn unicode_crlf_bom_and_eof_ranges_are_exact() {
    let source = "# λ 💥\r\nmodel {\r\n  x <- 1\r\n}\r\n";
    let mut parser = parser();
    let tree = parse(&mut parser, source, None);
    assert!(is_clean(source, &tree), "{}", tree.root_node().to_sexp());
    let root = tree.root_node();
    let mut cursor = root.walk();
    let model = root
        .named_children(&mut cursor)
        .find(|node| node.kind() == "model_block")
        .expect("model block");
    assert_eq!(model.kind(), "model_block");
    assert_eq!(
        model.start_byte(),
        source.find("model").expect("model byte")
    );
    assert_eq!(model.start_position(), Point::new(1, 0));
    assert_eq!(model.end_position(), Point::new(3, 1));
    let relation = model
        .child_by_field_name("body")
        .expect("body")
        .named_child(0)
        .expect("relation");
    assert_eq!(relation.start_position(), Point::new(2, 2));
    assert_eq!(relation.end_position(), Point::new(2, 8));
    assert_recursive_ranges(tree.root_node(), source.len());

    let bom = "\u{feff}model { x <- 1 }\n";
    let bom_tree = parse(&mut parser, bom, None);
    assert!(!bom_tree.root_node().has_error());
    assert_eq!(bom_tree.root_node().start_byte(), 3);
    assert_eq!(bom_tree.root_node().end_byte(), bom.len());
    assert!(!is_clean(bom, &bom_tree));

    let no_newline = "model { x <- 1 }\n# tail";
    let eof_tree = parse(&mut parser, no_newline, None);
    assert!(!is_clean(no_newline, &eof_tree));
    assert_eq!(recovery_issues(&eof_tree)[0].bytes, 17..23);
}

#[test]
fn property_generators_cover_valid_and_arbitrary_utf8_ranges() {
    let mut parser = parser();
    for index in 0..1_024 {
        let whitespace = match index % 4 {
            0 => " ",
            1 => "\n",
            2 => "\r\n",
            _ => " /* gap */ ",
        };
        let operator = ["+", "-", "*", "/", "%%", "%/%", "^", "**"][index % 8];
        let source = format!(
            "model{whitespace}{{{whitespace}x_{index}{whitespace}<-{whitespace}({}){whitespace}{operator}{whitespace}{}{whitespace}}}\n",
            index + 1,
            index + 2,
        );
        let tree = parse(&mut parser, &source, None);
        assert!(
            is_clean(&source, &tree),
            "case {index}: {}",
            tree.root_node().to_sexp()
        );
    }

    let alphabet = [
        "a", "1", "{", "}", "(", ")", "[", "]", "~", "<-", "λ", "💥", "\r\n", "#x\n",
    ];
    let mut state = 0x9e37_79b9_u32;
    for _ in 0..1_024 {
        let mut source = String::new();
        for _ in 0..32 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            source.push_str(alphabet[state as usize % alphabet.len()]);
        }
        let tree = parse(&mut parser, &source, None);
        assert_recursive_ranges(tree.root_node(), source.len());
    }
}

fn node_count(node: Node<'_>) -> usize {
    let mut count = 1;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count += node_count(child);
    }
    count
}

fn max_depth(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    1 + node.children(&mut cursor).map(max_depth).max().unwrap_or(0)
}

#[test]
fn large_tree_growth_and_late_cancellation_are_bounded() {
    let source = "model {\n".to_owned() + &"x <- f(1, 2);\n".repeat(65_536) + "}\n";
    let mut parser = parser();
    let tree = parse(&mut parser, &source, None);
    assert!(is_clean(&source, &tree));
    let nodes = node_count(tree.root_node());
    assert!(
        nodes < source.len(),
        "{nodes} nodes for {} bytes",
        source.len()
    );
    assert!(max_depth(tree.root_node()) < 32);

    let bytes = source.as_bytes();
    let mut reads = |offset: usize, _point: Point| &bytes[offset..];
    let mut polls = 0usize;
    let cancel_after = 256;
    let mut cancel = |_state: &tree_sitter::ParseState| {
        polls += 1;
        if polls >= cancel_after {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let started = Instant::now();
    let result = parser.parse_with_options(
        &mut reads,
        None,
        Some(ParseOptions::new().progress_callback(&mut cancel)),
    );
    let elapsed = started.elapsed();
    assert!(result.is_none(), "late-cancelled parse returned a tree");
    assert_eq!(
        polls, cancel_after,
        "cancellation was not meaningfully delayed"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "late cancellation took {elapsed:?}"
    );
}

#[test]
fn recovery_is_deterministic_and_bounded_on_many_faults() {
    let mut parser = parser();
    let source = format!("model {{\n{}\n}}", "x <- f(1, 2\n".repeat(1_000));
    let first = parse(&mut parser, &source, None);
    let second = parse(&mut parser, &source, None);
    assert_eq!(first.root_node().to_sexp(), second.root_node().to_sexp());
    assert!(first.root_node().has_error());
    let issues = recovery_issues(&first);
    assert!(issues.len() <= 1_010, "{} recovery issues", issues.len());
    assert!(node_count(first.root_node()) < source.len());
    assert_recursive_ranges(first.root_node(), source.len());
}

#[test]
fn offline_oracle_manifest_verifier_passes() {
    let status = Command::new("python3")
        .arg(oracle_dir().join("generate_quality_corpus.py"))
        .arg("--check")
        .status()
        .expect("run corpus drift verifier");
    assert!(status.success());
    let status = Command::new("python3")
        .arg(oracle_dir().join("jags_oracle.py"))
        .arg("--verify-results")
        .status()
        .expect("run oracle result verifier");
    assert!(status.success());
}

#[test]
#[ignore = "requires the exact hash-pinned JAGS 4.3.2 black-box oracle"]
fn all_committed_outcomes_match_live_jags() {
    let status = Command::new("python3")
        .arg(oracle_dir().join("jags_oracle.py"))
        .arg("--verify-results-live")
        .status()
        .expect("run live JAGS oracle verifier");
    assert!(status.success());
}
