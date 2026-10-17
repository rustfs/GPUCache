# Copyright 2024-2026 RustFS, Inc.
# SPDX-License-Identifier: Apache-2.0
"""Runner regressions; an optional tiny Cargo probe checks endpoint isolation."""

import contextlib
import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import xml.etree.ElementTree as ET

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import check_contract as contract  # noqa: E402
import vm_regression as vm  # noqa: E402


class ExecutionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="gpucache-runner-test-")
        self.root = pathlib.Path(self.directory.name)
        (self.root / "logs").mkdir()

    def tearDown(self):
        self.directory.cleanup()

    def execute(self, code, expected=(), timeout=5):
        gate = vm.Gate("probe", (sys.executable, "-c", code), expected)
        return vm.execute(gate, self.root, self.root / "logs" / "probe.log",
                          timeout, vm.sanitized_environment(7))

    def test_success_requires_exit_and_expected_markers(self):
        result = self.execute("print('status=READY')", ("status=READY",))
        self.assertEqual(result["status"], vm.PASS)
        self.assertEqual(result["returncode"], 0)
        self.assertEqual(result["log"], "logs/probe.log")

    def test_zero_exit_with_missing_contract_is_failure(self):
        result = self.execute("print('not the contract')", ("status=READY",))
        self.assertEqual(result["status"], vm.FAIL)
        self.assertEqual(result["missing_markers"], ["status=READY"])

    def test_success_marker_must_be_a_complete_line(self):
        result = self.execute("print('error=status=READY_NOT_READY')", ("status=READY",))
        self.assertEqual(result["status"], vm.FAIL)

    def test_contradictory_success_markers_fail_closed(self):
        result = self.execute("print('status=READY'); print('status=FAILED')", ("status=READY",))
        self.assertEqual(result["status"], vm.FAIL)
        self.assertEqual(result["contradictory_markers"], ["status=FAILED"])

    def test_failure_cannot_be_hidden_by_success_banner(self):
        result = self.execute("print('status=READY'); raise SystemExit(7)", ("status=READY",))
        self.assertEqual(result["status"], vm.FAIL)
        self.assertEqual(result["returncode"], 7)

    def test_missing_executable_is_reported(self):
        gate = vm.Gate("missing", (str(self.root / "nonexistent-executable"),))
        result = vm.execute(gate, self.root, self.root / "logs" / "missing.log", 1, {})
        self.assertEqual(result["status"], vm.FAIL)
        self.assertIsNone(result["returncode"])

    def test_timeout_terminates_process(self):
        result = self.execute("import time; time.sleep(60)", timeout=0.2)
        self.assertEqual(result["status"], vm.FAIL)
        self.assertEqual(result["reason"], "timed out")
        self.assertLess(result["duration_seconds"], 5)

    @unittest.skipUnless(os.name == "posix", "POSIX process groups")
    def test_timeout_kills_the_entire_child_process_group(self):
        child = mock.Mock(pid=1234)
        child.wait.side_effect = [subprocess.TimeoutExpired(["fake"], 1), -9]
        with mock.patch.object(vm.subprocess, "Popen", return_value=child), \
                mock.patch.object(vm.os, "killpg") as kill:
            result = vm.execute(vm.Gate("timeout", ("fake",)), self.root,
                                self.root / "logs" / "timeout.log", 1, {})
        kill.assert_called_once_with(1234, vm.signal.SIGKILL)
        self.assertEqual(result["reason"], "timed out")

    @unittest.skipUnless(os.name == "posix", "POSIX process groups")
    def test_failed_parent_exit_still_cleans_up_descendant_processes(self):
        child = mock.Mock(pid=1234)
        child.wait.return_value = 3
        with mock.patch.object(vm.subprocess, "Popen", return_value=child), \
                mock.patch.object(vm.os, "killpg") as kill:
            result = vm.execute(vm.Gate("failed", ("fake",)), self.root,
                                self.root / "logs" / "failed.log", 1, {})
        kill.assert_called_once_with(1234, vm.signal.SIGKILL)
        self.assertEqual(result["status"], vm.FAIL)

    def test_individual_rust_test_results_are_preserved(self):
        result = self.execute("print('test module::case ... ok')")
        self.assertEqual(result["rust_tests"], [{"name": "module::case", "status": "ok"}])

    def test_cpu_environment_does_not_enable_a_live_rustfs_probe(self):
        with mock.patch.dict(os.environ, {"GPUCACHE_RUSTFS_ENDPOINT": "https://private",
                                          "GPUCACHE_RUSTFS_SECRET_KEY": "secret"}):
            env = vm.sanitized_environment(7)
        self.assertEqual(env["GPUCACHE_RUSTFS_ENDPOINT"], "")
        self.assertNotIn("GPUCACHE_RUSTFS_SECRET_KEY", env)
        self.assertNotIn("PROPTEST_RNG_SEED", env)


