# Operations (V1 C)

V1 B is a library, an import helper, and a localhost TCP service. There is no package and no production config. The service creates files only under the directory you pass. It does not format NVMe, change a NIC, or touch a DPU.

## Reproduce the CPU tests

```bash
cargo test --workspace --all-targets
```

Rust stable is selected by `rust-toolchain.toml`. The declared MSRV is 1.85. CI also runs `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`.

## Import one frame

`kv-block-xfer` is built by the test suite. It reads a `GEXP` frame on stdin:

```bash
cargo run -p gpucache-storage --bin kv-block-xfer -- import <64-hex-char fingerprint>
```

The fingerprint is the tenant credential hash, not a prompt. The tool creates a one-tenant `MemoryStore`, commits the block, and prints `status=IMPORTED` only after lookup and get succeed. A checksum failure exits 2 and prints `error=` on stderr. This path uses synthetic or caller-supplied bytes. It does not contact vLLM.

## Two-client reuse

```bash
cargo run -p gpucache-transport --bin share-demo
```

This starts a loopback server on an ephemeral port, writes one synthetic block from one client, and reads it from a second client. Success is `status=REUSE_OK`. `partial_prefix=WHOLE_BLOCK_PREFIX` means the longer chain reused the whole shorter block. `intra_block_slice=DEGRADED_MISS` means a shorter `valid_length` was not sliced out of that block.

## Hit, miss, and fallback

```bash
cargo run -p gpucache-transport --bin fault-demo
```

The command uses a fresh temp directory. It commits a synthetic token block, reads it back from a second client, and checks the token ids exactly. An unknown key, a corrupt file after restart, a past deadline, a cancel, and a quota refusal all fail closed. The caller recomputes with the same pure function of those token ids. `status=FALLBACK_OK` is printed only after those checks. `tolerance=NONE` means no numeric slack was applied. A wrong tenant, model, tokenizer, adapter, or layout is not a hit.

## V2 shard and cold demos

```bash
cargo run -p gpucache-transport --bin shard-demo
cargo run -p gpucache-cold --bin cold-demo
```

`shard-demo` checks one shard, a joined fill, epoch fencing, quarantine across lease expiry, and a TCP host copy. It prints `rdma=NOT_RUN_HARDWARE`. `cold-demo` runs the mock pack, sink, and refill. It prints `gds=NOT_RUN_HARDWARE`. Without `GPUCACHE_RUSTFS_*` it also prints `rustfs_live=NOT_RUN_HARDWARE`.

Optional feature check, still with no device:

```bash
cargo test -p gpucache-transport --all-targets --features rdma-nixl
```

The paired-baseline stub does not measure anything:

```bash
cargo run -p gpucache-bench --bin paired-baseline
```

`status=NOT_RUN_HARDWARE` is the honest result until a GPU run exists. See [BENCHMARKS.md](BENCHMARKS.md) and [CORRECTNESS.md](CORRECTNESS.md).

## TCP service

```bash
cargo run -p gpucache-transport --bin gpucache-tcp -- serve \
  --data-dir /path/to/an/empty/test/dir \
  --tenant tenant-a \
  --trust td-local \
  --fingerprint <64 hex chars of a credential hash>
```

Default `--bind` is `127.0.0.1:9735`. `--allow-remote` is required before a non-loopback address is accepted. The process still checks the fingerprint.

### Config schema

| Field | Default | Meaning |
| --- | --- | --- |
| bind | `127.0.0.1:9735` (binary), port `0` in tests | Socket. Non-loopback needs `allow_remote` |
| data_dir | required, no default | Directory of `MANIFEST`, `LRU`, and `blocks/*.gblk` |
| tenant, trust, fingerprint | required | One V1 policy. Fingerprint is 32 bytes, not all zero |
| allow_remote | false | Does not disable auth |
| max_connections | 16 | Extra connections are dropped |
| max_frame_bytes | 1 MiB in the library, 2 MiB in the binary | Body cap. Larger declared lengths are rejected before read |
| pool_buffers | 16 | Read-buffer pool. Checkout fails at the cap |
| io_timeout | 5 s | Read and write timeout on each connection |
| ResourceLimits | `dev_defaults()` | Object size, in-flight, registrations, retained bytes |

There is no on-disk config file. Flags are the schema.

## RustFS probe

