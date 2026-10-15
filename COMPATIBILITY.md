# Compatibility manifest

Recorded for V1 milestones A, B, and C, the V2 first slice, and the V3 CPU slice. This file is not a performance acceptance report and not a hardware inventory of a BlueField node.

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
| NIXL / UCX / RDMA | Not linked. Candidate recorded: `nixl-sys` 1.3.1 (crates.io, updated 2026-07-07; docs.rs also lists 1.5.0), Apache-2.0, repo `ai-dynamo/nixl`. Feature `rdma-nixl` has no crate dependency. `stub-api` aborts on call, so it is not used. Mooncake Transfer Engine is Apache-2.0 and was not linked (in-tree `unsafe` FFI, not a pinned crates.io crate) | Live RDMA `NOT_RUN_HARDWARE`. `NixlTransport::open` returns `rdma not linked` with and without the feature |
| DOCA / BlueField BFB / firmware / GDS | Not present. Feature `dpu-bf3` is empty. Planned mode, BFB, OS, DOCA, firmware, and IOMMU values are unset targets in `docs/V3_DPU_TARGETS.md`. Wording stays in development / `NOT_RUN_HARDWARE` / optional feature | `NOT_RUN_HARDWARE` |
| RustFS server / client | No endpoint authorized in this environment. Contract mock covers Put, Head, RangeGet, Delete, pack, manifest, async sink, and cost-bounded refill | Live probe `NOT_RUN_HARDWARE`. Mock `PASS` |
| TCP / file store | Loopback TCP and a temp directory on the build host | `PASS` for `cargo test --workspace --all-targets` on x86_64. Not an NVMe format and not a DPU run |
| Direct Rust deps | `sha2` 0.10.8; dev `proptest` 1.4.0 | Apache-2.0 OR MIT; see `Cargo.lock`. V2 and V3 add no direct crate. `gpucache-bench` and `gpucache-dpu` add no third-party crate. Optional features `rdma-nixl` and `dpu-bf3` add none |
| SPDK | Not linked. `AffineQueue` is a reactor-id check with no thread | `NOT_RUN_HARDWARE` |
| mTLS | Not linked. Boundary is the trusted fabric in ADR 0007 | `NOT_RUN` (library hooks for ACL, replay, and seals are CPU tests) |
| License of this repo | Apache-2.0 | See `LICENSE` |

CI (see `.github/workflows/ci.yml`) builds and runs the same tests on x86_64 and on aarch64 (GitHub ARM runner, plus qemu-user cross execution). An aarch64 result is a CPU result. It does not certify BlueField-3, DOCA, or NVMe-on-DPU.

The integration decision between a native vLLM KV connector and LMCache is pending. No engine commit is pinned because the spike did not run. SGLang / HiCache is an evaluation note only (ADR 0008) and is not a dependency.

| Path | Default build | Optional feature | Hardware |
| --- | --- | --- | --- |
| TCP + file store | CPU tests | none | CPU `PASS` on the V1/V2 suite. Not an NVMe format |
| `rdma-nixl` | `open` refuses | empty feature, still refuses | `NOT_RUN_HARDWARE` |
| `dpu-bf3` | stubs refuse | empty feature, still refuses | `NOT_RUN_HARDWARE` |
| aarch64 | same Rust core | none | CPU only. Not a BlueField test |
| RustFS | mock | `GPUCACHE_RUSTFS_*` | Live `NOT_RUN_HARDWARE` |
| NIC RDMA vs DPU paired gate | `v3-paired` text | none | `NOT_RUN_HARDWARE`, `TCO_UNKNOWN` |
