#!/usr/bin/env python3
# Copyright 2024-2026 RustFS, Inc.
# SPDX-License-Identifier: Apache-2.0
"""Run the portable CPU gates and write reproducible VM regression evidence.

No third-party Python packages, root access, device writes, model downloads,
cloud services, or live RustFS credentials are required. Hardware profiles
fail closed until real qualified implementations exist in this repository.
"""

import argparse
import datetime
import hashlib
import json
import os
import pathlib
import platform
import re
import signal
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
from dataclasses import dataclass


SCHEMA_VERSION = 1
HARDWARE = ("rdma", "gds", "bf3", "gpu-engine")
PASS = "PASS"
FAIL = "FAIL"
NOT_RUN = "NOT_RUN_HARDWARE"
NO_LIVE_ENDPOINT = (
    "--config", 'env.GPUCACHE_RUSTFS_ENDPOINT.value=""',
    "--config", "env.GPUCACHE_RUSTFS_ENDPOINT.force=true",
    "--config", "env.GPUCACHE_RUSTFS_ENDPOINT.relative=false",
)


@dataclass(frozen=True)
class Gate:
    name: str
    argv: tuple
    expected: tuple = ()


def cpu_gates(offline=False):
    # Highest-precedence Cargo config also blocks [env] injection from a
    # checkout or an operator's global Cargo config, including force=true.
    cargo = ("cargo",) + NO_LIVE_ENDPOINT
    locked = ("--locked",) + (("--offline",) if offline else ())
    gates = [
        Gate("format", cargo + ("fmt", "--all", "--", "--check")),
        Gate("locked-dependencies", cargo + ("fetch",) + locked),
        Gate("dependency-contract", (sys.executable, "scripts/ci/check_contract.py"),
             ("status=CPU_DEPENDENCY_CONTRACT_OK",)),
        Gate("runner-tests", (sys.executable, "-m", "unittest", "discover",
                              "-s", "scripts/ci/tests", "-v")),
        Gate("clippy-default", cargo + ("clippy", "--workspace", "--all-targets")
             + locked + ("--", "-D", "warnings")),
        Gate("clippy", cargo + ("clippy", "--workspace", "--all-targets", "--all-features")
             + locked + ("--", "-D", "warnings")),
        Gate("workspace-tests", cargo + ("test", "--workspace", "--all-targets") + locked),
        Gate("doc-tests", cargo + ("test", "--workspace", "--doc") + locked),
        Gate("rdma-feature-contract", cargo + ("test", "-p", "gpucache-transport",
                                               "--all-targets", "--features", "rdma-nixl") + locked),
        Gate("dpu-feature-contract", cargo + ("test", "-p", "gpucache-dpu",
                                              "--all-targets", "--features", "dpu-bf3") + locked),
        Gate("combined-feature-tests", cargo + ("test", "--workspace", "--all-targets",
                                                "--all-features") + locked),
    ]
    demos = [
        ("share-demo", "gpucache-transport", "share-demo", ("status=REUSE_OK",)),
        ("fault-demo", "gpucache-transport", "fault-demo", ("status=FALLBACK_OK",)),
        ("shard-demo", "gpucache-transport", "shard-demo",
         ("status=SHARD_OK", "registration=SIMULATED", "rdma=NOT_RUN_HARDWARE")),
        ("cold-demo", "gpucache-cold", "cold-demo",
         ("status=COLD_CONTRACT_OK", "rustfs_live=NOT_RUN_HARDWARE")),
        ("dpu-demo", "gpucache-dpu", "dpu-demo",
         ("status=DPU_CONTRACT_OK", "offload_active=false", "hardware=NOT_RUN_HARDWARE")),
        ("paired-baseline", "gpucache-bench", "paired-baseline", ("status=NOT_RUN_HARDWARE",)),
        ("v3-paired", "gpucache-bench", "v3-paired",
         ("status=NOT_RUN_HARDWARE", "tco=TCO_UNKNOWN", "go_gate=NOT_EVALUATED")),
    ]
    for name, package, binary, expected in demos:
        gates.append(Gate(name, cargo + ("run", "-p", package, "--bin", binary, "--quiet")
                          + locked, expected))
    return gates