Leave `GPUCACHE_RUSTFS_ENDPOINT`, `GPUCACHE_RUSTFS_BUCKET`, `GPUCACHE_RUSTFS_PREFIX`, `GPUCACHE_RUSTFS_ACCESS_KEY`, and `GPUCACHE_RUSTFS_SECRET_KEY` unset unless an operator has authorized a test bucket. Unset means `NOT_RUN_HARDWARE`. The mock smoke still runs inside `cargo test`. `https://` is `NOT_RUN_HARDWARE` in this milestone because no TLS stack is linked. Do not invent credentials.

## V3 CPU commands

```bash
cargo run -p gpucache-dpu --bin dpu-demo
cargo run -p gpucache-bench --bin v3-paired
cargo test -p gpucache-dpu --all-targets --features dpu-bf3
```

`dpu-demo` prints the contract result and `NOT_RUN_HARDWARE`. `v3-paired` prints `TCO_UNKNOWN` and `go_gate=NOT_EVALUATED`. The feature test still refuses DOCA, SPDK, and NVMe open.

## Diagnostics

`dpu-demo` is the diagnostics sketch. It prints whether `dpu-bf3` was selected, that DOCA/SPDK/NVMe are not linked, that hot-tier EC is off, and that each planned target is `NOT_RUN_HARDWARE`. It does not print payloads, fingerprints, or prompts. There is no device health counter because no device is open.

## Capacity

Hot-tier limits (`ResourceLimits`, shard credits, the simulated slab cap, cold retention, the backfill byte budget) are admission ceilings for recomputable KV. They are not a measured cluster size. A lost hot block is a miss. Do not size the hot tier as the only copy of user data, and do not describe this tree as PB-scale. Cold retention deletes expired mock objects after a verified delete. It is not a second durability replica of the hot tier.

## Upgrade and rollback

`ControlPlane::begin_upgrade` stops new fills. In-flight fills can still complete. `rollback` before finish returns to serving without a fence. `finish_upgrade` bumps a software generation and fences, so old epochs miss and the engine recomputes. `rollback` after finish fences again. The generation counter does not decrease. No BlueField firmware, BFB, or DOCA package is part of this rollback. The TCP wire format is unchanged apart from quota kind byte 8 (`RateLimit`) on error replies.

## Minimal deploy example

Development only, loopback, ordinary files, no DPU mode change:

```bash
cargo run -p gpucache-transport --bin gpucache-tcp -- serve \
  --data-dir /tmp/gpucache-dev \
  --tenant tenant-a \
  --trust td-local \
  --fingerprint <64 hex chars of a credential hash>
```

Then, in another shell, `cargo run -p gpucache-dpu --bin dpu-demo` shows the offload interface is still `NOT_RUN_HARDWARE`. Do not point `--data-dir` at a device node. Do not pass a live secret as the fingerprint; pass the hash.

## Compatibility

See [../COMPATIBILITY.md](../COMPATIBILITY.md) and [V3_DPU_TARGETS.md](V3_DPU_TARGETS.md). TCP remains the correctness path. `rdma-nixl` and `dpu-bf3` are empty features. aarch64 CI is a CPU run.

## Not operated here

No NVMe format, no DPU mode change, no firmware, no IOMMU or BIOS change, no production network change, no hardware purchase. Those need a separate authorization. There is no website deploy and no crates.io publish in this milestone. G4 live calls stay env-gated.

## CPU audit changes (2026-10-15)

LRU recency is advisory. Lookup and Get update it in memory without filesystem writes.
Commit batches and invalidation checkpoint it; restart may lose recent read ordering.
Block persistence still uses the existing atomic-write and sync policy. A stale LRU
checkpoint can change eviction order, never tenant identity or checksum validation.

A TCP client becomes unusable after an incomplete or malformed exchange. Recompute
and reconnect before issuing another cache request. The connect call uses its
configured timeout. Socket I/O timeouts are not total RPC deadlines and do not
bound a store lock or a filesystem syscall. Zero server I/O timeouts are rejected.

The buffer pool owns receive payloads through decoding; framing writes borrow the
encoded payload through vectored I/O. This removes framing copies, not all copies.
Serialization, storage ownership, and Get replies still copy bytes. Parsing and
reply serialization run outside the store mutex; storage I/O remains inside it.

Run the optional local microbenchmarks with
`cargo run --release -p gpucache-transport --example cpu_audit`.
These measure synthetic CPU/file operations only. They do not measure RDMA, GPU
inference, TTFT, TPOT, DPU savings, or TCO.
