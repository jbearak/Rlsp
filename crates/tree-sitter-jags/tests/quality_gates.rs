use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use tree_sitter::{InputEdit, Node, ParseOptions, Parser, Point, Tree};

const VALID_CORPUS_SIZE: usize = 320;
const SEMANTIC_INVALID_CORPUS_SIZE: usize = 64;
const CURATED_INVALID_FAMILIES: usize = 12;
const CURATED_INVALID_VARIANTS: usize = 8;
const INCREMENTAL_SEQUENCE_COUNT: usize = 32;
const PROPERTY_CASES: usize = 1_024;
static ORACLE_FILE_ID: AtomicUsize = AtomicUsize::new(0);

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

fn valid_models() -> Vec<String> {
    (0..VALID_CORPUS_SIZE)
        .map(|index| {
            let value = index + 1;
            match index % 8 {
                0 => format!("model {{ x_{index} <- {value} + 2 * 3 }}\n"),
                1 => format!(
                    "model {{ x_{index} ~ extension_distribution_{index}({value}) }}\n"
                ),
                2 => format!(
                    "model {{ for (i_{index} in 1:3) {{ x_{index}[i_{index}] <- i_{index} + {value} }} }}\n"
                ),
                3 => format!(
                    "var x_{index}[3], y_{index}; model {{ x_{index}[1] <- y_{index} + {value} }}\n"
                ),
                4 => format!(
                    "data {{ scale_{index} <- {value} }} model {{ x_{index} <- scale_{index} / 2 }}\n"
                ),
                5 => format!(
                    "model {{ custom_link_{index}(p_{index}) <- alpha_{index} + {value} }}\n"
                ),
                6 => format!(
                    "model {{ x_{index} ~ extension_distribution_{index}(0, 1) T(, {value}) }}\n"
                ),
                _ => format!(
                    "model {{ x_{index} ~ extension_distribution_{index}(0, 1) I({value},) }}\n"
                ),
            }
        })
        .collect()
}

fn production_coverage_model() -> &'static str {
    r#"var arr[2, 3], x;
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
  y ~ extension_distribution(0, 1) T(0,);
  z ~ extension_distribution(0, 1) I(, 10);
}
"#
}

fn semantic_invalid_models() -> Vec<String> {
    (0..SEMANTIC_INVALID_CORPUS_SIZE)
        .map(|index| {
            if index % 2 == 0 {
                format!("model {{ x_{index} ~ definitely_unknown_distribution_{index}(1) }}\n")
            } else {
                format!("model {{ x_{index} <- definitely_unknown_function_{index}(1) }}\n")
            }
        })
        .collect()
}

#[derive(Debug)]
struct InvalidCase {
    name: String,
    source: String,
    defect_line: usize,
}

fn curated_invalid_models() -> Vec<InvalidCase> {
    let mut cases = Vec::new();
    for variant in 0..CURATED_INVALID_VARIANTS {
        let sources = [
            (
                "missing-operand",
                format!("model {{\n x_{variant} <- * 1\n}}\n"),
                1,
            ),
            ("missing-brace", format!("model {{\n x_{variant} <- 1\n"), 1),
            (
                "unclosed-call",
                format!("model {{\n x_{variant} <- f(1, 2\n}}\n"),
                1,
            ),
            (
                "missing-call-comma",
                format!("model {{\n x_{variant} <- f(1 2)\n}}\n"),
                1,
            ),
            (
                "unclosed-subset",
                format!("model {{\n x_{variant} <- values[i\n}}\n"),
                1,
            ),
            (
                "missing-relation",
                format!("model {{\n x_{variant} dnorm(0, 1)\n}}\n"),
                1,
            ),
            (
                "missing-loop-in",
                format!(
                    "model {{\n for (i_{variant} 1:3) {{ x_{variant}[i_{variant}] <- 1 }}\n}}\n"
                ),
                1,
            ),
            (
                "unbraced-loop",
                format!("model {{\n for (i_{variant} in 1:3) x_{variant}[i_{variant}] <- 1\n}}\n"),
                1,
            ),
            (
                "r-string",
                format!("model {{\n x_{variant} <- \"text\"\n}}\n"),
                1,
            ),
            (
                "r-named-argument",
                format!("model {{\n x_{variant} <- f(value = 1)\n}}\n"),
                1,
            ),
            (
                "empty-call",
                format!("model {{\n x_{variant} <- f()\n}}\n"),
                1,
            ),
            (
                "bare-distribution",
                format!("model {{\n x_{variant} ~ distribution_{variant}\n}}\n"),
                1,
            ),
        ];
        for (family, source, defect_line) in sources {
            cases.push(InvalidCase {
                name: format!("{family}-{variant}"),
                source,
                defect_line,
            });
        }
    }
    cases
}

