# GPUCache

GPUCache is an open shared KV-cache tier for disaggregated inference. In NVIDIA CMX terminology this tier is **G3.5**: a shared cache in front of GPU HBM, reused across requests that have the same tenant, model, and layout. [RustFS](https://github.com/rustfs/rustfs) is the colder **G4** capacity backend. G4 is not on the prefill-to-decode hot path.

A KV block is recomputable accelerator state. It is not the user's only copy of a prompt, and it is not model-agnostic long-term memory. A miss, timeout, checksum failure, version mismatch, or store outage must fall back to the inference engine and recompute. A wrong hit is a failure.

GPUCache does not do erasure coding and does not keep durability replicas. A lost or failed hot block is a clean miss: the engine recomputes and may repopulate the cache. Durability for cold data is RustFS G4's job. An extra copy of a hot shared prefix may be added later as a latency hint; that copy is not erasure coding and it is not a durability promise. See [docs/adr/0002-no-erasure-coding.md](docs/adr/0002-no-erasure-coding.md).

## Status

**V1 milestones A, B, and C are implemented. The V2 first slice is in this tree. V3 is in progress: a CPU contract for a storage-side BlueField path, plus experimental control and security hooks that do not need a DPU. DPU acceleration is not measured.**

V1 runs the cache contract on CPU. V2 adds a transport trait, a simulated registered-buffer pool, a shard-local directory, and a stricter G4 cold contract. V3 adds an optional `dpu-bf3` feature that does not link DOCA or SPDK, a simulated DRAM-slab storage queue, and CPU-side authz, replay, quota, and cold-ops hooks. The long-term deployment target is a storage-side BlueField-3. The GPU client keeps an ordinary ConnectX. This repository does not link DOCA, CUDA, RDMA, or SPDK, and it does not claim that any of those paths ran. Feature `rdma-nixl` does not link libnixl. Feature `dpu-bf3` does not link a device. An aarch64 build is a CPU build. It is not a BlueField functional test. GDS, DOCA, and BlueField offload are in development / `NOT_RUN_HARDWARE` / optional feature. They are not "already implemented" and not "deployed".

| Milestone | Status |
| --- | --- |
| V1 A: identity, protocol, `MemoryStore`, error/timeout/cancel tests | Implemented |
| V1 A: real vLLM KV block export/import | `NOT_RUN_HARDWARE` (no GPU in this environment) |
| V1 B: TCP service, file backend, LRU admission, tenant quotas, two-client reuse | Implemented in this tree |
| V1 B: RustFS live Put/Head/RangeGet/Delete | `NOT_RUN_HARDWARE` (no authorized endpoint). Contract mock passes |
| V1 C: fuzz, fault injection, synthetic token checks, paired-baseline stubs | Implemented in this tree. Real engine output is `NOT_RUN_HARDWARE` |
| V2: `KvTransport`, TCP default, feature `rdma-nixl` | In progress. The feature compiles and `NixlTransport::open` refuses. Live RDMA is `NOT_RUN_HARDWARE` |
| V2: bounded registered buffer pool (simulated pin, reuse, quarantine, backpressure) | Implemented in this tree. Checkout prefers an already-bound slot and does not partially check out a batch. Not a NIC registration |
| V2: shard-local directory, epoch fencing, single-flight, capacity credits | Implemented in-process. One process is one shard. Finish, failure, and fence leave a notice with no payload. An epoch that cannot advance does not drop the fill. No cross-shard quorum |
| V2: G4 pack/manifest, async sink, cost-bounded refill | Implemented against the mock. A failed drain keeps the job. A wrong-tenant refill does not return bytes. Live RustFS is `NOT_RUN_HARDWARE` |
| V2: P/D, G3.5, and G4 hop list | Documented in [docs/V2_DATA_PATHS.md](docs/V2_DATA_PATHS.md). Not a zero-copy claim |
| V2: paired TTFT/TPOT against native NIXL | `NOT_RUN_HARDWARE` |
| V3: `dpu-bf3` feature slot, simulated DRAM slab + storage queue, SPDK affinity check | In progress. Devices are not linked. `open_doca` / `open_nvme_device` / `open_spdk` refuse. `dpu-demo` prints `NOT_RUN_HARDWARE` |
| V3: one candidate offload (storage queue) and ablation stubs | Interface only. P2P, DPA, compression, and crypto offload stay `NOT_RUN_HARDWARE` when enabled. Hot-tier EC cannot be enabled |
| V3: authz, replay window, integrity seal, rate limit, secret rotation, audit | CPU hooks in this tree. Not mTLS. Trusted-fabric boundary is [docs/adr/0007-trusted-fabric.md](docs/adr/0007-trusted-fabric.md) |
| V3: rebalance, rolling upgrade, fault-domain isolation | In-process stubs on the V2 shard directory. They fence fills. They do not copy a replica |
| V3: degrade matrix (DPU restart, GPU reset, NIC flap, storage loss, NVMe timeout, control-plane loss, RustFS slow/down) | Simulated fault decisions request recompute. Unknown completions remain quarantined and charged until explicit drain. Tenant-scoped invalidation/reclaim cannot free referenced slots. Recovery is not proven on hardware |
| V3: G4 retention, manifest version, reclaim, delete verification, tenant cold keys, backfill budget | Implemented against the mock. Live RustFS is `NOT_RUN_HARDWARE` |
| V3: paired NIC-RDMA baseline vs DPU | `NOT_RUN_HARDWARE`. `tco=TCO_UNKNOWN`. `go_gate=NOT_EVALUATED`. TCP is not the DPU baseline |
| V3: second engine (SGLang / HiCache) | Not added. Evaluation note only. See [docs/adr/0008-second-engine.md](docs/adr/0008-second-engine.md) |
| Mooncake-informed CPU contracts | Implemented in this tree. See [docs/MOONCAKE.md](docs/MOONCAKE.md) and [docs/adr/0009-mooncake-lessons.md](docs/adr/0009-mooncake-lessons.md). Mooncake is not linked. No NIC, Soft-RoCE, or DPU number is claimed |
| Erasure coding and durability replicas | Will not be built. A lost or failed hot block is a clean miss, then recompute and repopulate. Cold durability is RustFS G4 |

The integration route (native vLLM KV connector or LMCache) is **not chosen**. See [docs/adr/0001-integration-route.md](docs/adr/0001-integration-route.md). Hot-tier failure policy is [docs/adr/0002-no-erasure-coding.md](docs/adr/0002-no-erasure-coding.md). The V3 offload candidate is [docs/adr/0006-storage-queue-offload.md](docs/adr/0006-storage-queue-offload.md). It is a hypothesis pending a paired measurement, not a measured bottleneck.

## What V1 A does

Three library crates, none of which depend on a GPU or DPU:

- `gpucache-core-types` — versioned `CacheIdentity`, length-delimited physical and logical keys, the block lifecycle, checksums, quotas, and errors.
- `gpucache-protocol` — tenant auth context, batch messages, deadlines, and wire-version negotiation.
- `gpucache-storage` — in-process `MemoryStore`.

The batch API is Lookup, Reserve/Put, Commit, Get, Release/Cancel, and Invalidate. Every item carries `request_id`, epoch, deadline, and, where it applies, generation, length, and checksum. Only a `COMMITTED` block whose identity is compatible is a hit. `ABSENT`, `RESERVED`, `WRITING`, `EVICTING`, and `QUARANTINED` blocks are not readable.

## What V1 B adds

`gpucache-transport` serves that same API over TCP. The default bind is `127.0.0.1`. The process refuses to listen on a non-loopback address unless `--allow-remote` is set, and it still requires a tenant credential fingerprint. There is no unauthenticated data path.

`FileStore` keeps committed blocks as ordinary files under a directory you pass in. It does not format a disk and it does not open a device node. When a tenant or global byte quota would fail, it evicts the oldest unpinned committed block. A pinned or quarantined block stays. `MemoryStore` still fails admission instead of evicting, so the V1 A tests keep that behavior.

Lookup can return `PrefixHit` when a committed block's hash chain is a strict prefix of the query and the layout, dtype, shard, and model identity match. The hit is the whole stored block. `Get` must use `identity_for_prefix_hit`. A shorter `valid_length` on the same chain is an incompatible miss. The service does not slice inside a block.

`gpucache-cold` packs blocks into an object plus a manifest and can Put, Head, RangeGet, and Delete through a trait. The TCP hot path does not call it. Without `GPUCACHE_RUSTFS_*` variables the live probe is `NOT_RUN_HARDWARE`. The mock still runs. V2 adds a bounded async sink and a refill that returns no bytes when the combined manifest and payload downloads exceed its byte budget.

Lifecycle:

```text
ABSENT -> RESERVED -> WRITING -> COMMITTED -> EVICTING -> ABSENT
```

A failed or timed-out registered region goes to `QUARANTINED`. Lease expiry does not prove that a copy has stopped, so that buffer stays allocated until the owner attests drain or a newer store epoch fences it. Same tokens do not imply the same bytes: the logical prefix key and the physical byte key are different encodings. V1 reuses only within one tenant, one model, and one layout. A reshard, quant, or layout mismatch is a miss. There is no cross-tenant dedup. A longer hash chain can reuse a whole shorter committed block (`PrefixHit`); the service does not slice inside a block. See [docs/adr/0003-partial-prefix.md](docs/adr/0003-partial-prefix.md).

Resource limits cover object size, writer admissions, memory registrations, per-tenant in-flight bytes, and retained payload bytes. They are not a complete process-RSS bound: reader pins, protocol reply allocations, and some auxiliary metadata still need admission limits. The idempotency table is bounded and scoped by authenticated tenant and trust domain.

## What you can run

For an unattended VM or Grok bot regression, run:

```bash
python3 scripts/ci/vm_regression.py --profile cpu --output artifacts/vm-regression
```

The runner checks the locked dependency contract, formatting, Clippy, workspace and doc tests, optional and combined features, and CPU demos. It writes JSON, JUnit, a summary and per-gate logs. Hardware profiles return `NOT_RUN_HARDWARE` and exit 2; a CPU pass does not qualify RDMA, GDS, a GPU engine or BF3. Live RustFS access is disabled for this profile, including Cargo-configured endpoints. See the [VM runbook](docs/VM_REGRESSION.md), [staged roadmap](docs/ROADMAP.md), [replacement strategy](docs/REPLACEMENT_STRATEGY.md), and [English PR plan](docs/PR_PLAN.md).

V3 regression hardening rejects active replay-window eviction, cross-domain replay, retired credential revival and malformed identities. TCP replies must match the operation, epoch and ordered request IDs before a connection can be reused. The DPU queue simulation bounds task/slot counts and retains unknown-completion credits; these checks do not constitute device isolation or hardware drain.

From a clean checkout, with Rust stable (MSRV 1.85):

```bash
cargo test --workspace --all-targets
```

That command is the CPU contract repro for V1 A, V1 B, and V1 C. It includes unit tests, property tests, protocol fuzz, timeout and cancel tests, a single-process import of a synthetic block, a cross-process import through `kv-block-xfer`, file-backend quota and restart tests, the TCP reuse tests, and the V1 C fault matrix. The bytes are synthetic. They are not a vLLM KV tensor.

Two localhost clients sharing one file-backed service:

```bash
cargo run -p gpucache-transport --bin share-demo
```

A successful run prints `status=REUSE_OK` and `partial_prefix=WHOLE_BLOCK_PREFIX`. It does not print a sliced-block hit.

Hit, miss, and recompute fallback from a clean temp directory:

```bash
cargo run -p gpucache-transport --bin fault-demo
```

Success is `status=FALLBACK_OK` plus `criteria=EXACT_TOKEN_IDS` and `tolerance=NONE`. The process prints that banner only after the checks pass. A wrong tenant or wrong model is not a hit. This is not a vLLM run.

Paired GPU baselines stay `NOT_RUN_HARDWARE` and print no timings:

```bash
cargo run -p gpucache-bench --bin paired-baseline
```

One shard, simulated registration, and the TCP staging path:

```bash
cargo run -p gpucache-transport --bin shard-demo
```

Success is `status=SHARD_OK`, `registration=SIMULATED`, and `rdma=NOT_RUN_HARDWARE`.

Mock G4 pack, sink, and budget-capped refill:

```bash
cargo run -p gpucache-cold --bin cold-demo
```

Success is `status=COLD_CONTRACT_OK` and `gds=NOT_RUN_HARDWARE`. `rustfs_live=NOT_RUN_HARDWARE` when no endpoint is configured.

In-process DPU contract. This does not open a device:

```bash
cargo run -p gpucache-dpu --bin dpu-demo
```

Success is `status=DPU_CONTRACT_OK`, `offload_active=false`, and `hardware=NOT_RUN_HARDWARE`. `doca=NOT_LINKED`. Arm process start is not an offload.

Optional feature, still with no SDK:

```bash
cargo test -p gpucache-dpu --all-targets --features dpu-bf3
```

V3 paired gate. No timings, no TCO, TCP is not the baseline:

```bash
cargo run -p gpucache-bench --bin v3-paired
```

`kv-block-xfer import <credential-hex>` reads one export frame on stdin and imports it into a fresh `MemoryStore`. It does not talk to an engine.

## Layout

```text
crates/core-types    identity, keys, lifecycle, errors
crates/protocol      batch messages, replies, version negotiation
crates/storage       MemoryStore, FileStore, kv-block-xfer
crates/transport     TCP server, client, transport trait, shard directory, share-demo, fault-demo, shard-demo
crates/cold          RustFS contract, pack/manifest, async sink, refill, retention, cold-demo
crates/dpu           BlueField feature slot, simulated storage queue, dpu-demo. Not linked by transport
crates/bench         paired-baseline and v3-paired stubs (no timings)
docs/adr             decision records
COMPATIBILITY.md     environment manifest
```

Design notes: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PROTOCOL.md](docs/PROTOCOL.md), [docs/SECURITY.md](docs/SECURITY.md), [docs/OPERATIONS.md](docs/OPERATIONS.md), [docs/BENCHMARKS.md](docs/BENCHMARKS.md), [docs/CORRECTNESS.md](docs/CORRECTNESS.md), [docs/RISKS.md](docs/RISKS.md), [docs/MOONCAKE.md](docs/MOONCAKE.md), [docs/V1C_TEST_MATRIX.md](docs/V1C_TEST_MATRIX.md), [docs/V2_DATA_PATHS.md](docs/V2_DATA_PATHS.md), [docs/V2_TEST_MATRIX.md](docs/V2_TEST_MATRIX.md), [docs/V3_DPU_TARGETS.md](docs/V3_DPU_TARGETS.md), [docs/V3_TEST_MATRIX.md](docs/V3_TEST_MATRIX.md), [docs/V3_FAILURES.md](docs/V3_FAILURES.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
