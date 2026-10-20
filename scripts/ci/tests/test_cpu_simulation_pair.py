# Copyright 2024-2026 RustFS, Inc.
# SPDX-License-Identifier: Apache-2.0
import hashlib
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import cpu_simulation_pair as pair


class PairTests(unittest.TestCase):
    def reports(self):
        digest = hashlib.sha256(b"shared-kv-block").hexdigest()
        return (
            dict(status="DPU_CONTRACT_OK", hardware="NOT_RUN_HARDWARE",
                 offload_active="false", message_path="HOST_SIMULATION",
                 fault_path="QUARANTINE_DRAIN_PASS",
                 performance="NOT_A_PERFORMANCE_RESULT", payload_sha256=digest),
            dict(status="REUSE_OK", bytes_match="PASS",
                 performance="NOT_A_PERFORMANCE_RESULT", payload_sha256=digest),
            dict(status="FALLBACK_OK"),
        )

    def test_missing_wrong_and_contradictory_evidence_fails(self):
        pair.validate(*self.reports())
        for report_index, key, value in (
            (0, "offload_active", "true"), (0, "payload_sha256", "incorrect"),
            (1, "bytes_match", "FAIL"), (2, "status", "FAIL"),
        ):
            reports = self.reports()
            reports[report_index][key] = value
            with self.assertRaises(ValueError):
                pair.validate(*reports)
        with self.assertRaises(ValueError):
            pair.fields("status=PASS\nstatus=FAIL\n")
        reports = self.reports()
        del reports[0]["hardware"]
        with self.assertRaises(ValueError):
            pair.validate(*reports)


if __name__ == "__main__":
    unittest.main()
