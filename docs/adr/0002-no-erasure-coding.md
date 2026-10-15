# ADR 0002: The hot tier does not use erasure coding or replicas

- Status: **Accepted**
- Date: 2026-10-15
- Milestone: V1 A

## Context

GPUCache is a shared KV-cache tier (G3.5). A block is recomputable accelerator state. RustFS is the colder G4 capacity backend, and durability for cold data belongs there.

Hardware erasure coding on a DPU was considered because other storage systems use it. It is not part of this cache. Parity and replica maintenance would sit on the hit path of data the inference engine can rebuild. That is the wrong cost for a miss-tolerant tier.

## Decision

GPUCache does not implement erasure coding. It does not implement durability replicas.

If a hot block is lost, corrupt, evicted, or fails a check, the result is a clean miss. The engine recomputes the block and may populate the cache again. There is no rebuild from parity.

An extra copy of a hot shared prefix may be considered later as a latency optimization. That copy is not erasure coding, it is not a durability promise, and it is not implemented in V1 A.

DOCA EC, SPDK RAID targets, and similar offloads are not GPUCache components. They must not reappear on the roadmap as cache features. A later BlueField data-path milestone is not an erasure-coding milestone.

## Consequences

- The README and roadmap do not list an EC milestone.
- The V1 crates have no EC types, no parity buffers, and no replica placement. The file backend deletes or skips a bad file; it does not rebuild one. V1 C did not add parity. A short write or a full disk is a miss.
- RustFS remains responsible for any durable cold object it is asked to store. The cold client is off the hot path and is a contract mock unless an authorized endpoint is configured.
- V2 shard membership fences in-flight fills. It does not copy a block onto another node and it is not a durability replica.
- V3 rebalance and fault-domain isolation fence the same way. They do not copy a block. The DPU ablation refuses to enable hot-tier erasure coding.
- A cache miss after a node loss is correct behavior. The KV block is not the user's only copy.
