# V2 data paths

These are the copies this tree actually performs, and the copies it does not. No path is a full zero-copy chain from GPU memory to a NIC to NVMe. GPU, GDS, BlueField, and live RDMA are `NOT_RUN_HARDWARE` / in development. An aarch64 build is not a BlueField test.

`staged_bytes` on a TCP receipt is the length of one host staging copy. It is not NIC wire bytes and not a latency.

## Path 1 — prefill / decode handoff

| Hop | What this tree does | Copy or registration |
| --- | --- | --- |
| Prefill GPU HBM | Not executed | `NOT_RUN_HARDWARE` |
| GPU → host | Not linked. No CUDA copy and no GPUDirect | `NOT_RUN_HARDWARE` |
| Simulated register | `RegisteredPool` binds a `Vec<u8>` slot. A second checkout of the same slot does not allocate a new region id. Unknown completion quarantines the slot. Lease expiry does not free it | Host allocation. Not `ibv_reg_mr` |
| Host staging | `TcpTransport::submit` and `stage_through_tcp` copy the payload into a bounded buffer, then drop it. Over the cap is backpressure (`QuotaExceeded`) and the slots are quarantined | One host copy. `dma_posted=false` |
| TCP RPC | Existing `TcpClient` / `serve` path. Request bytes are still GCK1. The read side uses the V1 buffer pool | Host copy into the socket and the read buffer |
| NIC | Kernel TCP. Feature `rdma-nixl` does not open NIXL | No RDMA work request. `NOT_RUN_HARDWARE` |
| Decode host → GPU | Not executed | `NOT_RUN_HARDWARE` |

A future NIXL path, once `libnixl` is linked on a machine that has it, would add registration, a posted transfer, and a completion event between the two host or GPU buffers. That path is not in this binary. See [adr/0004-nixl-transport.md](adr/0004-nixl-transport.md).

## Path 2 — G3.5 reuse

| Hop | What this tree does | Copy or registration |
| --- | --- | --- |
| Lookup | Metadata only. Hit, whole-block prefix hit, or miss. No payload | No data copy |
| Directory | `ShardDirectory` records one in-flight fill per physical-key checksum. A joiner does not reserve a second credit. The directory does not return bytes | No data copy. No per-token central lookup. No quorum |
| Hit `Get` | TCP returns the committed block | Host copies, same as path 1 |
| Miss, stale epoch, wrong tenant, checksum failure | No bytes. The engine recomputes | No data copy |
| File backend | `FileStore` writes an ordinary file under the directory the operator passed | Host copy plus the kernel page cache. Not an NVMe format |
| Quarantine | Unknown completion holds the simulated slot until drain or a newer fenced epoch | The `Vec` stays allocated and counts against the pool |

Single-node is shard 0 with the same directory methods. See [adr/0005-shard-fencing.md](adr/0005-shard-fencing.md).

## Path 3 — G4 refill

The hot TCP service does not depend on `gpucache-cold`. RustFS is not on the prefill-to-decode path.

| Hop | What this tree does | Copy or registration |
| --- | --- | --- |
| Pack | `pack_blocks` writes one object and one manifest in host memory | Host copy into the pack buffers |
| Async sink | `AsyncSink` queues that pair up to a job and byte cap. `drain` puts the object, then the manifest. `cancel` drops the queue and writes nothing | Host buffers until drain. Not a runtime thread pool |
| Mock store | In-process map. Contract test covers Put, Head, RangeGet, Delete | Not RustFS. Not a measurement |
| Live HTTP | `live_probe` runs only when `GPUCACHE_RUSTFS_*` is set. Otherwise `NOT_RUN_HARDWARE` | Full HTTP body copy. Not GDS |
| Refill | Head the manifest. If its length exceeds the budget, return `OverBudget` and do not download it. Otherwise download the manifest, and RangeGet one generation only when that range fits. Checksum mismatch is an error, not a hit | Host copy of the manifest and, when allowed, one payload range |
| Promote into G3.5 | A later hot `Put` through the TCP API | A separate host copy. Not chained to the GET |

GDS and BlueField storage DMA are in development and were not run.

## What a caller must do on failure

Miss, timeout, checksum failure, over-budget refill, fenced epoch, and transport error are recomputes. None of them is a successful hit. A wrong-tenant or wrong-layout lookup stays a miss.
