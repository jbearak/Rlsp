use std::collections::HashMap;
use std::collections::HashSet;
use std::process::Command;

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

#[derive(Deserialize)]
struct SemanticScopeFixture {
    name: String,
    code: String,
    missing: String,
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
fn compiler_valid_corpora_have_no_false_positives() {
    let groups = [
        include_str!("fixtures/stan/valid.json"),
        include_str!("fixtures/stan/generated.json"),
        include_str!("fixtures/stan/includes.json"),
        include_str!("fixtures/stan/raven_extensions.json"),
        include_str!("fixtures/stan/semantic_scope_valid.json"),
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
fn focused_semantic_scope_oracle_reports_each_single_missing_name() {
    let fixtures: Vec<SemanticScopeFixture> =
        serde_json::from_str(include_str!("fixtures/stan/semantic_scope.json")).unwrap();
    for expected in fixtures {
        let fixture = Fixture {
            name: expected.name.clone(),
            code: expected.code,
        };
        let (state, uri, findings) = analyze(&fixture);
        assert!(
            !state
                .get_document(&uri)
                .unwrap()
                .tree
                .as_ref()
                .unwrap()
                .root_node()
                .has_error(),
            "focused semantic fixture {} must remain syntax-valid",
            expected.name
        );
        assert_eq!(findings.len(), 1, "{}: {findings:#?}", expected.name);
        assert_eq!(
            findings[0].message,
            format!("{} is not defined", expected.missing),
            "{}",
            expected.name
        );
    }
}

#[test]
fn compiler_semantic_only_corpus_reports_only_clear_undeclared_variables() {
    let expected: HashMap<&str, &[&str]> = HashMap::from([
        ("unknown-variable", &["unknown_value"][..]),
        ("invalid-bound-reference", &["missing_bound"][..]),
    ]);
    for fixture in fixtures(include_str!("fixtures/stan/syntax_only.json")) {
        let (state, uri, findings) = analyze(&fixture);
        assert!(
            !state
                .get_document(&uri)
                .unwrap()
                .tree
                .as_ref()
                .unwrap()
                .root_node()
                .has_error(),
            "Stan grammar rejected semantic fixture {}",
            fixture.name
        );
        let observed: Vec<_> = findings
            .iter()
            .filter(|finding| {
                finding.code == Some(NumberOrString::String("undefined-variable".to_string()))
            })
            .map(|finding| {
                finding
                    .message
                    .strip_suffix(" is not defined")
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            observed,
            expected.get(fixture.name.as_str()).copied().unwrap_or(&[]),
            "semantic fixture {} drifted: {findings:#?}",
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

#[test]
fn malformed_stan_diagnostics_use_the_shared_default_cap() {
    let mut code = String::new();
    for index in 0..550 {
        code.push_str(&format!("model {{ print({index}); target += ; }}\n"));
    }
    let fixture = Fixture {
        name: "default-diagnostic-cap".to_string(),
        code,
    };
    let (_, _, findings) = analyze(&fixture);
    assert_eq!(findings.len(), 500, "cap fixture drifted: {findings:#?}");
}

#[test]
fn stan_semantic_severity_off_and_syntax_cap_independence() {
    let mut code = String::from("model {\n");
    for index in 0..525 {
        code.push_str(&format!("  target += missing_{index};\n"));
    }
    code.push_str("}\n");

    let analyze_with = |syntax_cap: usize, severity: Option<DiagnosticSeverity>| {
        let uri = Url::parse(&format!("untitled:stan-semantic-cap-{syntax_cap}")).unwrap();
        let mut state = WorldState::new();
        state.cross_file_config.max_syntax_diagnostics_per_file = syntax_cap;
        state.cross_file_config.undefined_variable_severity = severity;
        state.open_document_with_language_id(uri.clone(), &code, Some(1), Some("stan"));
        diagnostics(&state, &uri, &DiagCancelToken::never())
    };

    let cap_one = analyze_with(1, Some(DiagnosticSeverity::INFORMATION));
    let syntax_unlimited = analyze_with(0, Some(DiagnosticSeverity::INFORMATION));
    assert_eq!(cap_one, syntax_unlimited);
    assert_eq!(cap_one.len(), 500);
    assert!(
        cap_one
            .iter()
            .all(|finding| finding.severity == Some(DiagnosticSeverity::INFORMATION))
    );
    assert!(analyze_with(1, None).is_empty());
}

#[test]
fn stan_semantic_ranges_use_identifier_utf16_columns() {
    let fixture = Fixture {
        name: "semantic-utf16".to_string(),
        code: "model { print(\"😀\"); target += missing; }\n".to_string(),
    };
    let (_, _, findings) = analyze(&fixture);
    assert_eq!(findings.len(), 1, "{findings:#?}");
    let byte = fixture.code.find("missing").unwrap();
    let expected_column = fixture.code[..byte].encode_utf16().count() as u32;
    assert_eq!(findings[0].range.start.line, 0);
    assert_eq!(findings[0].range.start.character, expected_column);
    assert_eq!(
        findings[0].range.end.character,
        expected_column + "missing".encode_utf16().count() as u32
    );
}

#[test]
fn large_stan_semantic_file_stays_bounded() {
    let mut code = String::from("model {\n");
    while code.len() < 256 * 1024 {
        let index = code.len();
        code.push_str(&format!("  target += missing_{index};\n"));
    }
    code.push_str("}\n");
    let fixture = Fixture {
        name: "large-semantic-performance".to_string(),
        code,
    };
    let (_, _, findings) = analyze(&fixture);
    assert_eq!(findings.len(), 500);
}

#[test]
fn raven_check_stan_undefined_variable_matches_text_json_and_sarif() {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("undefined.stan"),
        "data { int N; }\nmodel {\n  target += N + supplied_from_r;\n  target += another_missing;\n}\n",
    )
    .unwrap();

    for format in ["text", "json", "sarif"] {
        let output = Command::new(env!("CARGO_BIN_EXE_raven"))
            .args(["check", "--workspace"])
            .arg(workspace.path())
            .args(["--no-config", "--format", format, "--quiet", "--no-color"])
            .output()
            .expect("run raven check for Stan undefined variable");
        assert_eq!(
            output.status.code(),
            Some(1),
            "{format} output must use the findings exit; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        match format {
            "text" => {
                let first = stdout.find("undefined.stan:3:17").expect("first location");
                let second = stdout.find("undefined.stan:4:13").expect("second location");
                assert!(
                    first < second,
                    "text findings must stay source ordered: {stdout}"
                );
                assert!(stdout.contains("warning:"), "{stdout}");
                assert!(
                    stdout.contains("supplied_from_r is not defined"),
                    "{stdout}"
                );
                assert!(
                    stdout.contains("another_missing is not defined"),
                    "{stdout}"
                );
                assert!(stdout.contains("[undefined-variable]"), "{stdout}");
            }
            "json" => {
                let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                let findings = report.as_array().unwrap();
                assert_eq!(findings.len(), 2, "{stdout}");
                let expected = [
                    ("supplied_from_r is not defined", 2, 16, 31),
                    ("another_missing is not defined", 3, 12, 27),
                ];
                for (finding, (message, line, start, end)) in findings.iter().zip(expected) {
                    assert_eq!(finding["path"], "undefined.stan");
                    assert_eq!(finding["diagnostic"]["code"], "undefined-variable");
                    assert_eq!(finding["diagnostic"]["severity"], 2);
                    assert_eq!(finding["diagnostic"]["message"], message);
                    assert_eq!(finding["diagnostic"]["range"]["start"]["line"], line);
                    assert_eq!(finding["diagnostic"]["range"]["start"]["character"], start);
                    assert_eq!(finding["diagnostic"]["range"]["end"]["line"], line);
                    assert_eq!(finding["diagnostic"]["range"]["end"]["character"], end);
                    assert!(
                        finding["diagnostic"].get("data").is_none(),
                        "internal Stan identifiers must not leak into CLI JSON: {finding}"
                    );
                }
            }
            "sarif" => {
                let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                let results = report["runs"][0]["results"].as_array().unwrap();
                assert_eq!(results.len(), 2, "{stdout}");
                let expected = [
                    ("supplied_from_r is not defined", 3, 17, 32),
                    ("another_missing is not defined", 4, 13, 28),
                ];
                for (result, (message, line, start, end)) in results.iter().zip(expected) {
                    assert_eq!(result["ruleId"], "undefined-variable");
                    assert_eq!(result["level"], "warning");
                    assert_eq!(result["message"]["text"], message);
                    let physical = &result["locations"][0]["physicalLocation"];
                    assert_eq!(physical["artifactLocation"]["uri"], "undefined.stan");
                    assert_eq!(physical["region"]["startLine"], line);
                    assert_eq!(physical["region"]["startColumn"], start);
                    assert_eq!(physical["region"]["endLine"], line);
                    assert_eq!(physical["region"]["endColumn"], end);
                }
            }
            _ => unreachable!(),
        }
    }
}
