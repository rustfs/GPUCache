# GPUCache

GPUCache is an open shared KV-cache tier for disaggregated inference. In NVIDIA CMX terminology this tier is **G3.5**: a shared cache in front of GPU HBM, reused across requests that have the same tenant, model, and layout. [RustFS](https://github.com/rustfs/rustfs) is the colder **G4** capacity backend. G4 is not on the prefill-to-decode hot path.

A KV block is recomputable accelerator state. It is not the user's only copy of a prompt, and it is not model-agnostic long-term memory. A miss, timeout, checksum failure, version mismatch, or store outage must fall back to the inference engine and recompute. A wrong hit is a failure.

GPUCache does not do erasure coding and does not keep durability replicas. A lost or failed hot block is a clean miss: the engine recomputes and may repopulate the cache. Durability for cold data is RustFS G4's job. An extra copy of a hot shared prefix may be added later as a latency hint; that copy is not erasure coding and it is not a durability promise. See [docs/adr/0002-no-erasure-coding.md](docs/adr/0002-no-erasure-coding.md).

## Status

**V1 milestones A, B, and C are implemented. DPU acceleration is not.**

V1 runs the cache contract on CPU. The long-term deployment target is a storage-side BlueField-3, but this repository does not link DOCA, CUDA, RDMA, or SPDK, and it does not claim that any of those paths exist. An aarch64 build is a CPU build. It is not a BlueField functional test.

| Milestone | Status |
| --- | --- |
| V1 A: identity, protocol, `MemoryStore`, error/timeout/cancel tests | Implemented |
| V1 A: real vLLM KV block export/import | `NOT_RUN_HARDWARE` (no GPU in this environment) |
| V1 B: TCP service, file backend, LRU admission, tenant quotas, two-client reuse | Implemented in this tree |
| V1 B: RustFS live Put/Head/RangeGet/Delete | `NOT_RUN_HARDWARE` (no authorized endpoint). Contract mock passes |
| V1 C: fuzz, fault injection, synthetic token checks, paired-baseline stubs | Implemented in this tree. Real engine output is `NOT_RUN_HARDWARE` |
| V2: NIC RDMA (NIXL or Mooncake) plus a real RustFS cold path | Not started; blocked on V1 |
| V3: BlueField-3 data-path offload and production hardening | Not started; not purchased and not measured. Not an erasure-coding milestone |
| Erasure coding and durability replicas | Will not be built. A lost or failed hot block is a clean miss, then recompute and repopulate. Cold durability is RustFS G4 |

The integration route (native vLLM KV connector or LMCache) is **not chosen**. See [docs/adr/0001-integration-route.md](docs/adr/0001-integration-route.md). Hot-tier failure policy is [docs/adr/0002-no-erasure-coding.md](docs/adr/0002-no-erasure-coding.md).

## What V1 A does

Three library crates, none of which depend on a GPU or DPU:

- `gpucache-core-types` — versioned `CacheIdentity`, length-delimited physical and logical keys, the block lifecycle, checksums, quotas, and errors.
- `gpucache-protocol` — tenant auth context, batch messages, deadlines, and wire-version negotiation.
- `gpucache-storage` — in-process `MemoryStore`.

The batch API is Lookup, Reserve/Put, Commit, Get, Release/Cancel, and Invalidate. Every item carries `request_id`, epoch, deadline, and, where it applies, generation, length, and checksum. Only a `COMMITTED` block whose identity is compatible is a hit. `ABSENT`, `RESERVED`, `WRITING`, `EVICTING`, and `QUARANTINED` blocks are not readable.

## What V1 B adds

`gpucache-transport` serves that same API over TCP. The default bind is `127.0.0.1`. The process refuses to listen on a non-loopback address unless `--allow-remote` is set, and it still requires a tenant credential fingerprint. There is no unauthenticated data path.

`FileStore` keeps committed blocks as ordinary files under a directory you pass in. It does not format a disk and it does not open a device node. When a tenant or global byte quota would fail, it evicts the oldest unpinned committed block. A pinned or quarantined block stays. `MemoryStore` still fails admission instead of evicting, so the V1 A tests keep that behavior.

Lookup can return `PrefixHit` when a committed block's hash chain is a strict prefix of the query and the layout, dtype, shard, and model identity match. The hit is the whole stored block. `Get` must use `identity_for_prefix_hit`. A shorter `valid_length` on the same chain is an incompatible miss. The service does not slice inside a block.

`gpucache-cold` packs blocks into an object plus a manifest and can Put, Head, RangeGet, and Delete through a trait. The TCP hot path does not call it. Without `GPUCACHE_RUSTFS_*` variables the live probe is `NOT_RUN_HARDWARE`. The mock still runs.

Lifecycle:

```text
ABSENT -> RESERVED -> WRITING -> COMMITTED -> EVICTING -> ABSENT
```

A failed or timed-out registered region goes to `QUARANTINED`. Lease expiry does not prove that a copy has stopped, so that buffer stays allocated until the owner attests drain or a newer store epoch fences it. Same tokens do not imply the same bytes: the logical prefix key and the physical byte key are different encodings. V1 reuses only within one tenant, one model, and one layout. A reshard, quant, or layout mismatch is a miss. There is no cross-tenant dedup. A longer hash chain can reuse a whole shorter committed block (`PrefixHit`); the service does not slice inside a block. See [docs/adr/0003-partial-prefix.md](docs/adr/0003-partial-prefix.md).

Resource limits cover object size, in-flight requests, memory registrations, per-tenant in-flight bytes, and retained bytes. The idempotency table is bounded.

## What you can run

From a clean checkout, with Rust stable (MSRV 1.85):

```bash
cargo test --workspace --all-targets
```

That command is the CPU contract repro for V1 A, V1 B, and V1 C. It includes unit tests, property tests, protocol fuzz, timeout and cancel tests, a single-process import of a synthetic block, a cross-process import through `kv-block-xfer`, file-backend quota and restart tests, the TCP reuse tests, and the V1 C fault matrix. The bytes are synthetic. They are not a vLLM KV tensor.

Two localhost clients sharing one file-backed service:

```bash
cargo run -p gpucache-transport --bin share-demo
```

A successful run prints `status=REUSE_OK` and `partial_prefix=WHOLE_BLOCK_PREFIX`. It does not print a sliced-block hit.

Hit, miss, and recompute fallback from a clean temp directory:

```bash
cargo run -p gpucache-transport --bin fault-demo
```

Success is `status=FALLBACK_OK` plus `criteria=EXACT_TOKEN_IDS` and `tolerance=NONE`. The process prints that banner only after the checks pass. A wrong tenant or wrong model is not a hit. This is not a vLLM run.

Paired GPU baselines stay `NOT_RUN_HARDWARE` and print no timings:

```bash
cargo run -p gpucache-bench --bin paired-baseline
```

`kv-block-xfer import <credential-hex>` reads one export frame on stdin and imports it into a fresh `MemoryStore`. It does not talk to an engine.

## Layout

```text
crates/core-types    identity, keys, lifecycle, errors
crates/protocol      batch messages, replies, version negotiation
crates/storage       MemoryStore, FileStore, kv-block-xfer
crates/transport     TCP server, client, share-demo, fault-demo
crates/cold          RustFS contract, pack/manifest, live probe
crates/bench         paired-baseline stub (no timings)
docs/adr             decision records
COMPATIBILITY.md     environment manifest
```

Design notes: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PROTOCOL.md](docs/PROTOCOL.md), [docs/SECURITY.md](docs/SECURITY.md), [docs/OPERATIONS.md](docs/OPERATIONS.md), [docs/BENCHMARKS.md](docs/BENCHMARKS.md), [docs/CORRECTNESS.md](docs/CORRECTNESS.md), [docs/RISKS.md](docs/RISKS.md), [docs/V1C_TEST_MATRIX.md](docs/V1C_TEST_MATRIX.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