fn error_intersects_line(node: Node<'_>, line: usize) -> bool {
    if (node.kind() == "ERROR" || node.is_missing())
        && node.start_position().row <= line
        && node.end_position().row >= line
    {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| error_intersects_line(child, line))
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
    let source = production_coverage_model();
    let mut parser = parser();
    let tree = parse(&mut parser, source, None);
    assert!(is_clean(source, &tree), "{}", tree.root_node().to_sexp());

    let mut named = BTreeSet::new();
    let mut anonymous = BTreeSet::new();
    collect_kinds(tree.root_node(), &mut named, &mut anonymous);

    let expected_named = BTreeSet::from([
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
        "stochastic_relation".to_owned(),
        "subset".to_owned(),
        "subset_arguments".to_owned(),
        "unary_operator".to_owned(),
        "variable_declaration".to_owned(),
    ]);
    assert_eq!(named, expected_named, "named production coverage drift");

    let expected_anonymous = BTreeSet::from([
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
        "|".repeat(2),
        "}".to_owned(),
        "~".to_owned(),
        "&&".to_owned(),
        "!=".to_owned(),
        "%%".to_owned(),
        "%/%".to_owned(),
    ]);
    assert_eq!(
        anonymous, expected_anonymous,
        "literal token coverage drift"
    );
}

#[test]
fn deterministic_valid_corpus_is_clean() {
    let corpus = valid_models();
    assert!(corpus.len() >= 256);
    let mut parser = parser();
    for (index, source) in corpus.iter().enumerate() {
        let tree = parse(&mut parser, source, None);
        assert!(
            is_clean(source, &tree),
            "valid corpus case {index}: {}\n{}",
            source.escape_debug(),
            tree.root_node().to_sexp(),
        );
    }
}

#[test]
fn semantic_invalid_corpus_is_syntax_clean() {
    let corpus = semantic_invalid_models();
    assert!(corpus.len() >= 50);
    let mut parser = parser();
    for (index, source) in corpus.iter().enumerate() {
        let tree = parse(&mut parser, source, None);
        assert!(
            is_clean(source, &tree),
            "semantic-only case {index}: {}\n{}",
            source.escape_debug(),
            tree.root_node().to_sexp(),
        );
    }
}

#[test]
fn curated_invalid_corpus_has_errors_in_defect_windows() {
    let corpus = curated_invalid_models();
    assert_eq!(
        corpus.len(),
        CURATED_INVALID_FAMILIES * CURATED_INVALID_VARIANTS
    );
    assert!(corpus.len() >= 75);
    let mut parser = parser();
    for case in &corpus {
        let tree = parse(&mut parser, &case.source, None);
        assert!(
            !is_clean(&case.source, &tree),
            "invalid case parsed cleanly: {}",
            case.name,
        );
        assert!(
            error_intersects_line(tree.root_node(), case.defect_line),
            "{} has no ERROR/MISSING in defect line {}:\n{}",
            case.name,
            case.defect_line,
            tree.root_node().to_sexp(),
        );
    }
}

