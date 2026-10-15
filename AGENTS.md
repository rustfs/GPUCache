# Agent notes

GPUCache is the G3.5 shared KV tier. RustFS is the colder G4 backend. Read [README.md](README.md) and [docs/adr/0001-integration-route.md](docs/adr/0001-integration-route.md) before changing scope.

## Phase

The active phase is **V1 milestone A** unless a human explicitly starts a later milestone. Do not implement V1 B, V2 RDMA, or V3 DPU work in a V1 A change. Do not publish crates, cut releases, deploy, or edit a website unless asked.

## Honesty rules

- Do not claim DPU, DOCA, RDMA, SPDK, or CUDA acceleration. V1 is CPU only. An aarch64 build is not a BlueField test.
- Do not invent vLLM, LMCache, or NIXL results. A real engine KV export/import needs a GPU and stays `NOT_RUN_HARDWARE` until that run exists.
- Do not treat a KV block as model-agnostic long-term memory. A miss falls back to recompute. A wrong-tenant or wrong-layout hit is a defect.
- Docs in this repo are English.

## Crates

| Crate | May depend on |
| --- | --- |
| `gpucache-core-types` | `sha2` only |
| `gpucache-protocol` | `gpucache-core-types` |
| `gpucache-storage` | the two crates above |

No crate may add CUDA, DOCA, NIXL, RDMA, or SPDK. `unsafe` is forbidden. Registered buffers whose completion is unknown stay quarantined until drain or a newer epoch; do not free them only because a lease timer fired.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

`cargo test --workspace --all-targets` is the one-command CPU repro.
