# Agent notes

GPUCache is the G3.5 shared KV tier. RustFS is the colder G4 backend. Read [README.md](README.md), [docs/adr/0001-integration-route.md](docs/adr/0001-integration-route.md), and [docs/adr/0002-no-erasure-coding.md](docs/adr/0002-no-erasure-coding.md) before changing scope.

## Phase

The active phase is the **V2 first slice** (explicitly authorized). V1 A/B/C stay in tree and must keep passing. Do not start V3 DPU, DOCA, SPDK, or GDS work. Do not publish crates, cut releases, deploy, or edit a website unless asked.

## Honesty rules

- Do not claim DPU, DOCA, RDMA, SPDK, or CUDA acceleration. The default build is CPU and TCP. Feature `rdma-nixl` does not link a device. An aarch64 build is not a BlueField test. GDS and BlueField stay in development / `NOT_RUN_HARDWARE`.
- Do not invent vLLM, LMCache, or NIXL results. A real engine KV export/import needs a GPU and stays `NOT_RUN_HARDWARE` until that run exists.
- Do not treat a KV block as model-agnostic long-term memory. A miss falls back to recompute. A wrong-tenant or wrong-layout hit is a defect.
- Do not add erasure coding, parity, or durability replicas. A lost or failed hot block is a clean miss, then recompute and repopulate. Cold durability belongs to RustFS. An extra hot-prefix copy, if it is ever added, is a latency hint and not a durability promise.
- Docs in this repo are English.

## Crates

| Crate | May depend on |
| --- | --- |
| `gpucache-core-types` | `sha2` only |
| `gpucache-protocol` | `gpucache-core-types` |
| `gpucache-storage` | the two crates above |
| `gpucache-transport` | the three crates above. TCP correctness path. Optional feature `rdma-nixl` must add no crate |
| `gpucache-cold` | `gpucache-core-types` and `sha2`. Not a dependency of storage or transport |
| `gpucache-bench` | nothing. Prints `NOT_RUN_HARDWARE` baseline rows and must not invent timings |

No crate may add a CUDA, DOCA, NIXL, RDMA, SPDK, erasure-coding, or replica-placement dependency. The feature name `rdma-nixl` is a slot only. `unsafe` is forbidden. Registered buffers whose completion is unknown stay quarantined until drain or a newer epoch; do not free them only because a lease timer fired. The cold crate stays off the hot path.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

`cargo test --workspace --all-targets` is the one-command CPU repro.
