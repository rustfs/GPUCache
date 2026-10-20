#!/usr/bin/env python3
# Copyright 2024-2026 RustFS, Inc.
# SPDX-License-Identifier: Apache-2.0
"""Compare deterministic CPU simulation and TCP functional evidence, never timing."""
import hashlib
import pathlib
import subprocess

from vm_regression import NO_LIVE_ENDPOINT, sanitized_environment


def fields(text):
    result = {}
    for line in text.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            if key in result:
                raise ValueError("duplicate report field: " + key)
            result[key] = value
    return result


def validate(dpu, tcp, fault):
    expected = hashlib.sha256(b"shared-kv-block").hexdigest()
    required = (
        (dpu, "status", "DPU_CONTRACT_OK"),
        (dpu, "hardware", "NOT_RUN_HARDWARE"),
        (dpu, "offload_active", "false"),
        (dpu, "message_path", "HOST_SIMULATION"),
        (dpu, "fault_path", "QUARANTINE_DRAIN_PASS"),
        (dpu, "performance", "NOT_A_PERFORMANCE_RESULT"),
        (tcp, "status", "REUSE_OK"),
        (tcp, "bytes_match", "PASS"),
        (tcp, "performance", "NOT_A_PERFORMANCE_RESULT"),
        (fault, "status", "FALLBACK_OK"),
        (dpu, "payload_sha256", expected),
        (tcp, "payload_sha256", expected),
    )
    for report, key, value in required:
        if report.get(key) != value:
            raise ValueError("functional pair mismatch: " + key)


def main():
    root = pathlib.Path(__file__).resolve().parents[2]
    reports = []
    for package, binary in (
        ("gpucache-dpu", "dpu-demo"),
        ("gpucache-transport", "share-demo"),
        ("gpucache-transport", "fault-demo"),
    ):
        command = ["cargo", *NO_LIVE_ENDPOINT, "run", "--locked", "--offline",
                   "-p", package, "--bin", binary, "--quiet"]
        completed = subprocess.run(command, cwd=root, env=sanitized_environment(0),
                                   capture_output=True, text=True, timeout=60, check=True)
        for line in completed.stdout.splitlines():
            print(binary + "." + line)
        reports.append(fields(completed.stdout))
    validate(*reports)
    print("status=CPU_SIMULATION_PAIR_OK")
    print("comparison=FUNCTIONAL_TCP_AND_HOST_SIMULATION")
    print("hardware=NOT_RUN_HARDWARE")
    print("performance=NOT_A_PERFORMANCE_RESULT")
    print("tco=TCO_UNKNOWN")


if __name__ == "__main__":
    main()
