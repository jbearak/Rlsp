from __future__ import annotations

import json
from pathlib import Path
import tempfile
import time
import unittest

import jags_oracle


def write_external_fixture(root: Path) -> tuple[Path, Path, bytes]:
    manifest = {
        "schema_version": 1,
        "language": "jags",
        "sources": [{
            "id": "example-source",
            "discovery": [{
                "kind": "complete",
                "raven_mode": "syntax-only",
                "oracle_mode": "jags-model-in",
            }],
        }],
    }
    manifest_path = root / "jags.json"
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
    source = b"model { x <- 1 }\n"
    source_path = root / "materialized/cases/example/model.bug"
    source_path.parent.mkdir(parents=True)
    source_path.write_bytes(source)
    index = {
        "schema_version": 1,
        "manifest_binding": [{
            "path": "jags.json",
            "sha256": jags_oracle.sha256(manifest_path),
        }],
        "cases": [{
            "id": "example-source:model.bug",
            "language": "jags",
            "source_id": "example-source",
            "materialized_path": "cases/example/model.bug",
            "sha256": jags_oracle.sha256_bytes(source),
            "kind": "complete",
            "raven_mode": "syntax-only",
            "oracle_mode": "jags-model-in",
        }],
        "counts": {"total": 1, "stan": 0, "jags": 1},
    }
    (root / "materialized/index.json").write_text(
        json.dumps(index), encoding="utf-8"
    )
    return manifest_path, source_path, source