class PlanTests(unittest.TestCase):
    def test_every_cargo_build_uses_the_lockfile(self):
        for gate in vm.cpu_gates():
            if gate.argv[0] == "cargo" and gate.name != "format":
                self.assertIn("--locked", gate.argv, gate.name)

    def test_offline_applies_to_every_cargo_build(self):
        for gate in vm.cpu_gates(offline=True):
            if gate.argv[0] == "cargo" and gate.name != "format":
                self.assertIn("--offline", gate.argv, gate.name)

    def test_cargo_cannot_reenable_live_rustfs_through_global_config(self):
        for gate in vm.cpu_gates():
            if gate.argv[0] == "cargo":
                self.assertEqual(gate.argv[1:1 + len(vm.NO_LIVE_ENDPOINT)], vm.NO_LIVE_ENDPOINT)

    def test_required_gates_are_unique_and_cover_features_and_demos(self):
        names = [gate.name for gate in vm.cpu_gates()]
        self.assertEqual(len(names), len(set(names)))
        for name in ("workspace-tests", "doc-tests", "rdma-feature-contract",
                     "dpu-feature-contract", "combined-feature-tests", "share-demo",
                     "fault-demo", "cold-demo", "dpu-demo", "v3-paired"):
            self.assertIn(name, names)

    def test_invalid_timeouts_are_rejected(self):
        for value in ("0", "-1", "nan", "inf"):
            with self.assertRaises(vm.argparse.ArgumentTypeError):
                vm.positive_number(value)


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="gpucache-report-test-")
        self.repo = pathlib.Path(self.directory.name)
        (self.repo / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
        self.output = self.repo / "report"

    def tearDown(self):
        self.directory.cleanup()

    def run_main(self, *args):
        with mock.patch.object(vm, "environment_report", return_value={"schema_version": 1}), \
                contextlib.redirect_stdout(io.StringIO()):
            return vm.main(["--repo", str(self.repo), "--output", str(self.output)] + list(args))

    def test_all_hardware_profiles_fail_closed_and_write_evidence(self):
        for profile in vm.HARDWARE:
            self.output = self.repo / profile
            with mock.patch.object(vm, "execute") as execute:
                self.assertEqual(self.run_main("--profile", profile), 2)
            execute.assert_not_called()
            result = json.loads((self.output / "results.json").read_text())
            self.assertEqual(result["status"], vm.NOT_RUN)
            capabilities = json.loads((self.output / "capabilities.json").read_text())
            self.assertEqual(capabilities[profile], vm.NOT_RUN)
            self.assertEqual(capabilities["cpu_contract"], "NOT_RUN")
            self.assertEqual(ET.parse(self.output / "junit.xml").getroot().attrib["failures"], "1")

    def test_cpu_success_does_not_qualify_hardware(self):
        gate = vm.Gate("real-python-check", (sys.executable, "-c", "print('checked')"))
        with mock.patch.object(vm, "cpu_gates", return_value=[gate]):
            self.assertEqual(self.run_main(), 0)
        result = json.loads((self.output / "results.json").read_text())
        capabilities = json.loads((self.output / "capabilities.json").read_text())
        self.assertEqual(result["status"], vm.PASS)
        self.assertEqual(capabilities["hardware_qualification"], "INCOMPLETE")
        self.assertEqual(capabilities["rdma"], vm.NOT_RUN)
        self.assertEqual(capabilities["cpu_contract"], vm.PASS)

    def test_failure_is_nonzero_and_remaining_gates_are_collected(self):
        gates = [vm.Gate("fails", (sys.executable, "-c", "raise SystemExit(3)")),
                 vm.Gate("runs-after-failure", (sys.executable, "-c", "print('checked')"))]
        with mock.patch.object(vm, "cpu_gates", return_value=gates):
            self.assertEqual(self.run_main(), 1)
        result = json.loads((self.output / "results.json").read_text())
        self.assertEqual([row["status"] for row in result["required_gates"]], [vm.FAIL, vm.PASS])
        self.assertEqual(result["status"], vm.FAIL)

    def test_nonempty_output_is_not_overwritten(self):
        self.output.mkdir()
        sentinel = self.output / "keep.txt"
        sentinel.write_text("previous evidence", encoding="utf-8")
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
            self.run_main()
        self.assertEqual(raised.exception.code, 2)
        self.assertEqual(sentinel.read_text(), "previous evidence")

    def test_file_output_is_rejected_without_overwriting_it(self):
        self.output.write_text("previous evidence", encoding="utf-8")
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as raised:
            self.run_main()
        self.assertEqual(raised.exception.code, 2)


def metadata(dependencies=(), features=None):
    packages = []
    for name in contract.ALLOWED:
        package = {"id": name, "name": name, "dependencies": [], "features": {},
                   "manifest_path": "/workspace/crates/" + name.removeprefix("gpucache-") + "/Cargo.toml"}
        if name == "gpucache-transport":
            package["dependencies"] = list(dependencies)
            package["features"] = features or {}
        packages.append(package)
    return {"workspace_root": "/workspace", "workspace_members": list(contract.ALLOWED), "packages": packages}


class DependencyContractTests(unittest.TestCase):
    def test_expected_graph_passes(self):
        self.assertEqual(contract.check_metadata(metadata()), [])

    def test_renamed_accelerator_dependency_is_not_allowed(self):
        value = metadata([{"name": "cuda-driver", "rename": "innocent", "kind": None}])
        self.assertTrue(contract.check_metadata(value))

    def test_target_specific_and_build_dependencies_cannot_bypass_check(self):
        for kind in (None, "build", "dev"):
            value = metadata([{"name": "libnixl", "kind": kind, "target": "cfg(target_arch = \"aarch64\")"}])
            self.assertTrue(contract.check_metadata(value))

    def test_same_name_foreign_crate_cannot_replace_a_workspace_dependency(self):
        value = metadata([{"name": "gpucache-core-types", "kind": None, "path": "/foreign"}])
        self.assertTrue(contract.check_metadata(value))

    def test_empty_feature_cannot_enable_another_feature(self):
        value = metadata(features={"rdma-nixl": ["dep:cuda-driver"]})
        self.assertTrue(contract.check_metadata(value))

    def test_unreviewed_workspace_members_fail(self):
        value = metadata()
        value["workspace_members"].append("new")
        value["packages"].append({"id": "new", "name": "new", "dependencies": [], "features": {},
                                  "manifest_path": "/workspace/crates/new/Cargo.toml"})
        self.assertTrue(contract.check_metadata(value))

    def test_missing_workspace_members_fail(self):
        value = metadata()
        value["packages"] = value["packages"][:-1]
        self.assertTrue(contract.check_metadata(value))


@unittest.skipUnless(shutil.which("cargo"), "Cargo required for configuration-injection probe")
class CargoEndpointIsolationTests(unittest.TestCase):
    def test_global_and_checkout_force_relative_config_cannot_enable_a_live_probe(self):
        with tempfile.TemporaryDirectory(prefix="gpucache-cargo-env-") as directory:
            root = pathlib.Path(directory)
            home, project = root / "cargo-home", root / "probe"
            home.mkdir()
            (project / "src").mkdir(parents=True)
            (project / ".cargo").mkdir()
            (project / "Cargo.toml").write_text(
                '[package]\nname="gpucache-env-probe"\nversion="0.0.0"\nedition="2021"\n',
                encoding="utf-8",
            )
            (project / "src/main.rs").write_text(
                'fn main() { if !std::env::var("GPUCACHE_RUSTFS_ENDPOINT").unwrap_or_default().is_empty() { std::process::exit(86); } }',
                encoding="utf-8",
            )
            hostile = ('[env]\nGPUCACHE_RUSTFS_ENDPOINT = '
                       '{ value="https://example.invalid", force=true, relative=true }\n')
            env = vm.sanitized_environment(7)
            env["CARGO_HOME"] = str(home)
            env["CARGO_TARGET_DIR"] = str(root / "target")
            for global_config, checkout_config in ((True, False), (False, True), (True, True)):
                with self.subTest(global_config=global_config, checkout_config=checkout_config):
                    (home / "config.toml").write_text(hostile if global_config else "", encoding="utf-8")
                    (project / ".cargo/config.toml").write_text(hostile if checkout_config else "", encoding="utf-8")
                    baseline = subprocess.run(
                        ["cargo", "run", "--quiet", "--offline"], cwd=project, env=env,
                        capture_output=True, text=True, timeout=60, check=False,
                    )
                    self.assertEqual(baseline.returncode, 86, baseline.stderr)
                    result = subprocess.run(
                        ["cargo"] + list(vm.NO_LIVE_ENDPOINT) + ["run", "--quiet", "--offline"],
                        cwd=project, env=env, capture_output=True, text=True, timeout=60,
                        check=False,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
