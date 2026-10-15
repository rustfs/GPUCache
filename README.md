# GPUCache

GPUCache is an open shared KV-cache tier for disaggregated inference. In NVIDIA CMX terminology this tier is **G3.5**: a shared cache in front of GPU HBM, reused across requests that have the same tenant, model, and layout. [RustFS](https://github.com/rustfs/rustfs) is the colder **G4** capacity backend. G4 is not on the prefill-to-decode hot path.

A KV block is recomputable accelerator state. It is not the user's only copy of a prompt, and it is not model-agnostic long-term memory. A miss, timeout, checksum failure, version mismatch, or store outage must fall back to the inference engine and recompute. A wrong hit is a failure.

## Status

**V1 milestone A is implemented. DPU acceleration is not.**

V1 runs the cache contract on CPU. The long-term deployment target is a storage-side BlueField-3, but this repository does not link DOCA, CUDA, RDMA, or SPDK, and it does not claim that any of those paths exist. An aarch64 build is a CPU build. It is not a BlueField functional test.

| Milestone | Status |
| --- | --- |
| V1 A: identity, protocol, `MemoryStore`, error/timeout/cancel tests | Implemented in this tree |
| V1 A: real vLLM KV block export/import | `NOT_RUN_HARDWARE` (no GPU in this environment) |
| V1 B: TCP service, file/NVMe, admission, RustFS smoke | Not started |
| V1 C: fuzz, fault injection, engine output comparison, paired baselines | Not started |
| V2: NIC RDMA (NIXL or Mooncake) plus a real RustFS cold path | Not started; blocked on V1 |
| V3: BlueField-3 offload and production hardening | Not started; not purchased and not measured |

The integration route (native vLLM KV connector or LMCache) is **not chosen**. See [docs/adr/0001-integration-route.md](docs/adr/0001-integration-route.md).

## What V1 A does

Three library crates, none of which depend on a GPU or DPU:

- `gpucache-core-types` — versioned `CacheIdentity`, length-delimited physical and logical keys, the block lifecycle, checksums, quotas, and errors.
- `gpucache-protocol` — tenant auth context, batch messages, deadlines, and wire-version negotiation.
- `gpucache-storage` — in-process `MemoryStore`.

The batch API is Lookup, Reserve/Put, Commit, Get, Release/Cancel, and Invalidate. Every item carries `request_id`, epoch, deadline, and, where it applies, generation, length, and checksum. Only a `COMMITTED` block whose identity is compatible is a hit. `ABSENT`, `RESERVED`, `WRITING`, `EVICTING`, and `QUARANTINED` blocks are not readable.

Lifecycle:

```text
ABSENT -> RESERVED -> WRITING -> COMMITTED -> EVICTING -> ABSENT
```

A failed or timed-out registered region goes to `QUARANTINED`. Lease expiry does not prove that a copy has stopped, so that buffer stays allocated until the owner attests drain or a newer store epoch fences it. Same tokens do not imply the same bytes: the logical prefix key and the physical byte key are different encodings. V1 reuses only within one tenant, one model, and one layout. A reshard, quant, or layout mismatch is a miss. There is no cross-tenant dedup and no partial-prefix hit in this milestone.

Resource limits cover object size, in-flight requests, memory registrations, per-tenant in-flight bytes, and retained bytes. The idempotency table is bounded.

## What you can run

From a clean checkout, with Rust stable (MSRV 1.85):

```bash
cargo test --workspace --all-targets
```

That command is the CPU contract repro. It includes unit tests, property tests, timeout and cancel tests, a single-process import of a synthetic block, and a cross-process import through `kv-block-xfer`. The bytes in those tests are synthetic. They are not a vLLM KV tensor.

`kv-block-xfer import <credential-hex>` reads one export frame on stdin and imports it into a fresh `MemoryStore`. It does not talk to an engine.

## Layout

```text
crates/core-types    identity, keys, lifecycle, errors
crates/protocol      batch messages and version negotiation
crates/storage       MemoryStore and kv-block-xfer
docs/adr             integration-route decision record
COMPATIBILITY.md     environment manifest
```

Design notes: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PROTOCOL.md](docs/PROTOCOL.md), [docs/SECURITY.md](docs/SECURITY.md), [docs/OPERATIONS.md](docs/OPERATIONS.md), [docs/BENCHMARKS.md](docs/BENCHMARKS.md), [docs/RISKS.md](docs/RISKS.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
