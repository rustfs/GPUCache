# ADR 0009: What to take from Mooncake, and what to leave

- Status: **Accepted**
- Date: 2026-10-16
- Updated: 2026-10-17
- Milestone: V3

## Context

Mooncake (kvcache-ai/Mooncake; the FAST 2025 paper "Mooncake: A KVCache-centric Disaggregated Architecture for LLM Serving", arXiv:2407.00079) is a public KV-cache transfer stack used beside LLM serving. Its Transfer Engine registers DRAM or VRAM buffers, opens a remote segment, submits a batch, and polls `getTransferStatus`. RDMA paths prefer a NIC that is local to the buffer's NUMA or GPU, and they can fall back to another NIC or to TCP. Failed endpoints stay unreclaimed until outstanding slices drain. The store keeps paged KV blocks keyed by a hash of the block and its prefix, evicts with LRU, and places extra copies of hot blocks to spread load.

GPUCache is a different product shape. The hot tier is recomputable G3.5 state. RustFS is the cold tier. [0002-no-erasure-coding.md](0002-no-erasure-coding.md) forbids hot-tier replicas and parity. [0004-nixl-transport.md](0004-nixl-transport.md) already refused to link Mooncake: the in-tree Rust binding uses bindgen and `unsafe`, and this workspace forbids both. [0005-shard-fencing.md](0005-shard-fencing.md) keeps the directory shard-local. [0003-partial-prefix.md](0003-partial-prefix.md) refuses to slice a stored block.

This environment has no GPU, no NIC, and no `libnixl`. A Mooncake throughput figure from the paper or from Transfer Engine's own bench is not a GPUCache result.

## Decision

Adopt four CPU-side contracts that match Mooncake's failure and registration behavior, without linking Mooncake and without claiming a device path:

1. **Registration reuse.** `RegisteredPool::checkout_batch` prefers a slot that already has a binding. A batch that cannot be fully satisfied, including region-id exhaustion, leaves the pool unchanged.
2. **Explicit transport fallback.** `submit_preferring_rdma` tries the NIXL slot, which still refuses to open. The receipt then says the attempt was RDMA, the path that ran was TCP, and the reason is `RdmaNotLinked`. `dma_posted` stays false. TCP is not relabeled as RDMA and is not a DPU baseline.
3. **Fill notices.** A leader finish, a leader failure, and a membership fence are different notices. None of them carries payload bytes. `LeaderFinished` is not a hit. A fence is not success. The notice table is bounded; a dropped notice is `UnknownGeneration`, which means recompute. An epoch that cannot advance does not drop in-flight fills and does not publish the membership change. Domain isolation preflights the full victim count, so insufficient epoch budget cannot partially remove a domain.
4. **Bounded cold retry and identity-bound refill.** A drain round attempts each selected job once. Temporary failures are retried at the tail for at most three attempts; permanent failures and exhausted retries enter an individually retrievable failed queue. Both queues share byte and metadata limits. Content-addressed pack keys prevent an older retry from overwriting a later publication. Ambiguous PUT failures preserve the object instead of deleting bytes that a visible manifest may reference. Manifest v3 binds each range to the full expected `CacheIdentity`, which every public refill API requires before downloading its payload.

Do not adopt:

- Hot-block replicas or `change_replica`. An extra copy would be a latency hint, and it is still not in this tree. It is not a durability promise.
- A central metadata service on the miss path. No etcd, Redis, or HTTP segment directory was added.
- Linking `mooncake-transfer-engine`, NIXL, DOCA, or a NIC.
- Slicing one KV block across transfers. A prefix hit remains a whole stored block.
- Any Mooncake, Soft-RoCE, NIC, or DPU timing. The bench reports name those rows `NOT_LINKED`, `NOT_REPRODUCED`, or `NOT_RUN_HARDWARE`.

## Consequences

- `cargo test --workspace --all-targets` still needs no NIC and no Mooncake library.
- Feature `rdma-nixl` still does not link libnixl. `NixlTransport::open` still returns `rdma not linked`.
- Callers that only watch a fill disappear must read `FillNotice`. Disappearance after `Fenced` or `LeaderFailed` is a miss.
- Manifest version is 3 because each range records its tenant and a digest of the validated canonical `CacheIdentity`. Versions 1 and 2 are `Missing`, with no payload download; recompute and repack instead of relabeling an old manifest.
- `refill_generation` and `refill_matching` require `expected: &CacheIdentity`; `refill_degraded` and `refill_for_tenant` forward that same required identity. Migrate every caller, including administrative helpers. No tenant-free public shortcut remains.
- Identity comparison assumes an authenticated and authorized upstream caller and trusted producer. This crate adds neither an authenticator nor proof that KV bytes equal model execution.
- `drain_report` exposes attempted/completed/requeued/failed counts and the first job error. The original `drain` signature remains, but a returned error can follow successful writes in the same round. Cancelled sinks reject drain at the outer `Result` level.
- Failed jobs retain admission credit until `take_failed` transfers them out; operators must handle that queue. `cancel` discards all pending and failed jobs and permanently closes the sink.
- Failed publications may leave orphaned objects. Explicit later reclamation must establish ownership and absence of live references; the sink does not delete them speculatively after an ambiguous PUT error.
- See [../MOONCAKE.md](../MOONCAKE.md) for the comparison this decision is based on.
