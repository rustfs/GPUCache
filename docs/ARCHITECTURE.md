# Architecture (V1 C, V2 first slice, V3 in progress)

GPUCache is a shared KV data tier. The hot tier is G3.5. RustFS is an optional colder G4 capacity tier. It is not on the prefill-to-decode path. The inference engine still owns tokenization, attention, and scheduling. This repo does not replace them.

```text
engine (not in tree; spike NOT_RUN_HARDWARE)
        |
        |  CacheIdentity + batch API
        v
gpucache-transport   TCP, loopback by default, tenant fingerprint
        |
        v
gpucache-storage::FileStore  -->  directory of ordinary files
        |
        v
gpucache-storage::MemoryStore
        |
        v
gpucache-core-types  (identity, keys, lifecycle, errors)

gpucache-cold        optional G4 pack/manifest, async sink, budgeted refill,
                     retention and reclaim. Not linked by transport.

V2 control plane (same process, not a new wire protocol)
  KvTransport        TCP submit is a host copy. NIXL open refuses.
  RegisteredPool     simulated pin, reuse, quarantine. Not a NIC.
  ShardDirectory     one shard per process. Epoch fence, single-flight, credits.

V3 (not on the TCP hot path)
  gpucache-dpu       simulated DRAM slab and storage queue. DOCA/SPDK/NVMe open refuses.
  ControlPlane       rebalance, rolling upgrade, fault domain. Fences fills. No replica.
  protocol harden    ACL, replay window, integrity seal, secret rotation.
```

`MemoryStore` remains the in-process contract. `FileStore` is what the TCP service uses. `gpucache-bench` only names the paired baselines and prints `NOT_RUN_HARDWARE`. Planned crates still absent: a standalone metadata service, an engine adapter, a DPU agent, and metrics. Their contracts stay out of `core-types` so GPU and DPU types do not leak in.

## Durability

The hot tier does not repair lost blocks. There is no erasure coding and no replica set. Loss, corruption, a short write, a full disk, eviction, and a failed check are clean misses. The engine recomputes and may write the block again. If the on-disk write fails after the in-memory commit, that commit is abandoned and the request id is forgotten, so a replay cannot report success for bytes that were not stored. An extra copy of a hot shared prefix is a possible later latency optimization, not a durability feature, and it is not in this tree. Cold durability is RustFS's responsibility. See [adr/0002-no-erasure-coding.md](adr/0002-no-erasure-coding.md).

## Identity

`CacheIdentity` is version 1. It carries tenant and trust domain, model revision, model-config digest, tokenizer revision, adapter/LoRA identity, KV dtype, quant format, layout ABI, TP/PP shard, position encoding, RoPE parameters (IEEE-754 bits), a token-prefix **hash** chain, block token capacity, valid length, and an optional multimodal processor/input digest.

Raw prompts, token ids, and media bytes are not fields. Callers hash those outside the store.

Keys:

- Logical prefix key (`GPCL`): tenant, model, tokenizer, adapter, prefix hashes, multimodal digests. Layout and shard are absent on purpose.
- Physical byte key (`GPCK`): the logical fields plus dtype, quant, layout, shard, RoPE, and lengths.

Both encodings are versioned and length-delimited. An exact hit requires the physical key and a `COMMITTED` block. A different physical key with the same logical key is an incompatible miss.

A longer hash chain can return `PrefixHit` for a committed block whose chain is a strict prefix, when layout and the other byte-affecting fields match. That hit is the whole stored block. See [adr/0003-partial-prefix.md](adr/0003-partial-prefix.md). The store does not slice a block down to a shorter `valid_length`.

## Lifecycle

```text
ABSENT -> RESERVED -> WRITING -> COMMITTED -> EVICTING -> ABSENT
                 \          \-> QUARANTINED -> ABSENT
                  \-> ABSENT (no buffer was registered)
```

`COMMITTED` is the only readable state, and only when the stored checksum still matches the bytes. `Get` of anything else returns an error and no payload. `Lookup` returns a hit, a whole-block `PrefixHit`, or a miss reason. It does not return payload bytes.

