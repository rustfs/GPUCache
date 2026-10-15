# Agent notes

GPUCache is the G3.5 shared KV tier. RustFS is the colder G4 backend. Read [README.md](README.md), [docs/adr/0001-integration-route.md](docs/adr/0001-integration-route.md), and [docs/adr/0002-no-erasure-coding.md](docs/adr/0002-no-erasure-coding.md) before changing scope.

## Phase

The active phase is **V3** (explicitly authorized). V1 A/B/C and the V2 first slice stay in tree and must keep passing. Do not start work past V3. Do not purchase hardware. Do not publish crates, cut releases, deploy, or edit a website unless asked.

## Honesty rules

- Do not claim DPU, DOCA, RDMA, SPDK, GDS, or CUDA acceleration. The default build is CPU and TCP. Feature `rdma-nixl` does not link a device. Feature `dpu-bf3` does not link DOCA, SPDK, or an NVMe device. An aarch64 build is not a BlueField test. Public wording for BlueField-3, GDS, and DPU stays in development / `NOT_RUN_HARDWARE` / optional feature.
- Do not invent vLLM, LMCache, NIXL, TTFT, or TCO results. A real engine KV export/import needs a GPU and stays `NOT_RUN_HARDWARE` until that run exists. Cost stays `TCO_UNKNOWN` until a paired NIC-RDMA vs DPU run exists. TCP is not the DPU win baseline.
- Do not treat a KV block as model-agnostic long-term memory. A miss falls back to recompute. A wrong-tenant or wrong-layout hit is a defect.
- Do not add erasure coding, parity, or durability replicas. A lost or failed hot block is a clean miss, then recompute and repopulate. Cold durability belongs to RustFS. An extra hot-prefix copy, if it is ever added, is a latency hint and not a durability promise.
- Do not call the tree PB-scale verified or production-ready. Arm process start is not a completed offload. Host GPUNetIO NIC-mode samples are not a DPU Arm storage daemon.
- Docs in this repo are English.

## Crates

| Crate | May depend on |
| --- | --- |
| `gpucache-core-types` | `sha2` only |
| `gpucache-protocol` | `gpucache-core-types` |
| `gpucache-storage` | the two crates above |
| `gpucache-transport` | the three crates above. TCP correctness path. Optional feature `rdma-nixl` must add no crate |
| `gpucache-cold` | `gpucache-core-types` and `sha2`. Not a dependency of storage or transport |
| `gpucache-dpu` | `gpucache-core-types` only. Optional feature `dpu-bf3` must add no crate. Not a dependency of storage, transport, protocol, or cold |
| `gpucache-bench` | nothing. Prints `NOT_RUN_HARDWARE` baseline rows and must not invent timings |

No crate may add a CUDA, DOCA, NIXL, RDMA, SPDK, erasure-coding, or replica-placement dependency. The feature names `rdma-nixl` and `dpu-bf3` are slots only. `unsafe` is forbidden. Registered buffers whose completion is unknown stay quarantined until drain or a newer epoch; do not free them only because a lease timer fired. The cold crate stays off the hot path. The DPU crate stays off the hot path until a measured offload exists.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

`cargo test --workspace --all-targets` is the one-command CPU repro. It must pass with default features and without DOCA or libnixl.
