# Mooncake reverse notes

This note compares public Mooncake behavior with GPUCache and records what this tree changed. It is not a port of Mooncake, and it is not a performance report. Sources used on 2026-10-16:

- Mooncake Transfer Engine design: <https://kvcache-ai.github.io/Mooncake/design/transfer-engine.html> (Segment, BatchTransfer, topology-aware path selection, endpoint pooling, TCP when `--protocol=tcp`).
- FAST 2025 paper, arXiv:2407.00079, "Mooncake: A KVCache-centric Disaggregated Architecture for LLM Serving" (paged KV blocks, prefix hashes, LRU, hot-key replicas, Transfer Engine, `getTransferStatus`).

No number from those sources is reproduced here. GPUCache did not run Mooncake, Soft-RoCE, RDMA, or a GPU for this change. The decision record is [adr/0009-mooncake-lessons.md](adr/0009-mooncake-lessons.md).

## What Mooncake does well

Mooncake separates KV reuse from the compute engine. Prefill and decode can sit on different machines. KV blocks live in a pool outside GPU HBM, and a miss still falls back to compute.

The Transfer Engine treats registration as a long-lived property of a buffer, not as something to repeat on every slice. A batch names many non-contiguous ranges. Completion is a status the caller polls, so a failure is visible instead of looking like a successful transfer. When a NIC or a queue is unhealthy, the engine tries another path, and TCP is an explicit protocol rather than a silent relabel of RDMA. Endpoints that have been evicted stay alive until their outstanding work drains.

The store key mixes a block hash with its prefix, so layout-sensitive bytes are not treated as interchangeable. Eviction skips a block that a request is still using.

## Where that diverges from GPUCache

| Mooncake | GPUCache constraint |
| --- | --- |
| Extra copies of hot blocks to balance NIC load | Hot-tier replicas and parity are rejected. A lost block is a clean miss, then recompute. Cold durability is RustFS. |
| Central segment metadata (etcd, Redis, or HTTP) | The directory is shard-local. There is no per-token index and no quorum on the miss path. |
| In-tree C++ Transfer Engine, with a bindgen Rust wrapper | `unsafe` is forbidden. NIXL is the named RDMA slot and is not linked. Mooncake is not linked either. |
| Transfer slices can use several NICs | A prefix hit is the whole stored block. The service does not slice inside a block. |
| Published transfer and serving measurements | This tree has no GPU and no NIC. Those measurements are not GPUCache results. TCP is not the DPU baseline. |

## What this tree changed

These are CPU contracts. They do not post a work request.

- **Pool.** Checkout uses an already-bound slot before it binds a free one. If the batch cannot be completed, including when region ids are exhausted, no slot changes state.
- **Fallback.** `submit_preferring_rdma` records an RDMA attempt and a TCP result with reason `RdmaNotLinked`. `dma_posted` is false. `hardware` on that path is `CpuContract`.
- **Fencing.** Leaders and joiners can read `Pending`, `LeaderFinished`, `LeaderFailed`, or `Fenced`. The notice has no bytes. `LeaderFinished` still requires a store `Get` or a recompute. A fenced fill is not success. If the epoch is already `u64::MAX`, the fence is refused and the in-flight fill stays.
- **Cold path.** `drain` puts a failed job back. A manifest is not published for an object the failed drain deleted. Manifest version 2 stores the tenant on each range. `refill_matching` returns `Unauthorized` for a different tenant and does not download that payload. A slow or down cold tier is still `Missing` and does not touch the store.
- **Bench labels.** `paired-baseline` prints `mooncake_transfer_engine=NOT_LINKED` and `mooncake_published_numbers=NOT_REPRODUCED`. `v3-paired` prints `mooncake_rdma=NOT_A_PERFORMANCE_RESULT`. Neither report contains a digit.

## What stays out

No Mooncake crate, no etcd client, no replica placement, no intra-block slice, and no Soft-RoCE timing. Live RDMA remains `NOT_RUN_HARDWARE`.
