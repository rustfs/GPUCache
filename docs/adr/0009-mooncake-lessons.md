# ADR 0009: What to take from Mooncake, and what to leave

- Status: **Accepted**
- Date: 2026-10-16
- Milestone: V3

## Context

Mooncake (kvcache-ai/Mooncake; the FAST 2025 paper "Mooncake: A KVCache-centric Disaggregated Architecture for LLM Serving", arXiv:2407.00079) is a public KV-cache transfer stack used beside LLM serving. Its Transfer Engine registers DRAM or VRAM buffers, opens a remote segment, submits a batch, and polls `getTransferStatus`. RDMA paths prefer a NIC that is local to the buffer's NUMA or GPU, and they can fall back to another NIC or to TCP. Failed endpoints stay unreclaimed until outstanding slices drain. The store keeps paged KV blocks keyed by a hash of the block and its prefix, evicts with LRU, and places extra copies of hot blocks to spread load.

GPUCache is a different product shape. The hot tier is recomputable G3.5 state. RustFS is the cold tier. [0002-no-erasure-coding.md](0002-no-erasure-coding.md) forbids hot-tier replicas and parity. [0004-nixl-transport.md](0004-nixl-transport.md) already refused to link Mooncake: the in-tree Rust binding uses bindgen and `unsafe`, and this workspace forbids both. [0005-shard-fencing.md](0005-shard-fencing.md) keeps the directory shard-local. [0003-partial-prefix.md](0003-partial-prefix.md) refuses to slice a stored block.

This environment has no GPU, no NIC, and no `libnixl`. A Mooncake throughput figure from the paper or from Transfer Engine's own bench is not a GPUCache result.

## Decision

Adopt four CPU-side contracts that match Mooncake's failure and registration behavior, without linking Mooncake and without claiming a device path:

1. **Registration reuse.** `RegisteredPool::checkout_batch` prefers a slot that already has a binding. A batch that cannot be fully satisfied, including region-id exhaustion, leaves the pool unchanged.
2. **Explicit transport fallback.** `submit_preferring_rdma` tries the NIXL slot, which still refuses to open. The receipt then says the attempt was RDMA, the path that ran was TCP, and the reason is `RdmaNotLinked`. `dma_posted` stays false. TCP is not relabeled as RDMA and is not a DPU baseline.
3. **Fill notices.** A leader finish, a leader failure, and a membership fence are different notices. None of them carries payload bytes. `LeaderFinished` is not a hit. A fence is not success. The notice table is bounded; a dropped notice is `UnknownGeneration`, which means recompute. An epoch that cannot advance does not drop in-flight fills and does not publish the membership change.
4. **Cold retry and tenant refill.** A failed `AsyncSink::drain` puts the job back and does not leave a manifest for an object that was deleted. `refill_matching` rejects another tenant's generation before the payload range is downloaded.

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
- Manifest version is 2 because each range now stores its tenant. A version-1 manifest is not a hit.
- See [../MOONCAKE.md](../MOONCAKE.md) for the comparison this decision is based on.
