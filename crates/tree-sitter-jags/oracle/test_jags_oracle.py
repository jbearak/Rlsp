from __future__ import annotations

import json
from pathlib import Path
import tempfile
import time
import unittest

import jags_oracle


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
