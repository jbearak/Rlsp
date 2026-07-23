use std::collections::HashMap;
use std::collections::HashSet;

use raven::handlers::{DiagCancelToken, diagnostics};
use raven::state::WorldState;
use serde::Deserialize;
use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString, Url};

#[derive(Deserialize)]
struct Fixture {
    name: String,
    code: String,
}

#[derive(Deserialize)]
struct InvalidExpectation {
    name: String,
    expected_lines: Vec<u32>,
    min_diagnostics: usize,
    max_diagnostics: usize,
}

fn fixtures(source: &str) -> Vec<Fixture> {
    serde_json::from_str(source).expect("checked-in Stan fixture JSON must parse")
}

fn analyze(fixture: &Fixture) -> (WorldState, Url, Vec<tower_lsp::lsp_types::Diagnostic>) {
    let uri = Url::parse(&format!("untitled:stan-fixture-{}", fixture.name)).unwrap();
    let mut state = WorldState::new();
    state.open_document_with_language_id(uri.clone(), &fixture.code, Some(1), Some("stan"));
    let findings = diagnostics(&state, &uri, &DiagCancelToken::never());
    (state, uri, findings)
}

#[test]
fn compiler_valid_and_semantic_only_corpora_have_no_false_positives() {
    let groups = [
        include_str!("fixtures/stan/valid.json"),
        include_str!("fixtures/stan/generated.json"),
        include_str!("fixtures/stan/syntax_only.json"),
        include_str!("fixtures/stan/includes.json"),
        include_str!("fixtures/stan/raven_extensions.json"),
    ];

    for fixture in groups.into_iter().flat_map(fixtures) {
        let (state, uri, findings) = analyze(&fixture);
        assert!(
            findings.is_empty(),
            "Stan false positive in {}: {findings:#?}",
            fixture.name
        );
        assert!(
            !state
                .get_document(&uri)
                .unwrap()
                .tree
                .as_ref()
                .unwrap()
                .root_node()
                .has_error(),
            "Stan grammar rejected fixture {}",
            fixture.name
        );
    }
}

#[test]
fn compiler_syntax_error_corpus_is_detected_without_cascades_or_duplicates() {
    let expectations: HashMap<String, InvalidExpectation> =
        serde_json::from_str::<Vec<InvalidExpectation>>(include_str!(
            "fixtures/stan/invalid_expectations.json"
        ))
        .unwrap()
        .into_iter()
        .map(|expectation| (expectation.name.clone(), expectation))
        .collect();
    let mut seen = HashSet::new();
    for fixture in fixtures(include_str!("fixtures/stan/invalid.json")) {
        let (state, uri, findings) = analyze(&fixture);
        let expectation = expectations
            .get(&fixture.name)
            .unwrap_or_else(|| panic!("missing invalid expectation for {}", fixture.name));
        assert!(!expectation.expected_lines.is_empty());
        assert!(expectation.min_diagnostics <= expectation.max_diagnostics);
        seen.insert(fixture.name.clone());
        assert!(
            !findings.is_empty(),
            "missed compiler syntax error {}: {}",
            fixture.name,
            state
                .get_document(&uri)
                .unwrap()
                .tree
                .as_ref()
                .unwrap()
                .root_node()
                .to_sexp(),
        );
        assert!(
            (expectation.min_diagnostics..=expectation.max_diagnostics).contains(&findings.len()),
            "fixture {} expected {}..={} diagnostics, got {findings:#?}",
            fixture.name,
            expectation.min_diagnostics,
            expectation.max_diagnostics,
        );
        for expected_line in &expectation.expected_lines {
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.range.start.line == *expected_line),
                "fixture {} has no finding near defect line {}: {findings:#?}",
                fixture.name,
                expected_line,
            );
        }

        let mut unique = HashSet::new();
        for finding in &findings {
            assert!(
                expectation
                    .expected_lines
                    .contains(&finding.range.start.line),
                "fixture {} emitted away from its declared defect lines: {findings:#?}",
                fixture.name,
            );
            assert_eq!(finding.severity, Some(DiagnosticSeverity::ERROR));
            assert_eq!(
                finding.code,
                Some(NumberOrString::String("syntax-error".to_string()))
            );
            let line = fixture
                .code
                .lines()
                .nth(finding.range.start.line as usize)
                .unwrap_or("");
            let line_width = line.encode_utf16().count() as u32;
            assert!(finding.range.start.character <= line_width);
            assert!(finding.range.end.character <= line_width);
            assert!(unique.insert((
                finding.range.start.line,
                finding.range.start.character,
                finding.range.end.line,
                finding.range.end.character,
                finding.message.clone(),
            )));
        }
    }
    assert_eq!(
        seen.len(),
        expectations.len(),
        "orphan invalid expectations"
    );
}

#[test]
fn stan_syntax_ranges_are_exact_across_utf16_line_endings_and_eof() {
    use tower_lsp::lsp_types::{Position, Range};

    let cases = [
        (
            "ascii",
            "data { int N }\nmodel {}\n",
            Range::new(Position::new(0, 12), Position::new(0, 13)),
        ),
        (
            "bmp-before-error",
            "model { print(\"é\"); target += ; }\n",
            Range::new(Position::new(0, 29), Position::new(0, 30)),
        ),
        (
            "astral-before-error",
            "model { print(\"😀\"); target += ; }\n",
            Range::new(Position::new(0, 30), Position::new(0, 31)),
        ),
        (
            "crlf",
            "data { int N }\r\nmodel {}\r\n",
            Range::new(Position::new(0, 12), Position::new(0, 13)),
        ),
        (
            "eof-recovery",
            "model {\n  target += 1",
            Range::new(Position::new(1, 12), Position::new(1, 13)),
        ),
    ];

    for (name, code, expected) in cases {
        let fixture = Fixture {
            name: name.to_string(),
            code: code.to_string(),
        };
        let (_, _, findings) = analyze(&fixture);
        assert_eq!(
            findings.len(),
            1,
            "{name} should produce one focused finding: {findings:#?}"
        );
        assert_eq!(findings[0].range, expected, "wrong exact range for {name}");
        assert_ne!(
            findings[0].range,
            Range::new(Position::new(0, 0), Position::new(0, 0)),
            "non-empty defects must never collapse to the document origin"
        );
    }
}