class OracleHarnessTests(unittest.TestCase):
    def test_unpinned_executables_fail_closed(self) -> None:
        provenance = json.loads(
            jags_oracle.DEFAULT_PROVENANCE.read_text(encoding="utf-8")
        )
        with tempfile.TemporaryDirectory(prefix="raven-jags-hash-test-") as directory:
            wrapper = Path(directory) / "jags"
            terminal = Path(directory) / "jags-terminal"
            wrapper.write_bytes(b"not the pinned wrapper\n")
            terminal.write_bytes(b"not the pinned terminal\n")
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                jags_oracle.validate_oracle_hashes(
                    wrapper,
                    terminal,
                    provenance,
                    allow_unpinned=False,
                )
            binding = jags_oracle.validate_oracle_hashes(
                wrapper,
                terminal,
                provenance,
                allow_unpinned=True,
            )
            self.assertEqual(binding["binding"], "override")

    def test_timeout_is_bounded_and_temporary_model_is_removed(self) -> None:
        before = set(Path(tempfile.gettempdir()).glob("raven-jags-oracle-*"))
        with tempfile.TemporaryDirectory(prefix="raven-jags-timeout-test-") as directory:
            executable = Path(directory) / "slow-jags"
            executable.write_text("#!/bin/sh\nsleep 10\n", encoding="utf-8")
            executable.chmod(0o755)
            started = time.monotonic()
            result = jags_oracle.run_probe(
                executable,
                b"model { x <- 1 }\n",
                compile_model=False,
                timeout_seconds=0.02,
            )
            elapsed = time.monotonic() - started
        after = set(Path(tempfile.gettempdir()).glob("raven-jags-oracle-*"))
        self.assertTrue(result["timed_out"])
        self.assertLess(elapsed, 1.0)
        self.assertEqual(after, before)

    def test_external_cases_verify_manifest_index_and_file_hashes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="raven-jags-external-test-") as directory:
            root = Path(directory)
            manifest_path, _, source = write_external_fixture(root)
            cases, inputs = jags_oracle.external_cases(manifest_path, root)
            manifest_hash = jags_oracle.sha256(manifest_path)
        self.assertEqual(len(cases), 1)
        self.assertEqual(cases[0]["id"], "example-source:model.bug")
        self.assertEqual(cases[0]["source_bytes"], source)
        self.assertEqual(inputs["manifest_sha256"], manifest_hash)

    def test_external_cases_reject_materialized_hash_drift(self) -> None:
        with tempfile.TemporaryDirectory(prefix="raven-jags-external-test-") as directory:
            root = Path(directory)
            manifest_path, source_path, _ = write_external_fixture(root)
            source_path.write_bytes(b"model { changed <- 1 }\n")
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                jags_oracle.external_cases(manifest_path, root)

    def test_external_results_bind_every_case_and_outcome(self) -> None:
        provenance = json.loads(
            jags_oracle.DEFAULT_PROVENANCE.read_text(encoding="utf-8")
        )
        with tempfile.TemporaryDirectory(prefix="raven-jags-external-test-") as directory:
            root = Path(directory)
            manifest_path, _, _ = write_external_fixture(root)
            cases, inputs = jags_oracle.external_cases(manifest_path, root)
        committed = {
            "schema_version": 1,
            "inputs": {
                "manifest_sha256": inputs["manifest_sha256"],
                "source_set_sha256": inputs["source_set_sha256"],
            },
            "oracle": {
                "version": provenance["target"]["version"],
                "wrapper_sha256": provenance["homebrew_installation"]["wrapper_sha256"],
                "terminal_sha256": provenance["homebrew_installation"]["terminal_sha256"],
                "binding": "pinned-homebrew-arm64",
            },
            "results": [jags_oracle.external_result_record(cases[0], False)],
            "failures": [],
        }
        provenance["official_model_holdout"]["accepted_count"] = 0
        provenance["official_model_holdout"]["outcomes_sha256"] = (
            jags_oracle.external_outcomes_sha256(committed["results"])
        )
        self.assertEqual(
            jags_oracle.verify_external_results(
                committed, provenance, inputs, cases
            ),
            [],
        )
        committed["results"][0]["sha256"] = "0" * 64
        self.assertTrue(
            jags_oracle.verify_external_results(
                committed, provenance, inputs, cases
            )
        )

    def test_external_results_verify_independent_accepted_outcome_binding(self) -> None:
        provenance = json.loads(
            jags_oracle.DEFAULT_PROVENANCE.read_text(encoding="utf-8")
        )
        with tempfile.TemporaryDirectory(prefix="raven-jags-external-test-") as directory:
            root = Path(directory)
            manifest_path, _, _ = write_external_fixture(root)
            cases, inputs = jags_oracle.external_cases(manifest_path, root)
        records = [jags_oracle.external_result_record(cases[0], True)]
        committed = {
            "schema_version": 1,
            "inputs": {
                "manifest_sha256": inputs["manifest_sha256"],
                "source_set_sha256": inputs["source_set_sha256"],
            },
            "oracle": {
                "version": provenance["target"]["version"],
                "wrapper_sha256": provenance["homebrew_installation"]["wrapper_sha256"],
                "terminal_sha256": provenance["homebrew_installation"]["terminal_sha256"],
                "binding": "pinned-homebrew-arm64",
            },
            "results": records,
            "failures": [],
        }
        provenance["official_model_holdout"]["accepted_count"] = 1
        provenance["official_model_holdout"]["outcomes_sha256"] = (
            jags_oracle.external_outcomes_sha256(records)
        )
        self.assertEqual(
            jags_oracle.verify_external_results(
                committed, provenance, inputs, cases
            ),
            [],
        )
        provenance["official_model_holdout"]["accepted_count"] = 0
        provenance["official_model_holdout"]["outcomes_sha256"] = "0" * 64
        failures = jags_oracle.verify_external_results(
            committed, provenance, inputs, cases
        )
        self.assertTrue(any("accepted count drifted" in item for item in failures))
        self.assertTrue(any("outcome digest drifted" in item for item in failures))

    def test_committed_results_are_bound_to_current_inputs(self) -> None:
        manifest = json.loads(jags_oracle.DEFAULT_MATRIX.read_text(encoding="utf-8"))
        corpus = json.loads(jags_oracle.DEFAULT_CORPUS.read_text(encoding="utf-8"))
        provenance = json.loads(
            jags_oracle.DEFAULT_PROVENANCE.read_text(encoding="utf-8")
        )
        report = json.loads(jags_oracle.DEFAULT_RESULTS.read_text(encoding="utf-8"))
        cases = jags_oracle.matrix_cases(manifest) + jags_oracle.quality_cases(corpus)
        binding = jags_oracle.input_binding(
            jags_oracle.DEFAULT_MATRIX,
            jags_oracle.DEFAULT_CORPUS,
            cases,
        )
        self.assertEqual(
            jags_oracle.verify_committed_results(
                report,
                provenance,
                binding,
                cases,
            ),
            [],
        )


if __name__ == "__main__":
    unittest.main()
