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
from pathlib import Path
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
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    if args.probe_timeout_seconds <= 0:
        parser.error("--probe-timeout-seconds must be positive")
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    corpus = json.loads(args.quality_corpus.read_text(encoding="utf-8"))
    provenance = json.loads(args.provenance.read_text(encoding="utf-8"))
    all_cases = matrix_cases(manifest) + quality_cases(corpus)
    selected_cases = all_cases if (
        args.refresh_results or args.verify_results or args.verify_results_live
    ) else matrix_cases(manifest)
    binding = input_binding(args.manifest, args.quality_corpus, all_cases)

    if args.verify_results or args.verify_results_live:
        committed = json.loads(args.results.read_text(encoding="utf-8"))
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