fn mutation_sources(category: &str) -> Vec<String> {
    (0..20)
        .map(|index| match category {
            "delete-relation-operator" => format!("model {{ x_{index} 1 }}\n"),
            "missing-operand" => format!("model {{ x_{index} <- * 1 }}\n"),
            "unclosed-call" => format!("model {{ x_{index} <- f(1, 2 }}\n"),
            "missing-call-comma" => format!("model {{ x_{index} <- f(1 2) }}\n"),
            "unclosed-subset" => format!("model {{ x_{index} <- values[i }}\n"),
            "missing-loop-in" => {
                format!("model {{ for (i_{index} 1:3) {{ x_{index}[i_{index}] <- 1 }} }}\n")
            }
            "unbraced-loop" => {
                format!("model {{ for (i_{index} in 1:3) x_{index}[i_{index}] <- 1 }}\n")
            }
            "r-string" => format!("model {{ x_{index} <- \"text\" }}\n"),
            "empty-call" => format!("model {{ x_{index} <- f() }}\n"),
            "bare-distribution" => {
                format!("model {{ x_{index} ~ distribution_{index} }}\n")
            }
            _ => unreachable!("known mutation category"),
        })
        .collect()
}

const MUTATION_CATEGORIES: [&str; 10] = [
    "delete-relation-operator",
    "missing-operand",
    "unclosed-call",
    "missing-call-comma",
    "unclosed-subset",
    "missing-loop-in",
    "unbraced-loop",
    "r-string",
    "empty-call",
    "bare-distribution",
];

#[test]
fn deterministic_mutations_have_at_least_95_percent_detection_per_category() {
    let mut parser = parser();
    for category in MUTATION_CATEGORIES {
        let mutants = mutation_sources(category);
        let detected = mutants
            .iter()
            .filter(|source| !is_clean(source, &parse(&mut parser, source, None)))
            .count();
        let recall = detected as f64 / mutants.len() as f64;
        assert!(
            recall >= 0.95,
            "{category}: {detected}/{} ({:.1}%)",
            mutants.len(),
            recall * 100.0,
        );
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

fn edit_and_reparse(
    parser: &mut Parser,
    tree: &mut Tree,
    source: &mut String,
    old_range: Range<usize>,
    replacement: &str,
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
    assert_eq!(
        incremental.root_node().to_sexp(),
        fresh.root_node().to_sexp(),
        "incremental/fresh mismatch for {}",
        new_source.escape_debug(),
    );
    assert_eq!(
        is_clean(&new_source, &incremental),
        is_clean(&new_source, &fresh)
    );
    *source = new_source;
    *tree = incremental;
}

#[test]
fn at_least_25_incremental_sequences_match_fresh_parses() {
    let mut parser = parser();
    for index in 0..INCREMENTAL_SEQUENCE_COUNT {
        let prefix = match index % 4 {
            0 => "",
            1 => "# lambda λ and comet 💥\n",
            2 => "# CRLF\r\n",
            _ => "/* block\n comment */\n",
        };
        let mut source = format!("{prefix}model {{ x_{index} <- f(1, 2) }}\n");
        let mut tree = parse(&mut parser, &source, None);
        assert!(is_clean(&source, &tree));

        let literal = source.rfind('2').expect("literal");
        edit_and_reparse(
            &mut parser,
            &mut tree,
            &mut source,
            literal..literal + 1,
            "2 + 3",
        );

        let close = source.rfind(')').expect("call closer");
        edit_and_reparse(&mut parser, &mut tree, &mut source, close..close + 1, "");
        assert!(!is_clean(&source, &tree));

        let insert = source.rfind(" }").expect("model closer");
        edit_and_reparse(&mut parser, &mut tree, &mut source, insert..insert, ")");
        assert!(is_clean(&source, &tree));
    }
}

#[test]
fn exact_ast_shapes_are_stable() {
    let cases = [
        (
            "model { x ~ dnorm(0, 1) }",
            "(program (model_block body: (block_statement (stochastic_relation lhs: (identifier) distribution: (call function: (identifier) arguments: (call_arguments argument: (number) argument: (number)))))))",
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
            "model { x ~ dnorm(0, 1) T(0,) }",
            "(program (model_block body: (block_statement (stochastic_relation lhs: (identifier) distribution: (call function: (identifier) arguments: (call_arguments argument: (number) argument: (number))) bounds: (bounds_clause lower: (number))))))",
        ),
    ];
    let mut parser = parser();
    for (source, expected) in cases {
        let tree = parse(&mut parser, source, None);
        assert!(is_clean(source, &tree));
        assert_eq!(tree.root_node().to_sexp(), expected);
    }
}

#[test]
fn encoding_and_eof_boundaries_are_explicit() {
    let cases = [
        ("model { x <- 1 }", true),
        ("model { x <- 1 }\n", true),
        ("model {\r\n x <- 1\r\n}\r\n", true),
        ("# λ 💥\nmodel { x <- 1 }\n", true),
        ("\u{feff}model { x <- 1 }\n", false),
        ("model { x <- 1", false),
        ("", false),
    ];
    let mut parser = parser();
    for (source, expected_clean) in cases {
        let tree = parse(&mut parser, source, None);
        assert_eq!(
            is_clean(source, &tree),
            expected_clean,
            "{}\n{}",
            source.escape_debug(),
            tree.root_node().to_sexp(),
        );
    }
}

#[test]
fn property_valid_generator_produces_1024_clean_models() {
    let mut parser = parser();
    for index in 0..PROPERTY_CASES {
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
            "property valid case {index}: {}\n{}",
            source.escape_debug(),
            tree.root_node().to_sexp(),
        );
    }
}

#[test]
fn property_arbitrary_utf8_never_panics_and_ranges_stay_in_bounds() {
    let alphabet = [
        "a", "1", "{", "}", "(", ")", "[", "]", "~", "<-", "λ", "💥", "\r\n", "#x\n",
    ];
    let mut parser = parser();
    let mut state = 0x9e37_79b9_u32;
    for _ in 0..PROPERTY_CASES {
        let mut source = String::new();
        for _ in 0..32 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            source.push_str(alphabet[state as usize % alphabet.len()]);
        }
        let tree = parse(&mut parser, &source, None);
        let root = tree.root_node();
        assert!(root.start_byte() <= root.end_byte());
        assert!(root.end_byte() <= source.len());
        assert_ranges_in_bounds(root, source.len());
    }
}

