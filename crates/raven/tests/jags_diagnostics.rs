use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::process::Command;

use raven::handlers::{DiagCancelToken, diagnostics};
use raven::state::WorldState;
use serde::Deserialize;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Url};

#[derive(Deserialize)]
struct QualityManifest {
    cases: Vec<QualityCase>,
}

#[derive(Deserialize)]
struct QualityCase {
    id: String,
    group: String,
    source: String,
    expect_parse: String,
}

#[derive(Deserialize)]
struct MatrixManifest {
    probes: Vec<MatrixCase>,
}

#[derive(Deserialize)]
struct MatrixCase {
    name: String,
    source: String,
    expect_parse: String,
    encoding: Option<String>,
}

#[derive(Deserialize)]
struct InvalidExpectation {
    lines: Vec<u32>,
    min_count: usize,
    max_count: usize,
}

fn analyze(state: &mut WorldState, uri: &Url, source: &str, language_id: &str) -> Vec<Diagnostic> {
    state.open_document_with_language_id(uri.clone(), source, Some(1), Some(language_id));
    diagnostics(state, uri, &DiagCancelToken::never())
}

fn assert_clean_jags_document(state: &WorldState, uri: &Url, name: &str, source: &str) {
    let document = state.get_document(uri).unwrap_or_else(|| {
        panic!(
            "JAGS case {name} has no Document at {uri}; source: {}",
            source.escape_debug()
        )
    });
    let tree = document.tree.as_ref().unwrap_or_else(|| {
        panic!(
            "JAGS case {name} has a Document but no syntax tree at {uri}; source: {}",
            source.escape_debug()
        )
    });
    let root = tree.root_node();
    assert_eq!(
        root.kind(),
        "program",
        "JAGS case {name} has the wrong root kind at {uri}; root: {}; source: {}",
        root.to_sexp(),
        source.escape_debug()
    );
    assert!(
        !root.has_error(),
        "JAGS grammar rejected case {name} at {uri}; root: {}; source: {}",
        root.to_sexp(),
        source.escape_debug()
    );
    assert!(
        source
            .get(..root.start_byte())
            .is_some_and(|prefix| prefix.trim_matches(char::is_whitespace).is_empty()),
        "JAGS case {name} has non-whitespace source before root byte range {:?} at {uri}; root: {}; source: {}",
        root.byte_range(),
        root.to_sexp(),
        source.escape_debug()
    );
    assert!(
        source
            .get(root.end_byte()..)
            .is_some_and(|suffix| suffix.trim_matches(char::is_whitespace).is_empty()),
        "JAGS case {name} has non-whitespace source after root byte range {:?} at {uri}; root: {}; source: {}",
        root.byte_range(),
        root.to_sexp(),
        source.escape_debug()
    );
}

fn assert_syntax_only_in_bounds(name: &str, source: &str, findings: &[Diagnostic]) {
    let mut unique = HashSet::new();
    assert!(findings.len() <= 500, "{name} exceeded the diagnostic cap");
    for finding in findings {
        assert_eq!(finding.severity, Some(DiagnosticSeverity::ERROR), "{name}");
        assert_eq!(
            finding.code,
            Some(NumberOrString::String("syntax-error".to_string())),
            "{name}"
        );
        assert!(finding.message.contains("JAGS"), "{name}: {finding:?}");
        let line = source
            .split('\n')
            .nth(finding.range.start.line as usize)
            .unwrap_or("")
            .trim_end_matches('\r');
        let width = line.encode_utf16().count() as u32;
        assert!(
            finding.range.start.character <= width,
            "{name}: {finding:?}"
        );
        if finding.range.end.line == finding.range.start.line {
            assert!(finding.range.end.character <= width, "{name}: {finding:?}");
        }
        assert!(
            unique.insert((
                finding.range.start.line,
                finding.range.start.character,
                finding.range.end.line,
                finding.range.end.character,
                finding.message.clone(),
            )),
            "duplicate finding in {name}: {finding:?}"
        );
    }
}

