# VM regression runbook

This runbook is for a Grok bot or another operator running the repository inside a disposable VM. The runner collects reproducible evidence; it does not contact a bot, allocate a VM, buy hardware, deploy a service or publish results externally.

The current executable profile is the CPU contract. Native RDMA, real engine KV export/import, GDS and BF3 offload are not implemented hardware paths in this tree. Their strict profiles must produce an explicit unavailable result and return nonzero. See [V3 test matrix](V3_TEST_MATRIX.md), [compatibility](../COMPATIBILITY.md) and [roadmap](ROADMAP.md).

## Prerequisites

- A clean checkout at a recorded commit, including `Cargo.lock` and `rust-toolchain.toml`.
- Linux x86_64 or aarch64, Python 3.9 or newer, Git, and a Rust toolchain with Cargo, rustfmt and Clippy. Follow the repository toolchain; the declared MSRV is 1.85.
- Sufficient RAM, CPU, disk space and a writable ordinary temporary directory for the build and file-store tests. Do not point test storage at a production disk or a device node.
- Cargo dependency access, or a prepopulated dependency cache for an offline VM. The Python runner requires no third-party Python package and no `pip install`.
- Loopback networking and child-process execution for TCP and import tests. No GPU, RDMA adapter, DOCA, NIXL or SPDK SDK is required for the CPU profile.

Record resource limits when comparing runs. A VM process-duration comparison is not a GPU inference benchmark. An aarch64 or QEMU run remains CPU evidence, not BlueField evidence.

## Run from the repository root

```sh
python3 scripts/ci/vm_regression.py --profile cpu --output artifacts/vm-regression
```

The output path is relative to the calling directory unless an absolute path is given. It must be new or empty; the runner refuses to overwrite an earlier run. Keep the process exit code and the full artifact directory, including logs. Run `python3 scripts/ci/vm_regression.py --help` for the implemented command-line interface.

| Option | Default | Use |
| --- | --- | --- |
| `--profile` | `cpu` | Select the required qualification profile |
| `--output` | `artifacts/vm-regression` | New or empty artifact directory |
| `--repo` | Repository containing the runner | Explicit checkout root, which must contain `Cargo.toml` |
| `--timeout` | `900` seconds | Positive, finite timeout for each gate; timed-out child process groups are killed |
| `--seed` | `20261017` | Recorded run seed; preserve any failing property-test case separately |
| `--offline` | Off | Pass Cargo `--offline`; dependencies must already be cached |

The CPU profile should cover the repository-required checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

The runner uses `--locked` for Cargo resolution and fetches the locked dependencies before its offline metadata check, so a clean VM does not need an already populated cache unless `--offline` is selected. It checks Clippy with default and all features, checks the dependency contract and runner unit tests, runs doc tests and combined-feature tests, and exercises the CPU-compatible `rdma-nixl` and `dpu-bf3` feature slots. It also runs the reuse, fallback, shard, cold, DPU and paired-report demos. The current list contains 18 gates; the command list in `results.json` is authoritative for a retained run. Optional feature compilation does not link a device, and a demo's successful exit does not convert an unavailable hardware row into a pass.

Manual reproduction of the optional feature contracts is:

```sh
cargo test -p gpucache-transport --all-targets --features rdma-nixl
cargo test -p gpucache-dpu --all-targets --features dpu-bf3
```

## Artifact contract

| Path | Contents | Interpretation |
| --- | --- | --- |
| `environment.json` | Repository revision/state, OS/architecture, available toolchain versions and run metadata | An observed environment manifest, not a certification or a dump of all environment variables |
| `capabilities.json` | CPU and optional path availability, including simulation and unrun hardware limitations | An installed tool or visible device is not proof that the data path executed |
| `results.json` | Selected profile, each command/check, exit/status, duration and log references | Required selected-profile failures remain failures; duration is command wall time, not TTFT/TPOT |
| `summary.md` | Human-readable outcome, failed/unavailable checks and evidence limits | The VM operator can review this first, then verify raw results/logs |
| `junit.xml` | One test case per executed required gate, with failure status and log reference | Additional CI-compatible view; unavailable selected hardware qualification is a failure, not a skipped pass |
| `logs/` | Per-command stdout/stderr and diagnostic output | Retain the evidence behind a result; no credentials, prompts or KV payloads should be added |

JSON reports carry `schema_version: 1`; consumers should reject incompatible versions rather than guess fields. `results.json` uses `required_gates` for the ordered rows. CPU child processes have live RustFS credentials removed and the endpoint forced empty. Every Cargo gate additionally overrides the endpoint with highest-precedence `--config`, so checkout or global Cargo `[env]` settings cannot silently enable a live probe. Environment reporting does not dump those variables.

