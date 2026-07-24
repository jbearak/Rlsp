#!/usr/bin/env python3
"""Verify that committed JAGS parser evidence is bound to current inputs."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
import sys
from typing import Any


FUZZ_DIR = Path(__file__).parent
CRATE_DIR = FUZZ_DIR.parent
EVIDENCE_PATH = FUZZ_DIR / "evidence.json"
SHA256_PATTERN = re.compile(r"[0-9a-f]{64}")


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def content_multiset(directory: Path) -> tuple[int, int, str]:
    files = sorted(path for path in directory.rglob("*") if path.is_file())
    content_hashes = sorted(sha256(path) for path in files)
    digest_input = "".join(f"{item}\n" for item in content_hashes).encode("ascii")
    return (
        len(files),
        sum(path.stat().st_size for path in files),
        hashlib.sha256(digest_input).hexdigest(),
    )


def require(condition: bool, message: str, failures: list[str]) -> None:
    if not condition:
        failures.append(message)


def verify_campaign(name: str, campaign: dict[str, Any], failures: list[str]) -> None:
    source_paths = {
        "grammar_js_sha256": CRATE_DIR / "grammar.js",
        "parser_c_sha256": CRATE_DIR / "src" / "parser.c",
        "fuzz_target_sha256": FUZZ_DIR / campaign["fuzz_target"],
        "fuzz_manifest_sha256": FUZZ_DIR / "Cargo.toml",
        "fuzz_lock_sha256": FUZZ_DIR / "Cargo.lock",
    }
    for field, path in source_paths.items():
        require(
            campaign.get(field) == sha256(path),
            f"{name}: {field} does not match {path.relative_to(CRATE_DIR)}",
            failures,
        )

    seed_dir = FUZZ_DIR / campaign["seed_corpus"]
    count, total_bytes, digest = content_multiset(seed_dir)
    require(campaign.get("seed_file_count") == count, f"{name}: seed count drift", failures)
    require(campaign.get("seed_total_bytes") == total_bytes, f"{name}: seed bytes drift", failures)
    require(
        campaign.get("seed_content_multiset_sha256") == digest,
        f"{name}: seed content multiset drift",
        failures,
    )

    require(campaign.get("executions", 0) > 0, f"{name}: no executions recorded", failures)
    require(campaign.get("new_units", -1) >= 0, f"{name}: invalid new-unit count", failures)
    require(campaign.get("peak_rss_mib", 0) > 0, f"{name}: no peak RSS recorded", failures)
    require(campaign.get("exit_status") == 0, f"{name}: nonzero exit status", failures)
    require(campaign.get("defects") == [], f"{name}: defects are not empty", failures)
    output_hash = campaign.get("output_corpus_content_multiset_sha256", "")
    require(
        bool(SHA256_PATTERN.fullmatch(output_hash)),
        f"{name}: invalid output-corpus hash",
        failures,
    )


def verify(evidence: dict[str, Any]) -> list[str]:
    failures: list[str] = []
    require(evidence.get("schema_version") == 1, "unsupported evidence schema", failures)

    toolchain = evidence.get("toolchain", {})
    require(
        toolchain.get("rustup_toolchain") == "nightly-2026-07-22",
        "Rust nightly pin drift",
        failures,
    )
    require(
        toolchain.get("rustc") == "rustc 1.99.0-nightly (0e29c21d9 2026-07-21)",
        "rustc version drift",
        failures,
    )
    require(toolchain.get("cargo_fuzz") == "cargo-fuzz 0.13.2", "cargo-fuzz pin drift", failures)
    require(toolchain.get("sanitizer") == "address", "sanitizer drift", failures)

    options = evidence.get("libfuzzer_options", {})
    expected_options = {
        "max_total_time_seconds": 600,
        "max_len": 4096,
        "seed": 424242,
        "rss_limit_mib": 2048,
        "timeout_seconds": 5,
        "print_final_stats": 1,
        "verbosity": 0,
    }
    require(options == expected_options, "libFuzzer options drift", failures)

    campaigns = evidence.get("campaigns", {})
    require(set(campaigns) == {"parser", "incremental_edits"}, "campaign set drift", failures)
    for name in ("parser", "incremental_edits"):
        if name in campaigns:
            verify_campaign(name, campaigns[name], failures)

    return failures


def main() -> int:
    evidence = json.loads(EVIDENCE_PATH.read_text(encoding="utf-8"))
    failures = verify(evidence)

    if failures:
        for failure in failures:
            print(f"ERROR: {failure}", file=sys.stderr)
        return 1
    print("verified 2 ASan campaign records and their current source/seed bindings")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