#[test]
fn all_committed_oracle_outcomes_map_to_raven_diagnostics() {
    let quality: QualityManifest = serde_json::from_str(include_str!(
        "../../tree-sitter-jags/oracle/quality-corpus.json"
    ))
    .expect("checked-in JAGS quality corpus must parse");
    let matrix: MatrixManifest = serde_json::from_str(include_str!(
        "../../tree-sitter-jags/oracle/syntax-matrix.json"
    ))
    .expect("checked-in JAGS syntax matrix must parse");

    assert_eq!(quality.cases.len(), 683, "quality corpus count drifted");
    assert_eq!(matrix.probes.len(), 123, "syntax matrix count drifted");

    for extension in ["jags", "bugs", "bug"] {
        let uri = Url::parse(&format!("file:///tmp/jags-oracle.{extension}")).unwrap();
        let mut state = WorldState::new();
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        let mut curated_invalid = 0usize;
        let mut mutations = 0usize;

        for case in &quality.cases {
            let findings = analyze(&mut state, &uri, &case.source, "jags");
            assert_syntax_only_in_bounds(&case.id, &case.source, &findings);
            if case.expect_parse == "accepted" {
                accepted += 1;
                assert_clean_jags_document(&state, &uri, &case.id, &case.source);
                assert!(
                    findings.is_empty(),
                    "false positive for {} ({} as .{extension}) at {uri}; source: {}; findings: {findings:#?}",
                    case.id,
                    case.group,
                    case.source.escape_debug()
                );
            } else {
                rejected += 1;
                curated_invalid += usize::from(case.group == "syntax-invalid");
                mutations += usize::from(case.group == "mutation");
                assert!(
                    !findings.is_empty(),
                    "missed rejected case {} ({} as .{extension}): {}",
                    case.id,
                    case.group,
                    state
                        .get_document(&uri)
                        .and_then(|document| document.tree.as_ref())
                        .map(|tree| tree.root_node().to_sexp())
                        .unwrap_or_default()
                );
            }
        }

        for case in &matrix.probes {
            let source = if case.encoding.as_deref() == Some("utf-8-bom") {
                format!("\u{feff}{}", case.source)
            } else {
                case.source.clone()
            };
            let findings = analyze(&mut state, &uri, &source, "jags");
            assert_syntax_only_in_bounds(&case.name, &source, &findings);
            if case.expect_parse == "accepted" {
                accepted += 1;
                assert_clean_jags_document(&state, &uri, &case.name, &source);
                assert!(
                    findings.is_empty(),
                    "false positive for syntax-matrix case {} as .{extension} at {uri}; source: {}; findings: {findings:#?}",
                    case.name,
                    source.escape_debug()
                );
            } else {
                rejected += 1;
                assert!(
                    !findings.is_empty(),
                    "missed syntax-matrix rejection {} as .{extension}",
                    case.name
                );
            }
        }

        assert_eq!((accepted, rejected), (460, 346), ".{extension}");
        assert_eq!(curated_invalid, 75, ".{extension}");
        assert_eq!(mutations, 200, ".{extension}");
    }
}