Do not infer a result from a missing file or empty log. A runner failure before complete reporting is an incomplete run. Do not manufacture results for commands that never started. Intermediate reports use `RUNNING`; that is not a successful completed run.

## Profile and exit policy

| Profile | Current behavior | Required outcome |
| --- | --- | --- |
| `cpu` | Executes portable checks and demos | Zero only when every required CPU check passes; preserve independent `NOT_RUN_HARDWARE` rows |
| `gpu-engine` | Real engine adapter and GPU KV round trip unavailable | `NOT_RUN_HARDWARE`, exit 2 |
| `rdma` | Native NIXL/RDMA backend is not linked | `NOT_RUN_HARDWARE`, exit 2 |
| `gds` | cuFile/GDS path is not linked | `NOT_RUN_HARDWARE`, exit 2 |
| `bf3` | DOCA/SPDK/NVMe device path is not linked | `NOT_RUN_HARDWARE`, exit 2 |

For example, this command must not silently pass on the current tree:

```sh
python3 scripts/ci/vm_regression.py --profile bf3 --output artifacts/vm-regression-bf3
```

Selecting a strict hardware profile must not fall back to TCP, CPU simulation, Soft-RoCE or a stub and report success. A detected GPU/DPU or a successful SDK import cannot override an unimplemented path. Unsupported profiles can still write useful reports before returning nonzero.

A missing tool, timeout, crashed child, failed assertion or invalid invocation returns nonzero. A failed CPU run exits 1; a passed CPU run exits 0. Invalid arguments exit nonzero before running gates and need not produce an artifact directory. A CPU pass means the required CPU checks passed in that environment; it does not mean that unavailable hardware checks passed. Keep `TCO_UNKNOWN`, `go_gate=NOT_EVALUATED` and the absence of performance samples intact.

## What to inspect

1. Confirm the revision and environment match the requested test. Note any dirty checkout; do not label a modified tree as the clean commit.
2. Inspect the selected profile and process exit code. Confirm failures, timeouts and unsupported selected profiles are visible in the summary and results.
3. Verify the identity, tenant, checksum, commit, prefix, timeout/cancel, quarantine and epoch tests passed. The current tests use synthetic bytes.
4. Check that demos retain their scope labels: `registration=SIMULATED`, `dma_posted=false` where reported, `offload_active=false`, `doca=NOT_LINKED` and `NOT_RUN_HARDWARE`. A changed label requires implementation evidence, not a report rewrite.
5. Inspect failures in the raw logs and reproduce the individual command in a fresh temporary environment. Preserve the failing seed and case if a property/fuzz test prints them. A recorded runner seed alone does not guarantee that a particular property-test library consumed it or can replay every failure.
6. Report limitations with the artifact. Current V3 security hooks are not wired into the TCP service; current limits do not bound every reader pin, reply allocation or metadata structure. Hidden simulated slab bytes stay charged until explicit CPU drain/invalidation/reclaim rules permit release and can cause admission denial. Those APIs are not hardware drain evidence. A green CPU report does not remove these limitations.

## Reruns and failure handling

Fix a demonstrated defect, then rerun the affected check and required suite. Keep the failed run separately from the corrected run; do not overwrite failure evidence with a later success. Infrastructure failures and implementation failures may need different fixes, but neither should be rewritten as a pass.

Do not change firmware, BFB, IOMMU/ACS, DPU mode, NIC setup, production mounts or NVMe formatting to make the CPU profile pass. A future hardware campaign needs its own environment manifest and acceptance evidence. Existing [DPU targets](V3_DPU_TARGETS.md) list unset targets, not VM setup commands.

## Proposed hardware qualification

The following is **PROPOSED / NOT IMPLEMENTED**. A later native runner may support device-backed execution after the corresponding backend exists. Its report must identify the actual selected path and retain raw measurements, topology and fault results.

- GPU engine: pinned engine/model, real KV export/import, declared deterministic output criterion, baseline/candidate configurations and fallback cases.
- RDMA: actual HCA/driver/firmware, registered regions, access bounds, completion/drain and NIC-failure evidence. Soft-RoCE can be a labeled functional test, not the native performance baseline.
- GDS: CUDA/cuFile, filesystem/device support, direct versus compatibility/fallback path, alignment and integrity evidence.
- BF3: storage-side topology, BFB/OS/DOCA/firmware, actual queue submit/complete and the paired NIC-RDMA baseline. Arm process start alone is insufficient.

These profiles remain unavailable until implemented and exercised. Physical fleet qualification, a day-long device soak, PB-scale and 10,000-GPU reliability cannot be derived from this VM runner.
