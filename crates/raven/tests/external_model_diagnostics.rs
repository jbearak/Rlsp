//! Fetch-only holdout corpus for Stan and JAGS diagnostics.
//!
//! The non-ignored tests validate the committed source manifests without network
//! access. The ignored tests consume oracle-verified material under
//! `target/diagnostic-corpora`; see `docs/diagnostic-corpora.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use raven::file_type::FileType;
use raven::handlers::{DiagCancelToken, diagnostics};
use raven::state::WorldState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower_lsp::lsp_types::Url;

const STAN_MANIFEST: &str = include_str!("fixtures/diagnostic_corpora/stan.json");
const JAGS_MANIFEST: &str = include_str!("fixtures/diagnostic_corpora/jags.json");

#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    language: String,
    suite: String,
    sources: Vec<Source>,
}

#[derive(Debug, Deserialize)]
struct Source {
    id: String,
    revision: String,
    archive_url: String,
    archive_sha256: String,
    archive_root: String,
    redistribution: String,
    license: License,
    discovery: Vec<Discovery>,
}

#[derive(Debug, Deserialize)]
struct License {
    spdx: String,
    evidence_url: String,
}

#[derive(Debug, Deserialize)]
struct Discovery {
    #[serde(rename = "type")]
    kind: String,
    globs: Vec<String>,
    expected_count: usize,
    raven_mode: String,
    oracle_mode: String,
}

#[derive(Debug, Deserialize)]
struct OracleReport {
    schema_version: u32,
    inputs: OracleInputs,
    counts: OracleCounts,
    verified_cases: Vec<VerifiedCase>,
    outcomes: Vec<OracleOutcome>,
    failures: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OracleInputs {
    manifest_sha256: String,
    materialized_index_sha256: String,
}

#[derive(Debug, Deserialize)]
struct OracleCounts {
    total: usize,
    accepted_direct: usize,
    #[serde(default)]
    accepted_wrapped: usize,
    rejected: usize,
    verified: usize,
}

#[derive(Debug, Deserialize)]
struct OracleOutcome {
    id: String,
    source_id: String,
    materialized_path: String,
    sha256: String,
    kind: String,
    raven_mode: String,
    oracle_mode: String,
    outcome: String,
    wrapper_id: Option<String>,
    syntax_accepted: bool,
}

#[derive(Debug, Deserialize)]
struct VerifiedCase {
    id: String,
    materialized_path: String,
    sha256: String,
    raven_mode: String,
    #[serde(default)]
    wrapper_id: Option<String>,
    source: VerifiedSource,
}

#[derive(Debug, Deserialize)]
struct VerifiedSource {
    materialized_path: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct MaterializedIndex {
    schema_version: u32,
    manifest_binding: Vec<ManifestBinding>,
    cases: Vec<IndexCase>,
    counts: IndexCounts,
}

#[derive(Debug, Deserialize)]
struct ManifestBinding {
    path: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct IndexCase {
    id: String,
    language: String,
    source_id: String,
    materialized_path: String,
    sha256: String,
    kind: String,
    raven_mode: String,
    oracle_mode: String,
}

#[derive(Debug, Deserialize)]
struct IndexCounts {
    total: usize,
    stan: usize,
    jags: usize,
}

#[derive(Debug, Serialize)]
struct RunReport {
    language: String,
    oracle_report: String,
    verified_cases: usize,
    passed: usize,
    failed: usize,
    elapsed_ms: u128,
    failures: Vec<String>,
}

fn parse_manifest(source: &str) -> Manifest {
    serde_json::from_str(source).expect("checked-in diagnostic corpus manifest must parse")
}

fn assert_manifest(manifest: &Manifest, expected_language: &str, expected_cases: usize) {
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.language, expected_language);
    assert_eq!(manifest.suite, "external-no-false-positive");
    assert!(!manifest.sources.is_empty());