#[test]
fn curated_invalid_cases_hit_only_their_declared_defect_lines() {
    let quality: QualityManifest = serde_json::from_str(include_str!(
        "../../tree-sitter-jags/oracle/quality-corpus.json"
    ))
    .expect("checked-in JAGS quality corpus must parse");
    let expectations: BTreeMap<String, InvalidExpectation> =
        serde_json::from_str(include_str!("fixtures/jags/invalid_expectations.json"))
            .expect("checked-in curated defect expectations must parse");
    let invalid_cases = quality
        .cases
        .iter()
        .filter(|case| case.group == "syntax-invalid")
        .map(|case| (case.id.as_str(), case))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        invalid_cases.len(),
        75,
        "curated invalid corpus count drifted"
    );
    assert_eq!(expectations.len(), 75, "expectation count drifted");
    let corpus_ids = invalid_cases.keys().copied().collect::<BTreeSet<_>>();
    let expectation_ids = expectations
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        corpus_ids, expectation_ids,
        "expectations must have exactly one key for every curated invalid ID"
    );

    for extension in ["jags", "bugs", "bug"] {
        let uri = Url::parse(&format!("file:///tmp/curated-invalid.{extension}")).unwrap();
        let mut state = WorldState::new();
        for (id, case) in &invalid_cases {
            let expectation = &expectations[*id];
            assert!(
                expectation.min_count > 0 && expectation.min_count <= expectation.max_count,
                "invalid count bounds for {id}"
            );
            let declared_lines = expectation.lines.iter().copied().collect::<BTreeSet<_>>();
            assert_eq!(
                declared_lines.len(),
                expectation.lines.len(),
                "duplicate declared defect line for {id}"
            );
            assert!(
                declared_lines
                    .iter()
                    .all(|line| (*line as usize) < case.source.split('\n').count()),
                "out-of-bounds declared defect line for {id}"
            );

            let findings = analyze(&mut state, &uri, &case.source, "jags");
            assert!(
                (expectation.min_count..=expectation.max_count).contains(&findings.len()),
                "{id} as .{extension}: expected {}..={} diagnostics, got {findings:#?}",
                expectation.min_count,
                expectation.max_count
            );
            let actual_lines = findings
                .iter()
                .map(|finding| finding.range.start.line)
                .collect::<BTreeSet<_>>();
            assert_eq!(
                actual_lines, declared_lines,
                "{id} as .{extension}: every defect line must be hit and unrelated lines must stay silent"
            );
        }
    }
}

#[test]
fn jags_extensions_mixed_case_and_untitled_jags_are_strict() {
    let invalid = "model { x <- * 1 }\n";
    let mut state = WorldState::new();
    for uri in [
        Url::parse("file:///tmp/model.bugs").unwrap(),
        Url::parse("file:///tmp/model.BUGS").unwrap(),
        Url::parse("file:///tmp/model.bug").unwrap(),
        Url::parse("file:///tmp/model.BUG").unwrap(),
    ] {
        assert!(!analyze(&mut state, &uri, invalid, "jags").is_empty());
    }

    let untitled = Url::parse("untitled:Untitled-1").unwrap();
    assert!(!analyze(&mut state, &untitled, invalid, "jags").is_empty());
}

#[test]
fn cancelled_jags_diagnostics_fail_closed() {
    let uri = Url::parse("untitled:cancelled-jags").unwrap();
    let mut state = WorldState::new();
    state.open_document_with_language_id(
        uri.clone(),
        "model { x <- * 1 }\n",
        Some(1),
        Some("jags"),
    );
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    assert!(diagnostics(&state, &uri, &DiagCancelToken::from_token(token)).is_empty());
}

#[test]
fn jags_ranges_are_exact_for_bom_crlf_astral_and_eof_recovery() {
    let cases = [
        (
            "ascii",
            "model { x <- * 1 }\n",
            Range::new(Position::new(0, 13), Position::new(0, 14)),
        ),
        (
            "crlf",
            "model { x <- * 1 }\r\n",
            Range::new(Position::new(0, 13), Position::new(0, 14)),
        ),
        (
            "astral-before-error",
            "/* 💥 */ model { x <- * 1 }\n",
            Range::new(Position::new(0, 22), Position::new(0, 23)),
        ),
        (
            "eof-recovery",
            "model { x <- 1",
            Range::new(Position::new(0, 13), Position::new(0, 14)),
        ),
        (
            "raw-bom",
            "\u{feff}model { x <- 1 }\n",
            Range::new(Position::new(0, 0), Position::new(0, 1)),
        ),
    ];

    let uri = Url::parse("untitled:jags-ranges").unwrap();
    let mut state = WorldState::new();
    for (name, source, expected) in cases {
        let findings = analyze(&mut state, &uri, source, "jags");
        assert_eq!(findings.len(), 1, "{name}: {findings:#?}");
        assert_eq!(findings[0].range, expected, "wrong range for {name}");
    }
}

