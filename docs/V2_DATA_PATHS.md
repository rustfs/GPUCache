# V2 data paths

These are the copies this tree actually performs, and the copies it does not. No path is a full zero-copy chain from GPU memory to a NIC to NVMe. GPU, GDS, BlueField, and live RDMA are `NOT_RUN_HARDWARE` / in development. An aarch64 build is not a BlueField test.

`staged_bytes` on a TCP receipt is the length of one host staging copy. It is not NIC wire bytes and not a latency.

## Path 1 — prefill / decode handoff

| Hop | What this tree does | Copy or registration |
| --- | --- | --- |
| Prefill GPU HBM | Not executed | `NOT_RUN_HARDWARE` |
| GPU → host | Not linked. No CUDA copy and no GPUDirect | `NOT_RUN_HARDWARE` |
| Simulated register | `RegisteredPool` binds a `Vec<u8>` slot and prefers one that is already bound. A batch is all or nothing. Unknown completion quarantines the slot. Lease expiry does not free it | Host allocation. Not `ibv_reg_mr` |
| Host staging | `TcpTransport::submit` and `stage_through_tcp` copy the payload into a bounded buffer, then drop it. Over the cap is backpressure (`QuotaExceeded`) and the slots are quarantined | One host copy. `dma_posted=false` |
| TCP RPC | Existing `TcpClient` / `serve` path. Request bytes are still GCK1. The read side uses the V1 buffer pool | Host copy into the socket and the read buffer |
| NIC | Kernel TCP. `submit_preferring_rdma` records an RDMA attempt that falls back to this TCP copy with reason `RdmaNotLinked`. Feature `rdma-nixl` does not open NIXL | No RDMA work request. `dma_posted=false`. `NOT_RUN_HARDWARE` |
| Decode host → GPU | Not executed | `NOT_RUN_HARDWARE` |

A future NIXL path, once `libnixl` is linked on a machine that has it, would add registration, a posted transfer, and a completion event between the two host or GPU buffers. That path is not in this binary. See [adr/0004-nixl-transport.md](adr/0004-nixl-transport.md).

## Path 2 — G3.5 reuse

| Hop | What this tree does | Copy or registration |
| --- | --- | --- |
| Lookup | Metadata only. Hit, whole-block prefix hit, or miss. No payload | No data copy |
| Directory | `ShardDirectory` records one in-flight fill per physical-key checksum. A joiner does not reserve a second credit. `FillNotice` is pending, finished, failed, or fenced, and it does not return bytes. `LeaderFinished` is not a hit | No data copy. No per-token central lookup. No quorum |
| Hit `Get` | TCP returns the committed block | Host copies, same as path 1 |
| Miss, stale epoch, wrong tenant, checksum failure | No bytes. The engine recomputes | No data copy |
| File backend | `FileStore` writes an ordinary file under the directory the operator passed | Host copy plus the kernel page cache. Not an NVMe format |
| Quarantine | Unknown completion holds the simulated slot until drain or a newer fenced epoch | The `Vec` stays allocated and counts against the pool |

Single-node is shard 0 with the same directory methods. See [adr/0005-shard-fencing.md](adr/0005-shard-fencing.md).

## Path 3 — G4 refill

The hot TCP service does not depend on `gpucache-cold`. RustFS is not on the prefill-to-decode path.

| Hop | What this tree does | Copy or registration |
| --- | --- | --- |
| Pack | `pack_blocks` writes an object and a v3 manifest in host memory. Each range carries its tenant and a digest of its validated canonical `CacheIdentity`. Object and manifest keys contain their respective content digests | Host copy into the pack buffers |
| Async sink | Admission validates the complete pack/key pairing. `drain_report` puts the object, then the manifest; temporary failures retry at the tail up to three attempts, and permanent/exhausted failures enter the failed queue. Pending and failed jobs share the job and byte caps. Ambiguous PUT errors preserve the object. `take_failed` releases an individual failed job; `cancel` clears both queues and closes the sink | Host buffers remain charged until completion, failed-job retrieval, or explicit cancellation. Not a runtime thread pool |
| Mock store | In-process map. Contract test covers Put, Head, RangeGet, Delete | Not RustFS. Not a measurement |
| Live HTTP | `live_probe` runs only when `GPUCACHE_RUSTFS_*` is set. Otherwise `NOT_RUN_HARDWARE` | Full HTTP body copy. Not GDS |
| Refill | Every public refill API requires the full expected `CacheIdentity`. Head the manifest and enforce the download budget. Versions 1 and 2 return `Missing` without fetching payload bytes. For v3, validate the manifest, tenant and canonical identity digest before requesting a payload range. Download only when manifest plus payload fit the total byte budget, then verify payload length and checksum | Host copy of the manifest and, when allowed, one payload range |
| Promote into G3.5 | A later hot `Put` through the TCP API | A separate host copy. Not chained to the GET |

`drain_report` visits at most the queue length captured at call start, bounded further by `max_jobs`; a requeued task cannot be attempted repeatedly in that call. The report preserves counts and the first job error. The compatible `drain` call returns that first error after the round, even if other jobs completed. Pending and failed jobs together cannot exceed the configured limits or the 4096-job hard ceiling. A full failed queue causes backpressure until the operator retrieves its jobs.

Retries use the original content-addressed keys. A failed publication may leave an object without a manifest. Retain its keys for explicit later cleanup after ownership and reference checks. The sink implements neither a reference-safe orphan collector nor a crash-persistent orphan inventory, and does not infer safe deletion from a PUT failure or a HEAD result. It cannot provide a cross-object storage transaction.

Callers migrate to `refill_generation(..., expected)` or `refill_matching(..., expected)` and pass that same identity through `refill_degraded` or `refill_for_tenant`. Old v1/v2 entries require recompute and repack; they are not converted by changing a header. The expected identity comes from the upstream authenticated and authorized request. The cold crate does not establish that identity, authorize arbitrary store keys, or verify the producer's model computation.

`RefillBudget` accounts for manifest and payload bodies requested through the refill interface. It is not a backend wire-traffic or RSS bound: the current live HTTP probe also fetches an object body during HEAD to derive its checksum. Mock budget tests do not qualify that backend behavior.

GDS and BlueField storage DMA are in development and were not run.

## What a caller must do on failure

Miss, timeout, checksum failure, over-budget refill, fenced epoch, and transport error are recomputes. None of them is a successful hit. A wrong-tenant or wrong-layout lookup stays a miss.