fn assert_ranges_in_bounds(node: Node<'_>, source_len: usize) {
    assert!(node.start_byte() <= node.end_byte());
    assert!(node.end_byte() <= source_len);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        assert_ranges_in_bounds(child, source_len);
    }
}

#[test]
fn recovery_is_deterministic_and_bounded() {
    let mut parser = parser();
    let source = format!("model {{\n{}\n}}", "x <- f(1, 2\n".repeat(1_000));
    let first = parse(&mut parser, &source, None);
    let second = parse(&mut parser, &source, None);
    assert_eq!(first.root_node().to_sexp(), second.root_node().to_sexp());
    assert!(first.root_node().has_error());
    assert!(node_count(first.root_node()) < source.len() * 2);
}

#[test]
fn cancellation_stops_a_large_parse_without_returning_a_partial_tree() {
    let source = "model {\n".to_owned() + &"x <- f(1, 2);\n".repeat(100_000) + "}\n";
    let bytes = source.as_bytes();
    let mut parser = parser();
    let mut reads = |offset: usize, _point: Point| &bytes[offset..];
    let mut polls = 0;
    let mut cancel = |_state: &tree_sitter::ParseState| {
        polls += 1;
        ControlFlow::Break(())
    };
    let start = std::time::Instant::now();
    let result = parser.parse_with_options(
        &mut reads,
        None,
        Some(ParseOptions::new().progress_callback(&mut cancel)),
    );
    let elapsed = start.elapsed();
    assert!(result.is_none(), "cancelled parse returned a tree");
    assert!(polls > 0, "progress callback was not polled");
    assert!(
        elapsed < std::time::Duration::from_millis(100),
        "cancellation took {elapsed:?}"
    );
}