#[test]
fn leading_and_trailing_whitespace_are_not_root_coverage_errors() {
    let source = "\n\t model { x <- 1 }\n\n";
    let mut state = WorldState::new();
    for uri in [
        Url::parse("file:///tmp/whitespace.jags").unwrap(),
        Url::parse("file:///tmp/whitespace.bugs").unwrap(),
        Url::parse("file:///tmp/whitespace.bug").unwrap(),
    ] {
        let findings = analyze(&mut state, &uri, source, "jags");
        assert_clean_jags_document(&state, &uri, "leading-and-trailing-whitespace", source);
        assert!(
            findings.is_empty(),
            "grammar extras outside the named root must remain valid at {uri}; source: {}; findings: {findings:#?}",
            source.escape_debug()
        );
    }
}

#[test]
fn malformed_jags_diagnostics_use_the_shared_default_cap() {
    let mut source = String::from("model {\n");
    for index in 0..550 {
        source.push_str(&format!("  x{index} <- * 1\n"));
    }
    source.push_str("}\n");
    let uri = Url::parse("untitled:jags-diagnostic-cap").unwrap();
    let findings = analyze(&mut WorldState::new(), &uri, &source, "jags");
    assert_eq!(findings.len(), 500, "cap fixture drifted: {findings:#?}");
}

