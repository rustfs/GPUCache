# Architecture (V1 A)

GPUCache is a shared KV data tier. The hot tier is G3.5. RustFS is an optional colder G4 capacity tier and is not implemented in this milestone. The inference engine still owns tokenization, attention, and scheduling. This repo does not replace them.

```text
engine (not in tree; spike NOT_RUN_HARDWARE)
        |
        |  CacheIdentity + batch API
        v
gpucache-protocol
        |
        v
gpucache-storage::MemoryStore
        |
        v
gpucache-core-types  (identity, keys, lifecycle, errors)
```

Planned crates that are **not** in V1 A: `metadata` as a standalone service, `transport` (TCP), file/NVMe store, RustFS object store, engine adapter, DPU agent, bench harness, metrics. Their contracts are reserved so later milestones do not push GPU or DPU types into `core-types`.

## Durability

The hot tier does not repair lost blocks. There is no erasure coding and no replica set. Loss, corruption, eviction, and a failed check are clean misses. The engine recomputes and may write the block again. An extra copy of a hot shared prefix is a possible later latency optimization, not a durability feature, and it is not in this tree. Cold durability is RustFS's responsibility. See [adr/0002-no-erasure-coding.md](adr/0002-no-erasure-coding.md).

## Identity

`CacheIdentity` is version 1. It carries tenant and trust domain, model revision, model-config digest, tokenizer revision, adapter/LoRA identity, KV dtype, quant format, layout ABI, TP/PP shard, position encoding, RoPE parameters (IEEE-754 bits), a token-prefix **hash** chain, block token capacity, valid length, and an optional multimodal processor/input digest.

Raw prompts, token ids, and media bytes are not fields. Callers hash those outside the store.

Keys:

- Logical prefix key (`GPCL`): tenant, model, tokenizer, adapter, prefix hashes, multimodal digests. Layout and shard are absent on purpose.
- Physical byte key (`GPCK`): the logical fields plus dtype, quant, layout, shard, RoPE, and lengths.

Both encodings are versioned and length-delimited. V1 hits only when the physical key matches a `COMMITTED` block. A different physical key with the same logical key is an incompatible miss. Partial-prefix reuse is not implemented; a shorter chain is a different key.

## Lifecycle

```text
ABSENT -> RESERVED -> WRITING -> COMMITTED -> EVICTING -> ABSENT
                 \          \-> QUARANTINED -> ABSENT
                  \-> ABSENT (no buffer was registered)
```

`COMMITTED` is the only readable state, and only when the stored checksum still matches the bytes. `Get` of anything else returns an error and no payload. `Lookup` returns a miss reason and no payload.

`QUARANTINED` means a registered region might still be in use. Timeout, lease expiry, and `KnownIdle` do not free it. Reclaim requires either `Attestation::Drained` from an authorized tenant caller, or `bump_epoch` plus `reclaim_fenced` with the new epoch. Region ids are monotonic and are not recycled.

## Quotas

`ResourceLimits` bounds object size, in-flight requests, registrations (including committed and quarantined buffers), per-tenant in-flight bytes, global retained bytes, and per-tenant retained bytes. The idempotency table is capped and entries expire. Over-capacity admission fails; V1 A does not silently evict a committed block to make room. `Invalidate` is the explicit eviction path.

## Process model

`MemoryStore` is single-threaded and in-process. The cross-process test exports a committed frame and runs `kv-block-xfer` in a child, which imports through the same public API. That is a wire-format check, not a network service. TCP is V1 B.
