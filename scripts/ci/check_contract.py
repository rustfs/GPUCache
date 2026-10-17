#!/usr/bin/env python3
# Copyright 2024-2026 RustFS, Inc.
# SPDX-License-Identifier: Apache-2.0
"""Check the V3 dependency boundaries using Cargo's resolved metadata graph."""

import argparse
import json
import pathlib
import subprocess
import sys


ALLOWED = {
    "gpucache-core-types": {"sha2", "proptest"},
    "gpucache-protocol": {"gpucache-core-types", "proptest"},
    "gpucache-storage": {"gpucache-core-types", "gpucache-protocol", "proptest"},
    "gpucache-transport": {
        "gpucache-core-types", "gpucache-protocol", "gpucache-storage"
    },
    "gpucache-cold": {"gpucache-core-types", "sha2"},
    "gpucache-dpu": {"gpucache-core-types"},
    "gpucache-bench": set(),
}


def check_metadata(metadata):
    """Return violations; dependency aliases cannot bypass package checks."""
    errors = []
    members = set(metadata["workspace_members"])
    packages = {p["name"]: p for p in metadata["packages"] if p["id"] in members}
    root = pathlib.Path(metadata["workspace_root"]).resolve()
    for name in sorted(set(packages) - set(ALLOWED)):
        errors.append("unreviewed workspace crate: " + name)
    for name in sorted(set(ALLOWED) - set(packages)):
        errors.append("missing workspace crate: " + name)
    for name, package in sorted(packages.items()):
        expected = root / "crates" / name.removeprefix("gpucache-") / "Cargo.toml"
        if pathlib.Path(package["manifest_path"]).resolve() != expected:
            errors.append(name + ": workspace manifest is outside its reviewed crate directory")
        for dependency in package["dependencies"]:
            # Cargo reports the actual package name even when `package = ...`
            # or target-specific dependency tables rename the local binding.
            actual = dependency["name"]
            if actual not in ALLOWED.get(name, set()):
                errors.append("{}: dependency {} is not allowed".format(name, actual))
            if actual in ALLOWED:
                expected_dependency = packages.get(actual)
                actual_path = dependency.get("path")
                if not expected_dependency or not actual_path or pathlib.Path(actual_path).resolve() != pathlib.Path(
                    expected_dependency["manifest_path"]
                ).resolve().parent:
                    errors.append("{}: {} must resolve to its reviewed workspace crate".format(name, actual))
            elif actual in {"sha2", "proptest"} and (
                dependency.get("path") is not None
                or not (dependency.get("source") or "").startswith("registry+")
            ):
                errors.append("{}: {} must be a locked registry dependency".format(name, actual))
            if actual == "proptest" and dependency.get("kind") != "dev":
                errors.append(name + ": proptest must remain a dev dependency")
        for feature in ("rdma-nixl", "dpu-bf3"):
            values = package.get("features", {}).get(feature)
            if values is not None and values != []:
                errors.append("{}: {} must remain an empty feature slot".format(name, feature))
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=pathlib.Path,
                        default=pathlib.Path(__file__).resolve().parents[2])
    args = parser.parse_args(argv)
    try:
        completed = subprocess.run(
            ["cargo", "metadata", "--locked", "--offline", "--no-deps", "--format-version", "1"],
            cwd=args.repo, capture_output=True, text=True, timeout=60, check=False,
        )
        if completed.returncode:
            print("CONTRACT_FAILED: cargo metadata failed", file=sys.stderr)
            return 1
        errors = check_metadata(json.loads(completed.stdout))
    except (OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print("CONTRACT_FAILED: " + type(error).__name__, file=sys.stderr)
        return 1
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print("status=CPU_DEPENDENCY_CONTRACT_OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