`QUARANTINED` means a registered region might still be in use. Timeout, lease expiry, and `KnownIdle` do not free it. Reclaim requires either `Attestation::Drained` from an authorized tenant caller, or `bump_epoch` plus `reclaim_fenced` with the new epoch. Region ids are monotonic and are not recycled.

## Quotas

`ResourceLimits` bounds object size, in-flight requests, registrations (including committed and quarantined buffers), per-tenant in-flight bytes, global retained bytes, and per-tenant retained bytes. The idempotency table is capped and entries expire. `MemoryStore` still refuses admission instead of evicting. `FileStore` evicts the oldest unpinned committed block when a retained-byte quota would fail. It does not evict another tenant to satisfy a tenant quota, and it does not evict a pinned or quarantined block. `Invalidate` remains the explicit client path.

## Process model

`MemoryStore` is single-threaded and in-process. The cross-process test exports a committed frame and runs `kv-block-xfer` in a child. The TCP service is a thread per connection with one mutex around `FileStore`. The read-buffer pool is bounded. Restart reloads committed files, bumps the epoch, and drops in-flight reservations. A corrupt or truncated file is skipped and is not a hit. A client that keeps talking to a stopped listener gets a transport error and must recompute; it does not keep a stale hit.

## Cold tier

`gpucache-cold` packs several blocks into one object and a manifest of payload offsets. Put, Head, RangeGet, and Delete are a trait. The mock implements them in memory. A live HTTP client runs only when the operator sets `GPUCACHE_RUSTFS_*`. Transport does not depend on this crate.

## V2 control plane

`KvTransport` is the byte-path trait. `TcpTransport::submit` copies into a bounded host buffer and reports `dma_posted=false`. `NixlTransport::open` fails closed. Feature `rdma-nixl` does not link libnixl. See [adr/0004-nixl-transport.md](adr/0004-nixl-transport.md) and [V2_DATA_PATHS.md](V2_DATA_PATHS.md).

`RegisteredPool` is a fixed set of `Vec<u8>` slots. Checkout reuses a binding instead of allocating a new region id. Unknown completion quarantines the slot. Lease expiry does not free it. `fence_epoch` then `reclaim_fenced` does, and so does `attest_drain`. Retired ids are not recycled.

`ShardDirectory` is local to one shard. Shard 0 is the single-node case and uses the same methods. A membership change bumps the epoch, drops in-flight fills, and releases credits. Single-flight joins do not charge twice and do not return payload bytes. There is no per-token central directory and no per-block quorum. See [adr/0005-shard-fencing.md](adr/0005-shard-fencing.md).

The cold sink queues a pack and drains it with Put. Refill Heads the manifest first. Over budget returns `OverBudget` and no payload. That outcome is a recompute, not a hit.

## V3 offload and hardening

`gpucache-dpu` is a separate crate. The hot path does not call it. The candidate offload is the storage queue: submit, then complete, against a bounded in-process slab. Completions are labeled `NOT_RUN_HARDWARE`. `ArmAgent::start` does not set `offload_active`. See [adr/0006-storage-queue-offload.md](adr/0006-storage-queue-offload.md) and [V3_DPU_TARGETS.md](V3_DPU_TARGETS.md).

`AffineQueue` records the reactor id a future SPDK queue would be bound to. A mismatched id is an error. No reactor thread is started.

`ControlPlane` wraps one `ShardDirectory`. Rebalance fences the source and returns an empty destination directory. Rolling upgrade rejects new admits while draining. Finish fences. Rollback before finish lets the in-flight fill complete. Fault-domain isolation removes members in that domain and refuses to remove the last member. None of these copy payload bytes.

G4 `ColdOps` records retention, checks `MANIFEST_VERSION`, reclaims through delete plus a following head, and refuses a retired tenant key. `refill_degraded` turns a slow or unavailable RustFS into `Missing` without touching the store. Backfill enqueue stops at a byte budget. Live HTTP is unchanged and still `NOT_RUN_HARDWARE` without `GPUCACHE_RUSTFS_*`.

The degrade table is [V3_FAILURES.md](V3_FAILURES.md). Every row is a clean miss.