#[test]
fn raven_check_bug_text_json_and_sarif_outputs_fail_with_syntax_error() {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("invalid.BUG"), "model { x <- * 1 }\n").unwrap();

    for format in ["text", "json", "sarif"] {
        let output = Command::new(env!("CARGO_BIN_EXE_raven"))
            .args(["check", "--workspace"])
            .arg(workspace.path())
            .args(["--no-config", "--format", format, "--quiet", "--no-color"])
            .output()
            .expect("run raven check for a singular BUG syntax error");
        assert_eq!(
            output.status.code(),
            Some(1),
            "{format} output must use the findings exit; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("CLI output must be UTF-8");

        match format {
            "text" => {
                assert!(stdout.contains("invalid.BUG"), "{stdout}");
                assert!(stdout.contains("error:"), "{stdout}");
                assert!(
                    stdout.contains("JAGS code could not be parsed here"),
                    "{stdout}"
                );
                assert!(stdout.contains("[syntax-error]"), "{stdout}");
            }
            "json" => {
                let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                let finding = &value.as_array().unwrap()[0];
                assert_eq!(finding["path"], "invalid.BUG");
                assert_eq!(finding["diagnostic"]["code"], "syntax-error");
                assert_eq!(finding["diagnostic"]["severity"], 1);
            }
            "sarif" => {
                let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(value["version"], "2.1.0");
                let result = &value["runs"][0]["results"][0];
                assert_eq!(result["ruleId"], "syntax-error");
                assert_eq!(result["level"], "error");
                assert_eq!(
                    result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
                    "invalid.BUG"
                );
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn raven_check_applies_one_project_cap_to_stan_and_jags_in_every_format() {
    use std::collections::BTreeMap;

    let workspace = tempfile::TempDir::new().unwrap();
    let mut jags = String::from("model {\n");
    let mut stan = String::new();
    for index in 0..8 {
        jags.push_str(&format!("  x{index} <- * 1\n"));
        stan.push_str(&format!("model {{ print({index}); target += ; }}\n"));
    }
    jags.push_str("}\n");
    std::fs::write(workspace.path().join("invalid.jags"), jags).unwrap();
    std::fs::write(workspace.path().join("invalid.stan"), stan).unwrap();
    std::fs::write(
        workspace.path().join("raven.toml"),
        "[diagnostics]\nmaxSyntaxDiagnosticsPerFile = 3\n",
    )
    .unwrap();
    let expected_paths = ["invalid.jags", "invalid.stan"];

    for format in ["text", "json", "sarif"] {
        let output = Command::new(env!("CARGO_BIN_EXE_raven"))
            .args(["check", "--workspace"])
            .arg(workspace.path())
            .args(["--format", format, "--quiet", "--no-color"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{format}: {output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let mut counts = BTreeMap::<String, usize>::new();
        match format {
            "text" => {
                for line in stdout
                    .lines()
                    .filter(|line| line.ends_with("[syntax-error]"))
                {
                    let path = expected_paths
                        .iter()
                        .find(|path| line.starts_with(&format!("{path}:")))
                        .unwrap_or_else(|| panic!("unexpected text finding: {line}"));
                    *counts.entry((*path).to_string()).or_default() += 1;
                }
            }
            "json" => {
                let findings = serde_json::from_str::<serde_json::Value>(&stdout).unwrap();
                for finding in findings.as_array().unwrap() {
                    assert_eq!(finding["diagnostic"]["code"], "syntax-error");
                    let path = finding["path"].as_str().unwrap();
                    *counts.entry(path.to_string()).or_default() += 1;
                }
            }
            "sarif" => {
                let report = serde_json::from_str::<serde_json::Value>(&stdout).unwrap();
                for result in report["runs"][0]["results"].as_array().unwrap() {
                    assert_eq!(result["ruleId"], "syntax-error");
                    let path =
                        result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
                            .as_str()
                            .unwrap();
                    *counts.entry(path.to_string()).or_default() += 1;
                }
            }
            _ => unreachable!(),
        }
        let observed_paths: Vec<_> = counts.keys().map(String::as_str).collect();
        assert_eq!(
            observed_paths, expected_paths,
            "{format} emitted findings for an unexpected path: {stdout}"
        );
        assert_eq!(counts.values().sum::<usize>(), 6);
        for path in expected_paths {
            assert_eq!(
                counts.get(path),
                Some(&3),
                "{format} must retain three findings for {path}: {stdout}"
            );
        }
    }

    let run_json_with_cap = |configured_cap: usize| {
        std::fs::write(
            workspace.path().join("raven.toml"),
            format!("[diagnostics]\nmaxSyntaxDiagnosticsPerFile = {configured_cap}\n"),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_raven"))
            .args(["check", "--workspace"])
            .arg(workspace.path())
            .args(["--format", "json", "--quiet", "--no-color"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let findings = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
        let mut counts = BTreeMap::<String, usize>::new();
        for finding in findings.as_array().unwrap() {
            assert_eq!(finding["diagnostic"]["code"], "syntax-error");
            let path = finding["path"].as_str().unwrap();
            *counts.entry(path.to_string()).or_default() += 1;
        }
        counts
    };

    let unlimited = run_json_with_cap(0);
    for path in expected_paths {
        assert!(
            unlimited.get(path).is_some_and(|count| *count > 2),
            "unlimited baseline must exercise truncation for {path}: {unlimited:?}"
        );
    }

    let high_cap = 20;
    assert!(
        unlimited.values().all(|count| *count < high_cap),
        "fixture baseline must remain below the verified high cap: {unlimited:?}"
    );
    assert_eq!(run_json_with_cap(high_cap), unlimited);

    let low_cap = 2;
    let low = run_json_with_cap(low_cap);
    let expected_low_total: usize = expected_paths
        .iter()
        .map(|path| unlimited[*path].min(low_cap))
        .sum();
    assert_eq!(low.values().sum::<usize>(), expected_low_total);
    for path in expected_paths {
        assert_eq!(
            low.get(path).copied().unwrap_or(0),
            unlimited[path].min(low_cap),
            "configured cap {low_cap} must apply independently to {path}"
        );
    }
}