    let mut source_ids = BTreeSet::new();
    let mut count = 0usize;
    for source in &manifest.sources {
        assert!(
            source_ids.insert(&source.id),
            "duplicate source id {}",
            source.id
        );
        assert!(!source.revision.is_empty(), "{} revision", source.id);
        assert!(
            source.archive_url.starts_with("https://"),
            "{} URL",
            source.id
        );
        assert_eq!(source.archive_sha256.len(), 64, "{} SHA-256", source.id);
        assert!(
            source
                .archive_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "{} SHA-256 must be lowercase hex",
            source.id
        );
        assert!(
            !source.archive_root.is_empty(),
            "{} archive root",
            source.id
        );
        assert_eq!(source.redistribution, "fetch-only", "{}", source.id);
        assert!(!source.license.spdx.is_empty(), "{} license", source.id);
        assert!(
            source.license.evidence_url.starts_with("https://"),
            "{} license evidence",
            source.id
        );
        assert!(!source.discovery.is_empty(), "{} discovery", source.id);
        for discovery in &source.discovery {
            assert!(matches!(discovery.kind.as_str(), "files" | "qmd-fences"));
            assert!(!discovery.globs.is_empty(), "{} globs", source.id);
            assert!(discovery.expected_count > 0, "{} expected count", source.id);
            assert!(
                matches!(
                    discovery.raven_mode.as_str(),
                    "all" | "syntax-only" | "oracle-classified"
                ),
                "{} Raven mode",
                source.id
            );
            assert!(
                !discovery.oracle_mode.is_empty(),
                "{} oracle mode",
                source.id
            );
            count += discovery.expected_count;
        }
    }
    assert_eq!(
        count, expected_cases,
        "{expected_language} corpus count drifted"
    );
}

#[test]
fn external_corpus_manifests_are_pinned_and_complete() {
    assert_manifest(&parse_manifest(STAN_MANIFEST), "stan", 2_155);
    assert_manifest(&parse_manifest(JAGS_MANIFEST), "jags", 60);
}

fn corpus_root() -> PathBuf {
    env::var_os("RAVEN_DIAGNOSTIC_CORPUS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/diagnostic-corpora")
        })
}

fn safe_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path.is_absolute() || value.is_empty() || value.contains('\\') {
        return Err(format!("unsafe materialized path {value:?}"));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(format!("unsafe materialized path {value:?}"));
        }
    }
    Ok(path.to_path_buf())
}

fn sha256_bytes(source: &[u8]) -> String {
    format!("{:x}", Sha256::digest(source))
}

fn read_regular_corpus_file(root: &Path, id: &str, relative_path: &str) -> Result<Vec<u8>, String> {
    let relative = safe_relative_path(relative_path)?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("{id}: cannot stat {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{}: source is not a regular file: {}",
            id,
            path.display()
        ));
    }
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("cannot canonicalize {}: {error}", root.display()))?;
    let canonical_path = fs::canonicalize(&path)
        .map_err(|error| format!("{}: cannot canonicalize {}: {error}", id, path.display()))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(format!("{}: source escapes corpus root", id));
    }
    fs::read(&canonical_path)
        .map_err(|error| format!("{}: cannot read {}: {error}", id, path.display()))
}

fn assert_clean_tree(
    state: &WorldState,
    uri: &Url,
    source: &str,
    expected_file_type: FileType,
) -> Result<(), String> {
    let document = state
        .get_document(uri)
        .ok_or_else(|| "Raven did not retain the opened document".to_string())?;
    if document.file_type != expected_file_type {
        return Err(format!(
            "Raven classified the document as {:?}, expected {:?}",
            document.file_type, expected_file_type
        ));
    }
    let tree = document
        .tree
        .as_ref()
        .ok_or_else(|| "Raven did not produce a parse tree".to_string())?;
    let root = tree.root_node();
    if root.kind() != "program" {
        return Err(format!("unexpected root kind {:?}", root.kind()));
    }
    if root.has_error() {
        return Err(format!("parse tree contains recovery: {}", root.to_sexp()));
    }
    let analysis = document.analysis_text();
    let prefix = analysis.get(..root.start_byte()).unwrap_or_default();
    let suffix = analysis.get(root.end_byte()..).unwrap_or_default();
    let prefix = prefix.strip_prefix('\u{feff}').unwrap_or(prefix);
    if !prefix.trim().is_empty() || !suffix.trim().is_empty() {
        return Err(format!(
            "parse root does not cover meaningful input: bytes {}..{} of {}",
            root.start_byte(),
            root.end_byte(),
            source.len()
        ));
    }
    Ok(())
}

