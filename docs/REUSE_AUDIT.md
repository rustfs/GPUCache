# Reuse audit

The execution spec asks for a look at LMCache, Mooncake, Dynamo KVBM, NIXL, OpenLake, ContextStore, and Fluxon before adding dependencies. None of them is a crate dependency. The V2 slice names NIXL as the RDMA slot and still does not link it. See [adr/0004-nixl-transport.md](adr/0004-nixl-transport.md).

| Candidate | Why it is not linked |
| --- | --- |
| LMCache | Plausible vLLM bridge. Route not selected; GPU spike `NOT_RUN_HARDWARE`. |
| Mooncake | Apache-2.0. Rust Transfer Engine is in-tree bindgen FFI (`unsafe`) and needs the C++ library at build time. Not a crates.io pin. Recorded as the fallback, not this slice's dependency. |
| Dynamo KVBM | Another KV manager. Not required for the CPU contract or the shard directory. |
| NIXL | Apache-2.0. `nixl-sys` 1.3.1 (crates.io, 2026-07-07; docs.rs also shows 1.5.0) is the chosen slot. `stub-api` aborts if called. Real linkage needs `libnixl` and FFI. Feature `rdma-nixl` compiles with an empty dependency list. Live RDMA is `NOT_RUN_HARDWARE`. Ordinary NIC RDMA is not a DPU result. |
| OpenLake, ContextStore, Fluxon | Not required for identity, protocol, the shard directory, or the cold contract. No version is pinned because no code is imported. |

This is a license-and-binding audit of the transport choice, not a line-by-line audit of those upstream trees. Nothing from them was copied. Direct dependencies are only `sha2` (runtime) and `proptest` (tests). See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
