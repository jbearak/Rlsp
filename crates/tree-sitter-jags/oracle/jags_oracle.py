#!/usr/bin/env python3
"""Run clean-room syntax probes against the JAGS command-line parser.

The harness uses only the public command-line interface. It deliberately does
not read or derive rules from JAGS source code or documentation. A probe is
syntax-accepted when `model in` completes without JAGS's parse-error heading;
`compile` is optional and is used only to prove that semantic failures happen
after an accepted parse.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import signal
import subprocess
import sys
import tempfile
from typing import Any


EXPECTED_VERSION = "4.3.2"
PARSE_ERROR_HEADING = "Error parsing model file:"
SEMANTIC_ERROR_MARKERS = ("RUNTIME ERROR:", "Compilation error on line")
DEFAULT_TIMEOUT_SECONDS = 5.0
ORACLE_DIR = Path(__file__).parent
DEFAULT_MATRIX = ORACLE_DIR / "syntax-matrix.json"
DEFAULT_CORPUS = ORACLE_DIR / "quality-corpus.json"
DEFAULT_RESULTS = ORACLE_DIR / "oracle-results.json"
DEFAULT_PROVENANCE = ORACLE_DIR / "provenance.json"
REPO_ROOT = ORACLE_DIR.parents[2]
DEFAULT_EXTERNAL_MANIFEST = (
    REPO_ROOT / "crates/raven/tests/fixtures/diagnostic_corpora/jags.json"
)
DEFAULT_EXTERNAL_ROOT = REPO_ROOT / "target/diagnostic-corpora"
DEFAULT_EXTERNAL_RESULTS = ORACLE_DIR / "external-model-results.json"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_bytes(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def allow_unpinned_from_environment() -> bool:
    return os.environ.get("JAGS_ORACLE_ALLOW_UNPINNED") == "1"


def validate_oracle_hashes(
    jags: Path,
    terminal: Path,
    provenance: dict[str, Any],
    allow_unpinned: bool,
) -> dict[str, str]:
    if not jags.is_file():
        raise ValueError(f"JAGS wrapper not found: {jags}")
    if not terminal.is_file():
        raise ValueError(f"JAGS terminal not found: {terminal}")
    actual = {
        "wrapper_path": str(jags),
        "wrapper_sha256": sha256(jags),
        "terminal_path": str(terminal),
        "terminal_sha256": sha256(terminal),
    }
    installation = provenance["homebrew_installation"]
    expected = {
        "wrapper_sha256": installation["wrapper_sha256"],
        "terminal_sha256": installation["terminal_sha256"],
    }
    mismatches = [
        f"{key}: expected {expected[key]}, got {actual[key]}"
        for key in expected
        if actual[key] != expected[key]
    ]
    if mismatches and not allow_unpinned:
        raise ValueError(
            "oracle executable hash mismatch; use --allow-unpinned-oracle only "
            "for an independently validated platform build:\n" + "\n".join(mismatches)
        )
    actual["binding"] = "override" if mismatches else "pinned-homebrew-arm64"
    return actual


def run_probe(
    jags: Path,
    source: bytes,
    compile_model: bool,
    timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
) -> dict[str, object]:
    with tempfile.TemporaryDirectory(prefix="raven-jags-oracle-") as temp_dir:
        model_path = Path(temp_dir) / "probe.jags"
        model_path.write_bytes(source)
        commands = f'model in "{model_path}"\n'
        if compile_model:
            commands += "compile\n"
        commands += "exit\n"
        process = subprocess.Popen(
            [str(jags)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={**os.environ, "LC_ALL": "C"},
            start_new_session=os.name == "posix",
        )
        try:
            stdout, stderr = process.communicate(
                input=commands.encode("utf-8"),
                timeout=timeout_seconds,
            )
        except subprocess.TimeoutExpired as error:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
            stdout, stderr = process.communicate()
            output = (error.stdout or b"") + (error.stderr or b"") + stdout + stderr
            return {
                "returncode": None,
                "version": None,
                "syntax_accepted": False,
                "semantic_error": False,
                "timed_out": True,
                "output": output.decode("utf-8", errors="replace"),
            }

    output = (stdout + stderr).decode("utf-8", errors="replace")
    version_match = re.search(r"Welcome to JAGS ([0-9.]+)", output)
    version = version_match.group(1) if version_match else None
    syntax_accepted = PARSE_ERROR_HEADING not in output
    semantic_error = any(marker in output for marker in SEMANTIC_ERROR_MARKERS)
    return {
        "returncode": process.returncode,
        "version": version,
        "syntax_accepted": syntax_accepted,
        "semantic_error": semantic_error,
        "timed_out": False,
        "output": output,
    }


def decode_source(probe: dict[str, object]) -> bytes:
    source = str(probe["source"])
    encoding = str(probe.get("encoding", "utf-8"))
    if encoding == "utf-8-bom":
        return b"\xef\xbb\xbf" + source.encode("utf-8")
    if encoding != "utf-8":
        raise ValueError(f"unsupported probe encoding: {encoding}")
    return source.encode("utf-8")


def matrix_cases(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for probe in manifest["probes"]:
        cases.append({
            "id": f'matrix-{probe["name"]}',
            "group": "syntax-matrix",
            "family": probe["category"],
            "template": probe["name"],
            "source_bytes": decode_source(probe),
            "compile": bool(probe.get("compile", False)),
            "expect_parse": probe["expect_parse"],
            "expect_semantic_error": probe.get("expect_semantic_error"),
        })
    return cases


def quality_cases(corpus: dict[str, Any]) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for case in corpus["cases"]:
        cases.append({
            "id": case["id"],
            "group": case["group"],
            "family": case["family"],
            "template": case["template"],
            "source_bytes": str(case["source"]).encode("utf-8"),
            "compile": bool(case.get("compile", False)),
            "expect_parse": case["expect_parse"],
            "expect_semantic_error": case.get("expect_semantic_error"),
        })
    return cases


def source_record(case: dict[str, Any]) -> dict[str, Any]:
    record: dict[str, Any] = {
        "id": case["id"],
        "group": case["group"],
        "family": case["family"],
        "template": case["template"],
        "source_sha256": sha256_bytes(case["source_bytes"]),
        "compile": case["compile"],
        "expect_parse": case["expect_parse"],
    }
    if case["expect_semantic_error"] is not None:
        record["expect_semantic_error"] = case["expect_semantic_error"]
    return record


def source_set_sha256(cases: list[dict[str, Any]]) -> str:
    canonical = json.dumps(
        [source_record(case) for case in cases],
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return sha256_bytes(canonical)


def result_counts(cases: list[dict[str, Any]]) -> dict[str, Any]:
    counts: dict[str, Any] = {}
    for group in sorted({str(case["group"]) for case in cases}):
        members = [case for case in cases if case["group"] == group]
        counts[group] = {
            "total": len(members),
            "families": len({str(case["family"]) for case in members}),
            "authored_templates": len({str(case["template"]) for case in members}),
            "expected_accepted": sum(case["expect_parse"] == "accepted" for case in members),
            "expected_rejected": sum(case["expect_parse"] == "rejected" for case in members),
        }
    return counts


def input_binding(
    matrix_path: Path,
    corpus_path: Path,
    cases: list[dict[str, Any]],
) -> dict[str, str]:
    return {
        "syntax_matrix_sha256": sha256(matrix_path),
        "quality_corpus_sha256": sha256(corpus_path),
        "corpus_generator_sha256": sha256(ORACLE_DIR / "generate_quality_corpus.py"),
        "oracle_harness_sha256": sha256(Path(__file__)),
        "source_set_sha256": source_set_sha256(cases),
    }


def run_cases(
    cases: list[dict[str, Any]],
    jags: Path,
    expected_version: str,
    timeout_seconds: float,
) -> tuple[list[dict[str, Any]], list[str]]:
    records: list[dict[str, Any]] = []
    failures: list[str] = []
    for case in cases:
        result = run_probe(
            jags,
            case["source_bytes"],
            case["compile"],
            timeout_seconds,
        )
        record = {
            **source_record(case),
            "returncode": result["returncode"],
            "version": result["version"],
            "syntax_accepted": result["syntax_accepted"],
            "semantic_error": result["semantic_error"],
            "timed_out": result["timed_out"],
        }
        records.append(record)
        if result["timed_out"]:
            failures.append(f'{case["id"]}: exceeded {timeout_seconds:.3f}s timeout')
            continue
        if result["version"] != expected_version:
            failures.append(
                f'{case["id"]}: expected JAGS {expected_version}, got {result["version"]}'
            )
        expected_acceptance = case["expect_parse"] == "accepted"
        if result["syntax_accepted"] != expected_acceptance:
            failures.append(
                f'{case["id"]}: expected {case["expect_parse"]}, '
                f'got {"accepted" if result["syntax_accepted"] else "rejected"}'
            )
        expected_semantic = case["expect_semantic_error"]
        if expected_semantic is not None and result["semantic_error"] != expected_semantic:
            failures.append(
                f'{case["id"]}: expected semantic_error={expected_semantic}, '
                f'got {result["semantic_error"]}'
            )
    return records, failures


def verify_committed_results(
    report: dict[str, Any],
    provenance: dict[str, Any],
    binding: dict[str, str],
    cases: list[dict[str, Any]],
) -> list[str]:
    failures: list[str] = []
    if report.get("schema_version") != 1:
        failures.append("oracle results schema_version must be 1")
    if report.get("inputs") != binding:
        failures.append("oracle result input hashes drifted; refresh with --refresh-results")
    expected_oracle = {
        "version": provenance["target"]["version"],
        "wrapper_sha256": provenance["homebrew_installation"]["wrapper_sha256"],
        "terminal_sha256": provenance["homebrew_installation"]["terminal_sha256"],
        "binding": "pinned-homebrew-arm64",
    }
    observed_oracle = report.get("oracle", {})
    for key, expected in expected_oracle.items():
        if observed_oracle.get(key) != expected:
            failures.append(
                f"oracle results {key}: expected {expected}, got {observed_oracle.get(key)}"
            )
    if report.get("counts") != result_counts(cases):
        failures.append("oracle result counts drifted")
    expected_sources = [source_record(case) for case in cases]
    records = report.get("results")
    if not isinstance(records, list) or len(records) != len(expected_sources):
        failures.append(
            f"oracle result records: expected {len(expected_sources)}, "
            f"got {len(records) if isinstance(records, list) else 'non-list'}"
        )
        return failures
    for expected, observed in zip(expected_sources, records, strict=True):
        for key, value in expected.items():
            if observed.get(key) != value:
                failures.append(
                    f'{expected["id"]}: committed {key} drifted '
                    f'(expected {value!r}, got {observed.get(key)!r})'
                )
        expected_acceptance = expected["expect_parse"] == "accepted"
        if observed.get("syntax_accepted") != expected_acceptance:
            failures.append(f'{expected["id"]}: committed syntax outcome contradicts expectation')
        expected_semantic = expected.get("expect_semantic_error")
        if expected_semantic is not None and observed.get("semantic_error") != expected_semantic:
            failures.append(f'{expected["id"]}: committed semantic outcome contradicts expectation')
        if observed.get("version") != provenance["target"]["version"]:
            failures.append(f'{expected["id"]}: committed version drifted')
        if observed.get("timed_out") is not False:
            failures.append(f'{expected["id"]}: committed probe timed out')
    if report.get("failures") != []:
        failures.append("committed oracle results contain failures")
    return failures


def load_json_object(path: Path, description: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read {description} {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"{description} {path} must contain a JSON object")
    return value


def require_external_string(record: dict[str, Any], key: str, context: str) -> str:
    value = record.get(key)
    if not isinstance(value, str) or not value:
        raise ValueError(f"{context}.{key} must be a non-empty string")
    return value


def materialized_file(root: Path, relative: str, context: str) -> Path:
    if "\\" in relative:
        raise ValueError(f"{context}.materialized_path must use POSIX separators")
    raw_parts = relative.split("/")
    pure = PurePosixPath(relative)
    if (
        pure.is_absolute()
        or not raw_parts
        or any(part in {"", ".", ".."} for part in raw_parts)
    ):
        raise ValueError(f"{context}.materialized_path must be a safe relative path")
    candidate = root.joinpath(*pure.parts)
    current = root
    try:
        for part in pure.parts:
            current = current / part
            if current.is_symlink():
                raise ValueError(
                    f"{context}.materialized_path traverses a symbolic link"
                )
        if not candidate.is_file():
            raise ValueError(
                f"{context}.materialized_path is not a regular file: {candidate}"
            )
        resolved_root = root.resolve(strict=True)
        resolved_candidate = candidate.resolve(strict=True)
    except OSError as error:
        raise ValueError(
            f"cannot inspect {context}.materialized_path {candidate}: {error}"
        ) from error
    if not resolved_candidate.is_relative_to(resolved_root):
        raise ValueError(f"{context}.materialized_path escapes the materialized root")
    return candidate


def external_cases(
    manifest_path: Path,
    external_root: Path,
) -> tuple[list[dict[str, Any]], dict[str, str]]:
    manifest = load_json_object(manifest_path, "external manifest")
    if manifest.get("schema_version") != 1 or manifest.get("language") != "jags":
        raise ValueError(
            "external JAGS manifest must have schema_version 1 and language jags"
        )
    sources = manifest.get("sources")
    if not isinstance(sources, list) or not sources:
        raise ValueError("external JAGS manifest sources must be a non-empty array")

    source_rules: dict[str, set[tuple[str, str, str]]] = {}
    for source_index, source_value in enumerate(sources):
        context = f"manifest.sources[{source_index}]"
        if not isinstance(source_value, dict):
            raise ValueError(f"{context} must be an object")
        source_id = require_external_string(source_value, "id", context)
        if source_id in source_rules:
            raise ValueError(f"duplicate external source id {source_id}")
        discoveries = source_value.get("discovery")
        if not isinstance(discoveries, list) or not discoveries:
            raise ValueError(f"{context}.discovery must be a non-empty array")
        rules: set[tuple[str, str, str]] = set()
        for discovery_index, discovery_value in enumerate(discoveries):
            discovery_context = f"{context}.discovery[{discovery_index}]"
            if not isinstance(discovery_value, dict):
                raise ValueError(f"{discovery_context} must be an object")
            oracle_mode = require_external_string(
                discovery_value, "oracle_mode", discovery_context
            )
            if oracle_mode != "jags-model-in":
                raise ValueError(
                    f"{discovery_context}.oracle_mode is unsupported: {oracle_mode}"
                )
            rules.add((
                require_external_string(discovery_value, "kind", discovery_context),
                require_external_string(
                    discovery_value, "raven_mode", discovery_context
                ),
                oracle_mode,
            ))
        source_rules[source_id] = rules

    materialized_root = external_root / "materialized"
    index_path = materialized_root / "index.json"
    index = load_json_object(index_path, "materialized index")
    index_cases = index.get("cases")
    if index.get("schema_version") != 1 or not isinstance(index_cases, list):
        raise ValueError(
            "materialized index must have schema_version 1 and a cases array"
        )
    manifest_digest = sha256(manifest_path)
    binding = index.get("manifest_binding")
    if not isinstance(binding, list) or not any(
        isinstance(item, dict) and item.get("sha256") == manifest_digest
        for item in binding
    ):
        raise ValueError(
            "materialized index is not bound to the supplied external manifest"
        )

    cases: list[dict[str, Any]] = []
    seen_ids: set[str] = set()
    seen_paths: set[str] = set()
    for case_index, case_value in enumerate(index_cases):
        if not isinstance(case_value, dict) or case_value.get("language") != "jags":
            continue
        context = f"index.cases[{case_index}]"
        case_id = require_external_string(case_value, "id", context)
        relative_path = require_external_string(
            case_value, "materialized_path", context
        )
        if case_id in seen_ids:
            raise ValueError(f"duplicate materialized JAGS case id {case_id}")
        if relative_path in seen_paths:
            raise ValueError(f"duplicate materialized JAGS path {relative_path}")
        seen_ids.add(case_id)
        seen_paths.add(relative_path)
        source_id = require_external_string(case_value, "source_id", context)
        rule = (
            require_external_string(case_value, "kind", context),
            require_external_string(case_value, "raven_mode", context),
            require_external_string(case_value, "oracle_mode", context),
        )
        if rule not in source_rules.get(source_id, set()):
            raise ValueError(
                f"{context} does not match a discovery rule for {source_id}"
            )
        source_path = materialized_file(materialized_root, relative_path, context)
        source_bytes = source_path.read_bytes()
        expected_hash = require_external_string(case_value, "sha256", context)
        actual_hash = sha256_bytes(source_bytes)
        if actual_hash != expected_hash:
            raise ValueError(
                f"{case_id}: materialized SHA-256 mismatch "
                f"(expected {expected_hash}, got {actual_hash})"
            )
        cases.append({
            "id": case_id,
            "group": "external-diagnostic-corpus",
            "family": source_id,
            "template": relative_path,
            "source_bytes": source_bytes,
            "compile": False,
            "expect_parse": "accepted",
            "expect_semantic_error": None,
            "source_id": source_id,
            "materialized_path": relative_path,
            "sha256": expected_hash,
            "kind": rule[0],
            "raven_mode": rule[1],
            "oracle_mode": rule[2],
        })
    if not cases:
        raise ValueError("materialized index contains no JAGS cases")
    counts = index.get("counts")
    if (
        isinstance(counts, dict)
        and isinstance(counts.get("jags"), int)
        and counts["jags"] != len(cases)
    ):
        raise ValueError(
            f"materialized JAGS count drifted: "
            f"index={counts['jags']}, observed={len(cases)}"
        )
    source_set = [{
        key: case[key]
        for key in (
            "id",
            "source_id",
            "materialized_path",
            "sha256",
            "kind",
            "raven_mode",
            "oracle_mode",
        )
    } for case in cases]
    source_set_digest = sha256_bytes(json.dumps(
        source_set,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8"))
    return cases, {
        "manifest_sha256": manifest_digest,
        "materialized_index_sha256": sha256(index_path),
        "source_set_sha256": source_set_digest,
    }


def run_external_cases(
    cases: list[dict[str, Any]],
    jags: Path,
    expected_version: str,
    timeout_seconds: float,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[str]]:
    records: list[dict[str, Any]] = []
    verified: list[dict[str, Any]] = []
    failures: list[str] = []
    for case in cases:
        result = run_probe(jags, case["source_bytes"], False, timeout_seconds)
        accepted = bool(result["syntax_accepted"]) and not bool(result["timed_out"])
        record = {
            "id": case["id"],
            "source_id": case["source_id"],
            "materialized_path": case["materialized_path"],
            "sha256": case["sha256"],
            "kind": case["kind"],
            "raven_mode": case["raven_mode"],
            "oracle_mode": case["oracle_mode"],
            "outcome": "accepted-direct" if accepted else "rejected",
            "wrapper_id": "model-in" if accepted else None,
            "syntax_accepted": accepted,
            "version": result["version"],
            "timed_out": result["timed_out"],
        }
        records.append(record)
        if result["timed_out"]:
            failures.append(
                f'{case["id"]}: exceeded {timeout_seconds:.3f}s timeout'
            )
            continue
        if result["version"] != expected_version:
            failures.append(
                f'{case["id"]}: expected JAGS {expected_version}, '
                f'got {result["version"]}'
            )
        if accepted:
            verified.append({
                "id": case["id"],
                "materialized_path": f'materialized/{case["materialized_path"]}',
                "sha256": case["sha256"],
                "raven_mode": "syntax-only",
                "wrapper_id": "model-in",
                "source": {
                    "materialized_path": f'materialized/{case["materialized_path"]}',
                    "sha256": case["sha256"],
                },
            })
    return records, verified, failures


def external_result_record(
    case: dict[str, Any], syntax_accepted: bool
) -> dict[str, Any]:
    return {
        "id": case["id"],
        "materialized_path": case["materialized_path"],
        "sha256": case["sha256"],
        "syntax_accepted": syntax_accepted,
    }


def external_outcomes_sha256(records: list[dict[str, Any]]) -> str:
    return sha256_bytes(json.dumps(
        records,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8"))


def verify_external_results(
    committed: dict[str, Any],
    provenance: dict[str, Any],
    inputs: dict[str, str],
    cases: list[dict[str, Any]],
) -> list[str]:
    failures: list[str] = []
    if committed.get("schema_version") != 1:
        failures.append("external results schema_version must be 1")
    expected_inputs = {
        "manifest_sha256": inputs["manifest_sha256"],
        "source_set_sha256": inputs["source_set_sha256"],
    }
    if committed.get("inputs") != expected_inputs:
        failures.append("external result input hashes drifted; refresh external results")
    expected_oracle = {
        "version": provenance["target"]["version"],
        "wrapper_sha256": provenance["homebrew_installation"]["wrapper_sha256"],
        "terminal_sha256": provenance["homebrew_installation"]["terminal_sha256"],
        "binding": "pinned-homebrew-arm64",
    }
    observed_oracle = committed.get("oracle")
    if not isinstance(observed_oracle, dict):
        failures.append("external results oracle binding is missing")
    else:
        for key, expected in expected_oracle.items():
            if observed_oracle.get(key) != expected:
                failures.append(
                    f"external results {key}: expected {expected}, "
                    f"got {observed_oracle.get(key)}"
                )
    records = committed.get("results")
    if not isinstance(records, list) or len(records) != len(cases):
        failures.append(
            f"external result records: expected {len(cases)}, "
            f"got {len(records) if isinstance(records, list) else 'non-list'}"
        )
        return failures
    for case, observed in zip(cases, records, strict=True):
        if not isinstance(observed, dict):
            failures.append(f'{case["id"]}: external result must be an object')
            continue
        expected = external_result_record(
            case, bool(observed.get("syntax_accepted"))
        )
        if not isinstance(observed.get("syntax_accepted"), bool):
            failures.append(
                f'{case["id"]}: committed syntax_accepted must be boolean'
            )
        for key, value in expected.items():
            if observed.get(key) != value:
                failures.append(
                    f'{case["id"]}: committed {key} drifted '
                    f'(expected {value!r}, got {observed.get(key)!r})'
                )
    holdout = provenance.get("official_model_holdout")
    if not isinstance(holdout, dict):
        failures.append("provenance official_model_holdout binding is missing")
    else:
        accepted_count = sum(
            isinstance(record, dict) and record.get("syntax_accepted") is True
            for record in records
        )
        if accepted_count != holdout.get("accepted_count"):
            failures.append(
                "external accepted count drifted "
                f"(expected {holdout.get('accepted_count')}, got {accepted_count})"
            )
        actual_digest = external_outcomes_sha256(records)
        if actual_digest != holdout.get("outcomes_sha256"):
            failures.append(
                "external canonical outcome digest drifted "
                f"(expected {holdout.get('outcomes_sha256')}, got {actual_digest})"
            )
    if committed.get("failures") != []:
        failures.append("committed external results contain failures")
    return failures


def external_report_records(
    cases: list[dict[str, Any]], committed: dict[str, Any]
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    results: list[dict[str, Any]] = []
    verified: list[dict[str, Any]] = []
    committed_records = committed["results"]
    for case, outcome in zip(cases, committed_records, strict=True):
        accepted = bool(outcome["syntax_accepted"])
        results.append({
            "id": case["id"],
            "source_id": case["source_id"],
            "materialized_path": case["materialized_path"],
            "sha256": case["sha256"],
            "kind": case["kind"],
            "raven_mode": case["raven_mode"],
            "oracle_mode": case["oracle_mode"],
            "outcome": "accepted-direct" if accepted else "rejected",
            "wrapper_id": "model-in" if accepted else None,
            "syntax_accepted": accepted,
            "version": committed["oracle"]["version"],
            "timed_out": False,
        })
        if accepted:
            verified.append({
                "id": case["id"],
                "materialized_path": f'materialized/{case["materialized_path"]}',
                "sha256": case["sha256"],
                "raven_mode": "syntax-only",
                "wrapper_id": "model-in",
                "source": {
                    "materialized_path": f'materialized/{case["materialized_path"]}',
                    "sha256": case["sha256"],
                },
            })
    return results, verified


def write_external_report(
    external_root: Path,
    oracle: dict[str, str],
    inputs: dict[str, str],
    results: list[dict[str, Any]],
    verified_cases: list[dict[str, Any]],
    failures: list[str],
) -> Path:
    report = {
        "schema_version": 1,
        "oracle": {"version": EXPECTED_VERSION, **oracle},
        "inputs": inputs,
        "counts": {
            "total": len(results),
            "accepted_direct": len(verified_cases),
            "rejected": len(results) - len(verified_cases),
            "verified": len(verified_cases),
        },
        "verified_cases": verified_cases,
        "outcomes": results,
        "failures": failures,
    }
    report_path = external_root / "materialized/jags-oracle.json"
    temporary = report_path.with_name(f"{report_path.name}.tmp-{os.getpid()}")
    temporary.write_text(
        json.dumps(report, indent=2, ensure_ascii=False, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, report_path)
    return report_path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--manifest",
        type=Path,
        default=DEFAULT_MATRIX,
    )
    parser.add_argument("--quality-corpus", type=Path, default=DEFAULT_CORPUS)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    parser.add_argument("--provenance", type=Path, default=DEFAULT_PROVENANCE)
    parser.add_argument("--jags", type=Path, default=Path("/opt/homebrew/bin/jags"))
    parser.add_argument(
        "--terminal",
        type=Path,
        default=Path("/opt/homebrew/Cellar/jags/4.3.2/libexec/jags-terminal"),
    )
    parser.add_argument(
        "--probe-timeout-seconds",
        type=float,
        default=DEFAULT_TIMEOUT_SECONDS,
    )
    parser.add_argument("--allow-unpinned-oracle", action="store_true")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--refresh-results", action="store_true")
    mode.add_argument("--verify-results", action="store_true")
    mode.add_argument("--verify-results-live", action="store_true")
    mode.add_argument("--verify-external", action="store_true")
    mode.add_argument("--verify-external-live", action="store_true")
    mode.add_argument("--refresh-external-results", action="store_true")
    parser.add_argument(
        "--external-manifest", type=Path, default=DEFAULT_EXTERNAL_MANIFEST
    )
    parser.add_argument("--external-root", type=Path, default=DEFAULT_EXTERNAL_ROOT)
    parser.add_argument(
        "--external-results", type=Path, default=DEFAULT_EXTERNAL_RESULTS
    )
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    if args.probe_timeout_seconds <= 0:
        parser.error("--probe-timeout-seconds must be positive")
    provenance = json.loads(args.provenance.read_text(encoding="utf-8"))

    external_mode = (
        args.verify_external
        or args.verify_external_live
        or args.refresh_external_results
    )
    if external_mode:
        try:
            selected_cases, external_inputs = external_cases(
                args.external_manifest.resolve(), args.external_root.resolve()
            )
        except ValueError as error:
            print(f"ERROR: {error}", file=sys.stderr)
            return 2

        external_committed: dict[str, Any] | None = None
        if args.verify_external or args.verify_external_live:
            try:
                external_committed = load_json_object(
                    args.external_results, "committed external results"
                )
            except ValueError as error:
                print(f"ERROR: {error}", file=sys.stderr)
                return 2
            offline_failures = verify_external_results(
                external_committed, provenance, external_inputs, selected_cases
            )
            if offline_failures:
                for failure in offline_failures:
                    print(f"ERROR: {failure}", file=sys.stderr)
                return 1
            if args.verify_external:
                results, verified_cases = external_report_records(
                    selected_cases, external_committed
                )
                report_path = write_external_report(
                    args.external_root.resolve(),
                    external_committed["oracle"],
                    external_inputs,
                    results,
                    verified_cases,
                    [],
                )
                if args.json:
                    print(report_path.read_text(encoding="utf-8"), end="")
                else:
                    print(
                        f"verified {len(results)} committed external JAGS outcomes "
                        f"and wrote {report_path}"
                    )
                return 0

        try:
            oracle = validate_oracle_hashes(
                args.jags,
                args.terminal,
                provenance,
                args.allow_unpinned_oracle or allow_unpinned_from_environment(),
            )
        except ValueError as error:
            print(f"ERROR: {error}", file=sys.stderr)
            return 2
        expected_version = provenance["target"]["version"]
        results, verified_cases, failures = run_external_cases(
            selected_cases,
            args.jags,
            expected_version,
            args.probe_timeout_seconds,
        )

        if args.verify_external_live and external_committed is not None:
            for case, fresh, recorded in zip(
                selected_cases, results, external_committed["results"], strict=True
            ):
                expected = external_result_record(
                    case, bool(fresh["syntax_accepted"])
                )
                if expected != recorded:
                    failures.append(
                        f'{case["id"]}: live external outcome differs from committed'
                    )

        if args.refresh_external_results:
            if oracle["binding"] != "pinned-homebrew-arm64":
                print(
                    "ERROR: refusing to commit external results from an unpinned oracle",
                    file=sys.stderr,
                )
                return 2
            if failures:
                for failure in failures:
                    print(f"ERROR: {failure}", file=sys.stderr)
                return 1
            committed_report = {
                "schema_version": 1,
                "inputs": {
                    "manifest_sha256": external_inputs["manifest_sha256"],
                    "source_set_sha256": external_inputs["source_set_sha256"],
                },
                "oracle": {"version": expected_version, **oracle},
                "results": [
                    external_result_record(
                        case, bool(result["syntax_accepted"])
                    )
                    for case, result in zip(selected_cases, results, strict=True)
                ],
                "failures": [],
            }
            args.external_results.write_text(
                json.dumps(
                    committed_report,
                    indent=2,
                    ensure_ascii=False,
                    sort_keys=True,
                ) + "\n",
                encoding="utf-8",
            )
            print(
                f"wrote {len(selected_cases)} pinned external outcomes "
                f"to {args.external_results}"
            )
            return 0

        report_path = write_external_report(
            args.external_root.resolve(),
            oracle,
            external_inputs,
            results,
            verified_cases,
            failures,
        )
        if args.json:
            print(report_path.read_text(encoding="utf-8"), end="")
        else:
            for item in results:
                status = "PASS" if item["syntax_accepted"] else "SKIP"
                print(
                    f'{status:4} external-diagnostic-corpus '
                    f'{item["id"]:52} '
                    f'{"accepted" if item["syntax_accepted"] else "rejected"}'
                )
            print(
                f"\n{len(results)} external probes: "
                f"{len(verified_cases)} accepted, "
                f"{len(results) - len(verified_cases)} rejected/accounted"
            )
            for failure in failures:
                print(f"ERROR: {failure}", file=sys.stderr)
        return 1 if failures else 0

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    corpus = json.loads(args.quality_corpus.read_text(encoding="utf-8"))
    all_cases = matrix_cases(manifest) + quality_cases(corpus)
    selected_cases = all_cases if (
        args.refresh_results or args.verify_results or args.verify_results_live
    ) else matrix_cases(manifest)
    binding = input_binding(args.manifest, args.quality_corpus, all_cases)

    committed: dict[str, Any] | None = None
    if args.verify_results or args.verify_results_live:
        committed = json.loads(args.results.read_text(encoding="utf-8"))
        assert committed is not None
        offline_failures = verify_committed_results(
            committed,
            provenance,
            binding,
            all_cases,
        )
        if offline_failures:
            for failure in offline_failures:
                print(f"ERROR: {failure}", file=sys.stderr)
            return 1
        if args.verify_results:
            print(
                f"verified {len(all_cases)} committed oracle outcomes and input hashes"
            )
            return 0

    try:
        oracle = validate_oracle_hashes(
            args.jags,
            args.terminal,
            provenance,
            args.allow_unpinned_oracle or allow_unpinned_from_environment(),
        )
    except ValueError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2

    expected_version = provenance["target"]["version"]
    results, failures = run_cases(
        selected_cases,
        args.jags,
        expected_version,
        args.probe_timeout_seconds,
    )
    report = {
        "schema_version": 1,
        "oracle": {"version": expected_version, **oracle},
        "inputs": binding,
        "counts": result_counts(selected_cases),
        "results": results,
        "failures": failures,
    }

    if args.refresh_results:
        if oracle["binding"] != "pinned-homebrew-arm64":
            print("ERROR: refusing to commit results from an unpinned oracle", file=sys.stderr)
            return 2
        if failures:
            for failure in failures:
                print(f"ERROR: {failure}", file=sys.stderr)
            return 1
        args.results.write_text(
            json.dumps(report, indent=2, ensure_ascii=False, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(f"wrote {len(results)} pinned outcomes to {args.results}")
        return 0

    if args.verify_results_live:
        assert committed is not None
        committed_records = committed["results"]
        for fresh, recorded in zip(results, committed_records, strict=True):
            for key in (
                "id",
                "source_sha256",
                "version",
                "syntax_accepted",
                "semantic_error",
                "timed_out",
            ):
                if fresh[key] != recorded[key]:
                    failures.append(
                        f'{fresh["id"]}: live {key}={fresh[key]!r} '
                        f'!= committed {recorded[key]!r}'
                    )

    if args.json:
        print(json.dumps(report, indent=2, ensure_ascii=False, sort_keys=True))
    else:
        for item in results:
            status = "PASS" if (
                (item["expect_parse"] == "accepted") == item["syntax_accepted"]
                and item["version"] == expected_version
                and not item["timed_out"]
            ) else "FAIL"
            print(
                f'{status:4} {item["group"]:18} {item["id"]:52} '
                f'{"accepted" if item["syntax_accepted"] else "rejected"}'
            )
        print(
            f"\n{len(results)} probes: "
            f'{sum(bool(item["syntax_accepted"]) for item in results)} accepted, '
            f'{sum(not bool(item["syntax_accepted"]) for item in results)} rejected'
        )
        for failure in failures:
            print(f"ERROR: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