fn validate_oracle_report(
    root: &Path,
    language: &str,
    oracle: &OracleReport,
) -> Result<(), String> {
    let (manifest_source, expected_total, expected_verified) = match language {
        "stan" => (STAN_MANIFEST, 2_155, 2_025),
        "jags" => (JAGS_MANIFEST, 60, 59),
        _ => return Err(format!("unsupported external language {language:?}")),
    };
    if oracle.schema_version != 1 {
        return Err(format!("{language} oracle schema_version must be 1"));
    }
    if !oracle.failures.is_empty() {
        return Err(format!(
            "{language} oracle reports failures: {}",
            oracle.failures.join("; ")
        ));
    }
    if oracle.counts.total != expected_total
        || oracle.outcomes.len() != expected_total
        || oracle.counts.verified != expected_verified
        || oracle.verified_cases.len() != expected_verified
    {
        return Err(format!(
            "{language} oracle counts drifted: expected total={expected_total} verified={expected_verified}, got counts total={} verified={}, outcomes={} verified_cases={}",
            oracle.counts.total,
            oracle.counts.verified,
            oracle.outcomes.len(),
            oracle.verified_cases.len()
        ));
    }
    if oracle.counts.accepted_direct + oracle.counts.accepted_wrapped != oracle.counts.verified
        || oracle.counts.verified + oracle.counts.rejected != oracle.counts.total
        || (language == "jags" && oracle.counts.accepted_wrapped != 0)
    {
        return Err(format!(
            "{language} oracle count accounting is inconsistent"
        ));
    }

    let materialized_root = root.join("materialized");
    let index_path = materialized_root.join("index.json");
    let index_source = fs::read(&index_path)
        .map_err(|error| format!("cannot read {}: {error}", index_path.display()))?;
    if oracle.inputs.materialized_index_sha256 != sha256_bytes(&index_source) {
        return Err(format!(
            "{language} oracle materialized index binding drifted"
        ));
    }
    let manifest_sha256 = sha256_bytes(manifest_source.as_bytes());
    if oracle.inputs.manifest_sha256 != manifest_sha256 {
        return Err(format!("{language} oracle manifest binding drifted"));
    }
    let index: MaterializedIndex = serde_json::from_slice(&index_source)
        .map_err(|error| format!("invalid {}: {error}", index_path.display()))?;
    if index.schema_version != 1
        || index.counts.total != index.counts.stan + index.counts.jags
        || !index
            .manifest_binding
            .iter()
            .any(|binding| binding.sha256 == manifest_sha256 && !binding.path.is_empty())
    {
        return Err(format!(
            "{language} materialized index accounting or binding is invalid"
        ));
    }
    let expected_index_count = if language == "stan" {
        index.counts.stan
    } else {
        index.counts.jags
    };
    if expected_index_count != expected_total {
        return Err(format!(
            "{language} materialized index count drifted: expected {expected_total}, got {expected_index_count}"
        ));
    }

    let mut index_cases = BTreeMap::new();
    for case in index.cases.iter().filter(|case| case.language == language) {
        if index_cases.insert(case.id.as_str(), case).is_some() {
            return Err(format!("duplicate {language} index case {}", case.id));
        }
    }
    if index_cases.len() != expected_total {
        return Err(format!("{language} index case accounting is incomplete"));
    }

    let mut outcomes = BTreeMap::new();
    let mut accepted_ids = BTreeSet::new();
    let mut accepted_direct = 0usize;
    let mut accepted_wrapped = 0usize;
    let mut rejected = 0usize;
    for outcome in &oracle.outcomes {
        if outcomes.insert(outcome.id.as_str(), outcome).is_some() {
            return Err(format!(
                "duplicate {language} oracle outcome {}",
                outcome.id
            ));
        }
        let indexed = index_cases
            .get(outcome.id.as_str())
            .ok_or_else(|| format!("{}: oracle outcome is absent from the index", outcome.id))?;
        if outcome.source_id != indexed.source_id
            || outcome.materialized_path != indexed.materialized_path
            || outcome.sha256 != indexed.sha256
            || outcome.kind != indexed.kind
            || outcome.raven_mode != indexed.raven_mode
            || outcome.oracle_mode != indexed.oracle_mode
        {
            return Err(format!(
                "{}: oracle outcome does not match the index",
                outcome.id
            ));
        }
        let accepted = matches!(
            outcome.outcome.as_str(),
            "accepted-direct" | "accepted-wrapped"
        );
        if accepted != outcome.syntax_accepted
            || (accepted && outcome.wrapper_id.as_deref().is_none_or(str::is_empty))
            || (!accepted && (outcome.outcome != "rejected" || outcome.wrapper_id.is_some()))
        {
            return Err(format!(
                "{}: oracle outcome fields are inconsistent",
                outcome.id
            ));
        }
        match outcome.outcome.as_str() {
            "accepted-direct" => accepted_direct += 1,
            "accepted-wrapped" => accepted_wrapped += 1,
            "rejected" => rejected += 1,
            _ => {}
        }
        if accepted {
            accepted_ids.insert(outcome.id.as_str());
        }
    }
    if accepted_direct != oracle.counts.accepted_direct
        || accepted_wrapped != oracle.counts.accepted_wrapped
        || rejected != oracle.counts.rejected
    {
        return Err(format!(
            "{language} oracle outcome categories do not match reported counts"
        ));
    }
    if outcomes.len() != index_cases.len() || accepted_ids.len() != expected_verified {
        return Err(format!(
            "{language} oracle outcomes do not account for the index"
        ));
    }

    let mut verified_ids = BTreeSet::new();
    for case in &oracle.verified_cases {
        if !verified_ids.insert(case.id.as_str()) {
            return Err(format!("duplicate {language} verified case {}", case.id));
        }
        let outcome = outcomes
            .get(case.id.as_str())
            .ok_or_else(|| format!("{}: verified case has no outcome", case.id))?;
        if case.wrapper_id != outcome.wrapper_id
            || case.source.materialized_path
                != format!("materialized/{}", outcome.materialized_path)
            || case.source.sha256 != outcome.sha256
            || !matches!(case.raven_mode.as_str(), "all" | "syntax-only")
            || (outcome.outcome == "accepted-direct"
                && case.materialized_path != case.source.materialized_path)
            || (outcome.outcome == "accepted-wrapped"
                && (language != "stan"
                    || !case
                        .materialized_path
                        .starts_with("materialized/oracle-cases/stan/")))
        {
            return Err(format!(
                "{}: verified case does not bind to its outcome",
                case.id
            ));
        }
        let verified_bytes = read_regular_corpus_file(root, &case.id, &case.materialized_path)?;
        if sha256_bytes(&verified_bytes) != case.sha256 {
            return Err(format!("{}: verified source SHA-256 mismatch", case.id));
        }
        let source_bytes =
            read_regular_corpus_file(root, &case.id, &case.source.materialized_path)?;
        if sha256_bytes(&source_bytes) != case.source.sha256 {
            return Err(format!("{}: indexed source SHA-256 mismatch", case.id));
        }
    }
    if verified_ids != accepted_ids {
        return Err(format!(
            "{language} verified cases are not the exact accepted outcome set"
        ));
    }
    Ok(())
}