def sanitized_environment(seed):
    env = os.environ.copy()
    # CPU regression must never probe an operator's configured live endpoint.
    for key in list(env):
        if key.startswith("GPUCACHE_RUSTFS_"):
            del env[key]
    env["GPUCACHE_RUSTFS_ENDPOINT"] = ""
    env.pop("PROPTEST_RNG_SEED", None)
    env["CARGO_TERM_COLOR"] = "never"
    env["RUST_BACKTRACE"] = "1"
    # proptest 1.4 does not consume PROPTEST_RNG_SEED. The campaign seed is
    # report metadata only; failing property cases use proptest persistence.
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return env


def execute(gate, repo, log, timeout, env):
    started = time.monotonic()
    returncode = None
    reason = None
    with log.open("wb") as output:
        try:
            child = subprocess.Popen(
                list(gate.argv), cwd=repo, env=env, stdout=output,
                stderr=subprocess.STDOUT, start_new_session=(os.name == "posix"),
            )
        except OSError as error:
            reason = "launch failed: " + type(error).__name__
        else:
            try:
                returncode = child.wait(timeout=timeout)
            except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                reason = "timed out" if isinstance(error, subprocess.TimeoutExpired) else "interrupted"
            finally:
                # Include child Cargo/test processes; leaving a cache server or
                # a cargo lock behind makes the next VM run nondeterministic.
                # A failed command can leave descendants even after it exits.
                try:
                    if os.name == "posix":
                        os.killpg(child.pid, signal.SIGKILL)
                    elif child.poll() is None:
                        child.kill()
                except ProcessLookupError:
                    pass
                returncode = child.wait()
    text = log.read_text(encoding="utf-8", errors="replace")
    lines = set(text.splitlines())
    missing = [marker for marker in gate.expected if marker not in lines]
    contradictory = [line for marker in gate.expected for line in lines
                     if "=" in marker and line.startswith(marker.split("=", 1)[0] + "=")
                     and line != marker]
    if reason is None and returncode != 0:
        reason = "nonzero exit"
    if reason is None and missing:
        reason = "missing required output markers"
    if reason is None and contradictory:
        reason = "contradictory output markers"
    # This is elapsed command runtime, never a GPU/engine latency benchmark.
    return {
        "name": gate.name, "status": FAIL if reason else PASS,
        "command": list(gate.argv), "returncode": returncode,
        "duration_seconds": round(time.monotonic() - started, 6),
        "reason": reason, "missing_markers": missing,
        "contradictory_markers": sorted(contradictory),
        "log": str(log.relative_to(log.parent.parent)),
        "rust_tests": [
            {"name": match.group(1), "status": match.group(2)}
            for match in re.finditer(r"^test (.+?) \.\.\. (ok|FAILED|ignored)$", text, re.MULTILINE)
        ],
    }


def probe(argv, repo):
    try:
        result = subprocess.run(argv, cwd=repo, capture_output=True, text=True,
                                timeout=10, check=False)
        return result.stdout.strip() if result.returncode == 0 else "UNAVAILABLE"
    except (OSError, subprocess.TimeoutExpired):
        return "UNAVAILABLE"


def environment_report(repo, seed):
    lock = repo / "Cargo.lock"
    return {
        "schema_version": SCHEMA_VERSION,
        "recorded_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "repository_commit": probe(["git", "rev-parse", "HEAD"], repo),
        "working_tree_dirty": probe(["git", "status", "--porcelain", "--untracked-files=normal"], repo) != "",
        "cargo_lock_sha256": hashlib.sha256(lock.read_bytes()).hexdigest() if lock.is_file() else None,
        "os": platform.system(), "os_release": platform.release(),
        "architecture": platform.machine(), "python": platform.python_version(),
        "rustc": probe(["rustc", "--version", "--verbose"], repo),
        "cargo": probe(["cargo", "--version"], repo),
        "seed": seed,
        "seed_scope": "campaign metadata; proptest uses its own persisted failure seeds",
        "live_rustfs_probe": "DISABLED_FOR_CPU_REGRESSION",
        "performance_measurements": "NONE",
    }


def capability_report(cpu_status):
    return {
        "schema_version": SCHEMA_VERSION,
        "cpu_contract": cpu_status,
        "simulated_registration": "IMPLEMENTED_SIMULATED",
        "simulated_dpu_queue": "IMPLEMENTED_SIMULATED",
        "rdma": NOT_RUN, "gds": NOT_RUN, "bf3": NOT_RUN,
        "gpu-engine": NOT_RUN, "live_rustfs": NOT_RUN,
        "hardware_qualification": "INCOMPLETE",
        "tco": "TCO_UNKNOWN", "fleet_qualification": "NOT_RUN_HARDWARE",
    }


