# Compatibility manifest

Recorded for V1 milestones A, B, and C. This file is not a performance acceptance report and not a hardware inventory of a BlueField node.

| Component | Value | Result |
| --- | --- | --- |
| OS | Ubuntu 24.04.5 LTS | Recorded on the V1 A build host |
| Kernel | Linux 6.12.94+ | Recorded on the V1 A build host |
| Architecture exercised locally for V1 C | x86_64 native | `PASS` for `cargo test --workspace --all-targets`, `fault-demo`, and `paired-baseline`. aarch64 was not executed on this host (`NOT_RUN` here). CI still runs `test-aarch64` and `test-aarch64-qemu`. Neither is a BlueField run. |
| Rust | stable 1.99.0 (b940084d7 2026-09-28); MSRV declared as 1.85 | `PASS` |
| Python | 3.12.3 present on the build host | Not used by V1 A |
| CUDA | Not installed | `NOT_RUN_HARDWARE` |
| GPU / driver | None in this environment | `NOT_RUN_HARDWARE` |
| vLLM | Not pinned; no engine commit | `NOT_RUN_HARDWARE` |
| LMCache | Not a dependency | `NOT_RUN_HARDWARE` (route undecided) |
| NIXL / UCX / RDMA | Not a dependency | Not in V1 |
| DOCA / BlueField BFB / firmware | Not present | `NOT_RUN_HARDWARE` |
| RustFS server / client | No endpoint authorized in this environment. Contract mock covers Put, Head, RangeGet, Delete, pack, and manifest | Live probe `NOT_RUN_HARDWARE`. Mock `PASS` |
| TCP / file store | Loopback TCP and a temp directory on the build host | `PASS` for `cargo test --workspace --all-targets` on x86_64. Not an NVMe format and not a DPU run |
| Direct Rust deps | `sha2` 0.10.8; dev `proptest` 1.4.0 | Apache-2.0 OR MIT; see `Cargo.lock`. V1 C adds no direct crate. `gpucache-bench` has an empty dependency list |
| License of this repo | Apache-2.0 | See `LICENSE` |

CI (see `.github/workflows/ci.yml`) builds and runs the same tests on x86_64 and on aarch64 (GitHub ARM runner, plus qemu-user cross execution). An aarch64 result is a CPU result. It does not certify BlueField-3, DOCA, or NVMe-on-DPU.

The integration decision between a native vLLM KV connector and LMCache is pending. No engine commit is pinned because the spike did not run.
