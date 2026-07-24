import copy
import json
import unittest

import verify_evidence


class EvidenceVerifierTests(unittest.TestCase):
    def test_fuzz_manifest_hash_mismatch_fails_closed(self) -> None:
        evidence = json.loads(
            verify_evidence.EVIDENCE_PATH.read_text(encoding="utf-8")
        )
        evidence = copy.deepcopy(evidence)
        evidence["campaigns"]["incremental_edits"]["fuzz_manifest_sha256"] = "0" * 64

        failures = verify_evidence.verify(evidence)

        self.assertIn(
            "incremental_edits: fuzz_manifest_sha256 does not match fuzz/Cargo.toml",
            failures,
        )


if __name__ == "__main__":
    unittest.main()