fn node_count(node: Node<'_>) -> usize {
    let mut count = 1;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count += node_count(child);
    }
    count
}

#[derive(Debug)]
struct JagsResult {
    syntax_accepted: bool,
    semantic_error: bool,
}

fn jags_path() -> PathBuf {
    std::env::var_os("JAGS_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/homebrew/bin/jags"))
}

fn run_jags(jags: &Path, source: &str, compile: bool) -> JagsResult {
    let id = ORACLE_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let model_path = std::env::temp_dir().join(format!(
        "raven-tree-sitter-jags-{}-{id}.jags",
        std::process::id()
    ));
    std::fs::write(&model_path, source).expect("write oracle model");

    let mut child = Command::new(jags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start JAGS oracle");
    let mut commands = format!("model in \"{}\"\n", model_path.display());
    if compile {
        commands.push_str("compile\n");
    }
    commands.push_str("exit\n");
    use std::io::Write as _;
    child
        .stdin
        .take()
        .expect("JAGS stdin")
        .write_all(commands.as_bytes())
        .expect("write JAGS commands");
    let output = child.wait_with_output().expect("wait for JAGS oracle");
    std::fs::remove_file(&model_path).expect("remove oracle model");

    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.contains("Welcome to JAGS 4.3.2"),
        "oracle version drift:\n{output}"
    );
    JagsResult {
        syntax_accepted: !output.contains("Error parsing model file:"),
        semantic_error: output.contains("RUNTIME ERROR:")
            || output.contains("Compilation error on line"),
    }
}

#[test]
#[ignore = "requires the separately installed JAGS 4.3.2 black-box oracle"]
fn generated_corpora_and_mutations_match_real_jags() {
    let jags = jags_path();
    assert!(jags.exists(), "JAGS oracle not found at {}", jags.display());

    for (index, source) in valid_models().iter().enumerate() {
        let result = run_jags(&jags, source, false);
        assert!(result.syntax_accepted, "valid corpus case {index} rejected");
    }
    assert!(
        run_jags(&jags, production_coverage_model(), false).syntax_accepted,
        "production coverage model rejected by JAGS"
    );
    for (index, source) in semantic_invalid_models().iter().enumerate() {
        let result = run_jags(&jags, source, true);
        assert!(
            result.syntax_accepted,
            "semantic-invalid corpus case {index} rejected during parse"
        );
        assert!(
            result.semantic_error,
            "semantic-invalid corpus case {index} did not fail compilation"
        );
    }
    for case in curated_invalid_models() {
        let result = run_jags(&jags, &case.source, false);
        assert!(
            !result.syntax_accepted,
            "curated invalid case {} was accepted by JAGS",
            case.name,
        );
    }

    let mut parser = parser();
    for category in MUTATION_CATEGORIES {
        let mutants = mutation_sources(category);
        let mut jags_rejected = 0;
        let mut detected = 0;
        let mut escapes = Vec::new();
        for (index, source) in mutants.iter().enumerate() {
            if run_jags(&jags, source, false).syntax_accepted {
                continue;
            }
            jags_rejected += 1;
            if !is_clean(source, &parse(&mut parser, source, None)) {
                detected += 1;
            } else {
                escapes.push(index);
            }
        }
        assert!(
            jags_rejected > 0,
            "{category}: empty JAGS-filtered denominator"
        );
        let recall = detected as f64 / jags_rejected as f64;
        eprintln!(
            "mutation {category}: {detected}/{jags_rejected} ({:.1}%), escapes={escapes:?}",
            recall * 100.0,
        );
        assert!(
            recall >= 0.95,
            "{category}: {detected}/{jags_rejected} ({:.1}%), reviewed escapes={escapes:?}",
            recall * 100.0,
        );
    }
}
