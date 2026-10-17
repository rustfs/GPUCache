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
- **Fencing.** Leaders and joiners can read `Pending`, `LeaderFinished`, `LeaderFailed`, or `Fenced`. The notice has no bytes. `LeaderFinished` still requires a store `Get` or a recompute. A fenced fill is not success. A single fence refuses epoch overflow. `ControlPlane::isolate_domain` also checks the epoch budget for every victim before removing any member; an insufficient budget preserves the entire membership change and its current fills.
- **Cold path.** Publication uses content-addressed object and manifest keys. The sink validates their pairing before admission, bounds retries, and isolates terminal failures without blocking unrelated work. An ambiguous PUT error never triggers deletion of an object that a visible manifest could reference. Manifest version 3 binds each range to the complete expected `CacheIdentity`; every public refill path requires that identity before it can return payload bytes. A slow or down cold tier is still `Missing` and does not touch the store.
- **Bench labels.** `paired-baseline` prints `mooncake_transfer_engine=NOT_LINKED` and `mooncake_published_numbers=NOT_REPRODUCED`. `v3-paired` prints `mooncake_rdma=NOT_A_PERFORMANCE_RESULT`. Neither report contains a digit.

## Cold failure contract and migration

`AsyncSink::drain_report` attempts at most `min(max_jobs, queued_at_call_start)` jobs. A job is attempted once per call. Temporary storage, timeout, and quota errors move it to the queue tail until `MAX_WRITE_ATTEMPTS` (3) is reached. Permanent errors and exhausted retries enter the failed queue. `DrainReport` separates attempted, completed, requeued, and failed counts and retains the first job error. The compatible `drain` method processes that same round and then returns the first error, if any; an error does not undo jobs that completed in that round.

Pending and failed jobs share the configured job and byte limits. `MAX_SINK_JOBS` (4096) is the metadata ceiling. `failed_count` and `take_failed` let an operator retrieve an individual failed job and release its retained credit. A full failed queue can still cause admission backpressure until its jobs are taken. `cancel` explicitly clears both queues and closes the sink. It is not a per-job recovery operation.

Object keys are `namespace/<object-digest>/object`; manifest keys are `namespace/<manifest-digest>/manifest`. Each digest covers its own complete bytes. Retries retain those keys, so an older pack cannot overwrite a newer pack under the same namespace. A failed publication can leave an unreferenced object. `AsyncSink` does not implement reference-safe orphan reclamation. A later explicit cleanup must establish ownership and absence of live manifest references; a timeout, an unsuccessful PUT response, or one HEAD result is not permission to delete an object.

Manifest v3 stores the tenant and the digest of the validated canonical `CacheIdentity` for each range. `refill_generation` and `refill_matching` now require `expected: &CacheIdentity`; the availability wrappers `refill_degraded` and `refill_for_tenant` require it as well. There is no public tenant-free refill path. Versions 1 and 2 yield `Missing` without downloading payload bytes. Existing cold entries must be recomputed and repacked with v3; merely changing the version field cannot migrate them.

The expected identity must come from the caller's authenticated, authorized inference context. This crate does not authenticate a tenant, authorize arbitrary object-store keys, or prove that a producer computed the correct model state. Matching identity digests and payload checksums detects mismatched metadata and bytes within that upstream trust boundary. The same tenant name in another trust domain, model revision, adapter, prefix, or layout is not interchangeable.

The required regression coverage is listed in [V2_TEST_MATRIX.md](V2_TEST_MATRIX.md). A Grok bot or VM operator should use the existing [VM runner](VM_REGRESSION.md#pr-8-cold-and-fencing-regressions), retain its exit code and artifacts, and preserve all `NOT_RUN_HARDWARE` labels.

## What stays out

No Mooncake crate, no etcd client, no replica placement, no intra-block slice, and no Soft-RoCE timing. Live RDMA remains `NOT_RUN_HARDWARE`.
