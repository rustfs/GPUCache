# ADR 0004: NIXL is the RDMA slot; it is not linked

- Status: **Accepted**
- Date: 2026-10-15
- Milestone: V2 first slice

## Context

V2 needs a transport besides TCP for a future RoCE or InfiniBand path. The execution spec says to reuse NIXL or Mooncake, prefer an existing Rust binding, and not rewrite an RDMA protocol. CI and this development host have no Soft-RoCE device and no `libnixl`. The workspace forbids `unsafe`. TCP is already the correctness backend.

Candidates audited on 2026-10-15:

| Candidate | License | Rust binding | Why it is or is not linked |
| --- | --- | --- | --- |
| NIXL (`ai-dynamo/nixl`) | Apache-2.0 | `nixl-sys` on crates.io. The 1.3.1 publish was updated 2026-07-07. docs.rs also lists 1.5.0. The crate describes a higher-level `nixl` wrapper. | Not linked. `stub-api` compiles without the C++ library and aborts the process if a NIXL function is called. A real build needs `libnixl` (default prefix `/opt/nvidia/nvda_nixl`) and FFI. |
| Mooncake Transfer Engine (`kvcache-ai/Mooncake`) | Apache-2.0 | In-tree `mooncake-transfer-engine/rust` uses bindgen and `unsafe` to link the C++ library. It is not a crates.io crate we can pin without vendoring. | Not linked. Fallback candidate only. |

No CUDA, DOCA, or SPDK crate was considered.

## Decision

The V2 RDMA slot is NIXL, behind the Cargo feature `rdma-nixl` on `gpucache-transport`.

- The feature has an empty dependency list. `Cargo.lock` does not pin `nixl-sys`.
- `NixlTransport::open` returns `CacheError::Storage { reason: "rdma not linked" }` whether or not the feature is enabled.
- `rdma_feature_selected()` is the only behavioral difference. It does not post a work request.
- `TcpTransport` implements `KvTransport` and is the default. `submit` copies bytes into a bounded host buffer. `dma_posted` is false. `staged_bytes` is that copy's length, not NIC wire bytes and not a latency.
- The GCK1 / GCKR client API is unchanged. `TcpClient` remains the cache RPC path.

Live RDMA is `NOT_RUN_HARDWARE`. This ADR does not claim a TTFT, TPOT, or bandwidth number.

## Consequences

- `cargo test --workspace --all-targets` does not fetch NIXL and does not need a NIC.
- `cargo test -p gpucache-transport --all-targets --features rdma-nixl` still refuses `open`.
- A later change may add an optional dependency only after a non-aborting adapter exists and a machine with `libnixl` is authorized. That dependency stays feature-gated and off the default build.
- Mooncake is not a second half-implemented path.
