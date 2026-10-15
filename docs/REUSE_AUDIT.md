# Reuse audit (V1 A)

The execution spec asks for a look at LMCache, Mooncake, Dynamo KVBM, NIXL, OpenLake, ContextStore, and Fluxon before adding dependencies. V1 A adds none of them.

| Candidate | Why it is not in the tree |
| --- | --- |
| LMCache | Plausible vLLM bridge. Route not selected; GPU spike `NOT_RUN_HARDWARE`. |
| Mooncake | RDMA / KV transport. Out of scope until V2, and only after V1 correctness. |
| Dynamo KVBM | Another KV manager. Not required to define the CPU contract. |
| NIXL | Candidate V2 transport. No Rust binding is vendored. Ordinary NIC RDMA is not a DPU result. |
| OpenLake, ContextStore, Fluxon | Not required for identity, protocol, or `MemoryStore`. No version is pinned because no code is imported. |

This is a scope decision, not a line-by-line license audit of those upstream trees. Nothing from them was copied. Direct dependencies are only `sha2` (runtime) and `proptest` (tests). V1 C did not add a crate. See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
