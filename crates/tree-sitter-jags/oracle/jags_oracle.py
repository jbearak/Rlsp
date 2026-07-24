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
import subprocess
import sys
import tempfile


EXPECTED_VERSION = "4.3.2"
PARSE_ERROR_HEADING = "Error parsing model file:"
SEMANTIC_ERROR_MARKERS = ("RUNTIME ERROR:", "Compilation error on line")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run_probe(jags: Path, source: bytes, compile_model: bool) -> dict[str, object]:
    with tempfile.TemporaryDirectory(prefix="raven-jags-oracle-") as temp_dir:
        model_path = Path(temp_dir) / "probe.jags"
        model_path.write_bytes(source)
        commands = f'model in "{model_path}"\n'
        if compile_model:
            commands += "compile\n"
        commands += "exit\n"
        completed = subprocess.run(
            [str(jags)],
            input=commands.encode("utf-8"),
            capture_output=True,
            timeout=10,
            check=False,
            env={**os.environ, "LC_ALL": "C"},
        )

    output = (completed.stdout + completed.stderr).decode("utf-8", errors="replace")
    version_match = re.search(r"Welcome to JAGS ([0-9.]+)", output)
    version = version_match.group(1) if version_match else None
    syntax_accepted = PARSE_ERROR_HEADING not in output
    semantic_error = any(marker in output for marker in SEMANTIC_ERROR_MARKERS)
    return {
        "returncode": completed.returncode,
        "version": version,
        "syntax_accepted": syntax_accepted,
        "semantic_error": semantic_error,
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


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--manifest",
        type=Path,
        default=Path(__file__).with_name("syntax-matrix.json"),
    )
    parser.add_argument("--jags", type=Path, default=Path("/opt/homebrew/bin/jags"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    expected_version = manifest["oracle"]["version"]
    results: list[dict[str, object]] = []
    failures: list[str] = []

    if not args.jags.exists():
        print(f"JAGS executable not found: {args.jags}", file=sys.stderr)
        return 2

    for probe in manifest["probes"]:
        result = run_probe(
            args.jags,
            decode_source(probe),
            bool(probe.get("compile", False)),
        )
        record = {
            "name": probe["name"],
            "category": probe["category"],
            "expected": probe["expect_parse"],
            **{key: value for key, value in result.items() if key != "output"},
        }
        results.append(record)

        if result["version"] != expected_version:
            failures.append(
                f'{probe["name"]}: expected JAGS {expected_version}, got {result["version"]}'
            )
        expected_acceptance = probe["expect_parse"] == "accepted"
        if result["syntax_accepted"] != expected_acceptance:
            failures.append(
                f'{probe["name"]}: expected {probe["expect_parse"]}, '
                f'got {"accepted" if result["syntax_accepted"] else "rejected"}'
            )
        expect_semantic_error = probe.get("expect_semantic_error")
        if expect_semantic_error is not None and result["semantic_error"] != expect_semantic_error:
            failures.append(
                f'{probe["name"]}: expected semantic_error={expect_semantic_error}, '
                f'got {result["semantic_error"]}'
            )

    report = {
        "oracle": {
            "path": str(args.jags),
            "version": expected_version,
            "sha256": sha256(args.jags.resolve()),
        },
        "counts": {
            "total": len(results),
            "accepted": sum(bool(item["syntax_accepted"]) for item in results),
            "rejected": sum(not bool(item["syntax_accepted"]) for item in results),
        },
        "results": results,
        "failures": failures,
    }
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        for item in results:
            status = "PASS" if (
                (item["expected"] == "accepted") == item["syntax_accepted"]
                and item["version"] == expected_version
            ) else "FAIL"
            print(
                f'{status:4} {item["category"]:24} {item["name"]:36} '
                f'{"accepted" if item["syntax_accepted"] else "rejected"}'
            )
        print(
            f'\n{len(results)} probes: {report["counts"]["accepted"]} accepted, '
            f'{report["counts"]["rejected"]} rejected'
        )
        for failure in failures:
            print(f"ERROR: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
