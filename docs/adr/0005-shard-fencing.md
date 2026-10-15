# ADR 0005: Shard-local directory and fencing

- Status: **Accepted**
- Date: 2026-10-15
- Milestone: V2 first slice

## Context

A distributed KV tier needs to know which process may fill a block, and it needs to reject work from a stale membership. A central directory touched once per token, or a quorum vote per block, would sit on the miss path of recomputable data. V1 already fences buffer reclaim with a store epoch. The client wire format is stable from one process to many.

## Decision

`ShardDirectory` is local to one shard. One process is one shard. A single machine is shard 0 using the same methods as any other shard id.

- Membership is a list of `(node_id, incarnation)` on that shard. A new member, a removed member, or a changed incarnation bumps the shard epoch and fails in-flight fills. An unchanged incarnation does not. The last member cannot be removed.
- There is no cross-shard quorum and no per-token index. The map key is the SHA-256 of the physical byte key. One entry is one in-flight fill.
- Single-flight: the first same-epoch caller is the leader and reserves capacity credits once. A later same-tenant caller joins and does not reserve again. A different tenant is `Unauthorized`. The directory stores no payload. `Admit` is `Leader` or `Joined` only.
- Completing or failing a fill requires the leader request id and the current epoch. A stale epoch is `EpochMismatch`. After the epoch moves, the fill is gone, credits are released, and a new fill may start. That new fill is not a hit.
- Credits are an admission ceiling (`InFlightBytes`, `TenantRetainedBytes`). Exhaustion refuses the new leader and does not evict an existing one. This is not a replica and not erasure coding. See [0002-no-erasure-coding.md](0002-no-erasure-coding.md).

The GCK1 request bytes and GCKR replies are unchanged.

## Consequences

- Tests construct two `ShardDirectory` values and show they do not share flights. That is the single-node and the multi-shard API. It is not a multi-host deployment.
- A lost shard is a miss. The engine recomputes and may fill again.
- Buffer quarantine stays in `RegisteredPool`: unknown completion and lease expiry do not free a slot. Fence plus `reclaim_fenced`, or `attest_drain`, does. Region ids are not recycled. The pool's bytes are a `Vec<u8>`. No NIC memory registration is performed.