def write_json(path, data):
    # Atomic replacement avoids a half-written PASS document after interruption.
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def write_reports(output, environment, results, status, profile):
    capabilities = capability_report(status if profile == "cpu" else "NOT_RUN")
    write_json(output / "environment.json", environment)
    write_json(output / "capabilities.json", capabilities)
    write_json(output / "results.json", {
        "schema_version": SCHEMA_VERSION, "profile": profile, "status": status,
        "required_gates": results, "hardware_qualification": "INCOMPLETE",
    })
    suite = ET.Element("testsuite", name="gpucache-vm-" + profile,
                       tests=str(len(results)),
                       failures=str(sum(row["status"] != PASS for row in results)))
    for row in results:
        case = ET.SubElement(suite, "testcase", name=row["name"],
                             time=str(row["duration_seconds"]))
        if row["status"] != PASS:
            ET.SubElement(case, "failure", message=row["reason"] or row["status"])
        ET.SubElement(case, "system-out").text = row["log"]
    ET.ElementTree(suite).write(output / "junit.xml", encoding="utf-8", xml_declaration=True)
    lines = ["# GPUCache VM regression", "", "Profile: `{}`".format(profile),
             "CPU/selected-profile result: **{}**".format(status), "",
             "Hardware qualification: **INCOMPLETE**. RDMA, GDS, GPU engine and BF3 are `NOT_RUN_HARDWARE`.",
             "Command durations are execution times, not TTFT, TPOT or goodput.", "",
             "| Gate | Result | Log |", "| --- | --- | --- |"]
    lines.extend("| {} | {} | {} |".format(row["name"], row["status"], row["log"])
                 for row in results)
    (output / "summary.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def positive_number(value):
    parsed = float(value)
    if not (0 < parsed < float("inf")):
        raise argparse.ArgumentTypeError("must be finite and positive")
    return parsed


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("cpu",) + HARDWARE, default="cpu")
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path("artifacts/vm-regression"))
    parser.add_argument("--repo", type=pathlib.Path,
                        default=pathlib.Path(__file__).resolve().parents[2])
    parser.add_argument("--timeout", type=positive_number, default=900,
                        help="timeout in seconds for each gate")
    parser.add_argument("--seed", type=int, default=20261017)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args(argv)
    repo = args.repo.resolve()
    output = args.output.resolve()
    if not (repo / "Cargo.toml").is_file():
        parser.error("--repo must contain the GPUCache Cargo workspace")
    # Use a new report directory. Never overwrite another run's evidence or
    # clean an operator-supplied arbitrary directory.
    if output.exists() and (not output.is_dir() or any(output.iterdir())):
        parser.error("--output must be empty or new; preserve previous regression evidence")
    output.mkdir(parents=True, exist_ok=True)
    (output / "logs").mkdir()
    environment = environment_report(repo, args.seed)
    results = []
    write_reports(output, environment, results, "RUNNING", args.profile)
    if args.profile != "cpu":
        results.append({
            "name": args.profile + "-qualification", "status": NOT_RUN,
            "command": [], "returncode": None, "duration_seconds": 0,
            "reason": "real hardware adapter and qualification runner are not implemented",
            "missing_markers": [], "log": "", "rust_tests": [],
        })
        write_reports(output, environment, results, NOT_RUN, args.profile)
        print("status=NOT_RUN_HARDWARE profile=" + args.profile)
        return 2
    env = sanitized_environment(args.seed)
    for index, gate in enumerate(cpu_gates(args.offline)):
        log = output / "logs" / "{:02d}-{}.log".format(index + 1, gate.name)
        print("running=" + gate.name, flush=True)
        row = execute(gate, repo, log, args.timeout, env)
        results.append(row)
        write_reports(output, environment, results, "RUNNING", args.profile)
        print("gate={} status={}".format(gate.name, row["status"]), flush=True)
        if row["reason"] == "interrupted":
            break
    status = PASS if len(results) == len(cpu_gates(args.offline)) and all(
        row["status"] == PASS for row in results
    ) else FAIL
    write_reports(output, environment, results, status, args.profile)
    print("status={} hardware=NOT_RUN_HARDWARE report={}".format(status, output / "summary.md"))
    return 0 if status == PASS else 1


if __name__ == "__main__":
    sys.exit(main())