fn selected(case: &VerifiedCase) -> bool {
    let Some(filter) = env::var_os("RAVEN_DIAGNOSTIC_CORPUS_CASES") else {
        return true;
    };
    let filter = filter.to_string_lossy();
    filter
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .any(|value| case.id.contains(value))
}

fn run_external(language: &str, report_name: &str) {
    let root = corpus_root();
    let report_path = root.join("materialized").join(report_name);
    let report_source = fs::read_to_string(&report_path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}; materialize the corpus and run its external oracle first",
            report_path.display()
        )
    });
    let oracle: OracleReport = serde_json::from_str(&report_source)
        .unwrap_or_else(|error| panic!("invalid {}: {error}", report_path.display()));
    validate_oracle_report(&root, language, &oracle)
        .unwrap_or_else(|error| panic!("invalid {}: {error}", report_path.display()));

    let expected_file_type = if language == "stan" {
        FileType::Stan
    } else {
        FileType::Jags
    };
    let uri = Url::parse(&format!("untitled:raven-external-{language}"))
        .expect("static external corpus URI must parse");
    let mut state = WorldState::new();
    match language {
        "stan" => state.cross_file_config.stan_diagnostics_enabled = true,
        "jags" => state.cross_file_config.jags_diagnostics_enabled = true,
        _ => unreachable!("validated language must be Stan or JAGS"),
    }
    let mut version = 0i32;
    let mut failures = Vec::new();
    let mut selected_cases = 0usize;
    let mut passed = 0usize;
    let started = Instant::now();
    let mut seen = BTreeMap::<(&str, &str), &str>::new();

    for case in oracle.verified_cases.iter().filter(|case| selected(case)) {
        selected_cases += 1;
        if case.sha256.len() != 64 {
            failures.push(format!("{}: invalid source SHA-256", case.id));
            continue;
        }
        let dedupe_key = (case.sha256.as_str(), case.raven_mode.as_str());
        if let Some(first) = seen.insert(dedupe_key, &case.id) {
            eprintln!("alias {} reuses already checked source {first}", case.id);
            continue;
        }
        let source_bytes = match read_regular_corpus_file(&root, &case.id, &case.materialized_path)
        {
            Ok(source) => source,
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
        let actual_sha256 = sha256_bytes(&source_bytes);
        if actual_sha256 != case.sha256 {
            failures.push(format!(
                "{}: verified source SHA-256 mismatch (expected {}, got {actual_sha256})",
                case.id, case.sha256
            ));
            continue;
        }
        let source = match String::from_utf8(source_bytes) {
            Ok(source) => source,
            Err(error) => {
                failures.push(format!(
                    "{}: verified source is not UTF-8: {error}",
                    case.id
                ));
                continue;
            }
        };
        if language == "stan" {
            state.cross_file_config.undefined_variable_severity = match case.raven_mode.as_str() {
                "all" => Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING),
                "syntax-only" => None,
                other => {
                    failures.push(format!("{}: unsupported Raven mode {other:?}", case.id));
                    continue;
                }
            };
        } else if case.raven_mode != "syntax-only" {
            failures.push(format!("{}: JAGS case must be syntax-only", case.id));
            continue;
        }

        version += 1;
        state.open_document_with_language_id(uri.clone(), &source, Some(version), Some(language));
        let findings = diagnostics(&state, &uri, &DiagCancelToken::never());
        let clean = assert_clean_tree(&state, &uri, &source, expected_file_type);
        if let Err(error) = clean {
            failures.push(format!(
                "{}{}: {error}; findings={findings:#?}",
                case.id,
                case.wrapper_id
                    .as_deref()
                    .map(|wrapper| format!(" (wrapper {wrapper})"))
                    .unwrap_or_default()
            ));
        } else if !findings.is_empty() {
            failures.push(format!(
                "{}: false-positive diagnostics: {findings:#?}",
                case.id
            ));
        } else {
            passed += 1;
        }
    }

    let report = RunReport {
        language: language.to_string(),
        oracle_report: report_path.display().to_string(),
        verified_cases: selected_cases,
        passed,
        failed: failures.len(),
        elapsed_ms: started.elapsed().as_millis(),
        failures,
    };
    if let Some(directory) = env::var_os("RAVEN_DIAGNOSTIC_CORPUS_REPORT_DIR") {
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory).expect("external corpus report directory must be writable");
        let destination = directory.join(format!("{language}.json"));
        fs::write(
            &destination,
            serde_json::to_string_pretty(&report).unwrap() + "\n",
        )
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", destination.display()));
    }
    assert!(
        report.failures.is_empty(),
        "{} external corpus failures:\n{}",
        language,
        report.failures.join("\n\n")
    );
    assert!(
        report.passed > 0,
        "{language} selection executed no unique cases"
    );
}

#[test]
#[ignore = "requires materialized, oracle-verified external Stan sources"]
fn external_stan_examples_have_no_false_positive_diagnostics() {
    run_external("stan", "stan-oracle.json");
}

#[test]
#[ignore = "requires materialized, oracle-verified external JAGS sources"]
fn external_jags_examples_have_no_false_positive_diagnostics() {
    run_external("jags", "jags-oracle.json");
}
