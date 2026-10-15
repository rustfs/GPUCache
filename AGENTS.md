# Agent notes

GPUCache is the G3.5 shared KV tier. RustFS is the colder G4 backend. Read [README.md](README.md), [docs/adr/0001-integration-route.md](docs/adr/0001-integration-route.md), and [docs/adr/0002-no-erasure-coding.md](docs/adr/0002-no-erasure-coding.md) before changing scope.

## Phase

The active phase is **V1 milestone C**. Do not implement V2 RDMA or V3 DPU work in a V1 C change. Do not publish crates, cut releases, deploy, or edit a website unless asked.

## Honesty rules

- Do not claim DPU, DOCA, RDMA, SPDK, or CUDA acceleration. V1 is CPU only. An aarch64 build is not a BlueField test.
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
| `gpucache-transport` | the three crates above. TCP only |
| `gpucache-cold` | `gpucache-core-types` and `sha2`. Not a dependency of storage or transport |
| `gpucache-bench` | nothing. Prints `NOT_RUN_HARDWARE` baseline rows and must not invent timings |

No crate may add CUDA, DOCA, NIXL, RDMA, SPDK, erasure coding, or replica placement. `unsafe` is forbidden. Registered buffers whose completion is unknown stay quarantined until drain or a newer epoch; do not free them only because a lease timer fired. The cold crate stays off the hot path.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

`cargo test --workspace --all-targets` is the one-command CPU repro.
