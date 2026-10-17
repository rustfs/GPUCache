# V2 test matrix

CPU contract for the first V2 slice. Live RDMA, live RustFS, GPU export, and token goodput stay `NOT_RUN_HARDWARE`. This file is not a performance report.

| Check | Result | Where |
| --- | --- | --- |
| Default workspace tests, including V1 A/B/C | PASS | `cargo test --workspace --all-targets` |
| TCP transport trait: one host copy, backpressure, `dma_posted=false` | PASS | `iface::tests` |
| `NixlTransport::open` refuses without a device. `submit_preferring_rdma` labels the TCP fallback `RdmaNotLinked` and does not post | PASS | `iface::tests`, `tests/v2_control.rs` |
| Feature `rdma-nixl` still refuses `open` | PASS | `cargo test -p gpucache-transport --all-targets --features rdma-nixl` |
| Registration reuse, prefer an idle binding, atomic checkout, quarantine, lease hold, fence reclaim, epoch overflow | PASS | `registered::tests` |
| Failed TCP stage quarantines the simulated slots | PASS | `stage::tests` |
| Single-node directory is one shard; join charges credits once | PASS | `directory::tests` |
| Two shard values do not share flights | PASS | `directory::tests::two_shard_values_do_not_share_flights` |
| Stale epoch fences the fill and releases credits. Finish, failure, and fence are distinct notices with no payload. Epoch overflow keeps the fill | PASS | `directory::tests` |
| Other tenant cannot join; quota does not drop the leader | PASS | `directory::tests` |
| `shard-demo` prints `SHARD_OK` and `rdma=NOT_RUN_HARDWARE` | PASS | `cargo run -p gpucache-transport --bin shard-demo` |
| Cold mock Put/Head/RangeGet/Delete, pack, manifest | PASS | `contract_smoke` |
| Async sink publication, bounded retry, failed-job retrieval, cancellation and shared backpressure | REQUIRED_CPU | `sink::tests`; checklist below |
| Identity-bound v3 refill, legacy v1/v2 misses, and download budgets | REQUIRED_CPU | Cold refill/pack tests; checklist below |
| `cold-demo` prints `COLD_CONTRACT_OK` and `gds=NOT_RUN_HARDWARE` | PASS | `cargo run -p gpucache-cold --bin cold-demo` |
| Live RustFS Put/Head/RangeGet/Delete | NOT_RUN_HARDWARE | No `GPUCACHE_RUSTFS_*` endpoint |
| Live NIXL / Soft-RoCE / hardware RDMA | NOT_RUN_HARDWARE | `libnixl` is not linked. See [adr/0004-nixl-transport.md](adr/0004-nixl-transport.md) |
| GPU KV export, NIXL P/D, TTFT, TPOT, goodput | NOT_RUN_HARDWARE | `cargo run -p gpucache-bench --bin paired-baseline` |
| BlueField, DOCA, GDS, SPDK | NOT_RUN_HARDWARE | Not started. Not an erasure-coding milestone |

## Cold publication and batch-fencing regressions

This checklist defines required coverage for the updated CPU contract. It is not a retained test report: `REQUIRED_CPU` does not mean that a new checkout has passed. Record the exact revision and command outcomes with the existing [VM runner](VM_REGRESSION.md#pr-8-cold-and-fencing-regressions). The workspace test gate includes these Rust tests; no separate PR-specific runner is needed.

| Contract | Required outcome | Test area |
| --- | --- | --- |
| Permanent sink error followed by a healthy job | The bad job is isolated, the healthy job publishes, and the failed job can be retrieved independently | `sink::tests` |
| Temporary sink error | Each selected job is attempted once per drain call; unrelated jobs advance; retry attempts never exceed `MAX_WRITE_ATTEMPTS` (3) | `sink::tests` |
| Retry exhaustion | Move the job to the failed queue; later drain calls do not automatically retry it | `sink::tests` |
| Partial progress and reporting | `drain_report` counts attempts, completions, retries and terminal failures; `drain` returns the first error after processing the round without undoing completed jobs | `sink::tests` |
| Shared accounting | Pending and failed jobs jointly obey `max_jobs` and `max_bytes`; failed-job retrieval and cancel release exact credit; metadata cannot exceed `MAX_SINK_JOBS` (4096) | `sink::tests` |
| Ambiguous object or manifest PUT | Simulate commit followed by an error, not only failure before writing; preserve any object a visible manifest could reference; no speculative DELETE | `sink::tests` |
| Retry ordering under one namespace | Distinct packs use distinct content-addressed keys; a delayed older retry cannot overwrite the later pack | Cold pack/sink tests |
| Forged or mismatched queued pack | Reject an inconsistent key, manifest/object pairing, identity digest, range or payload checksum before issuing a store write | Cold pack/sink tests |
| Full expected identity | A wrong tenant, trust domain, model/config/tokenizer revision, adapter, prefix, position/RoPE, dtype/quantization, layout/shard or valid length cannot return payload bytes | `tests/refill_safety.rs::complete_identity_mismatches_are_misses_before_payload_io` |
| Every public refill entry point | `refill_generation`, `refill_matching`, `refill_degraded` and `refill_for_tenant` all require and enforce the expected `CacheIdentity`; no tenant-free helper bypass | `tests/refill_safety.rs::every_public_refill_entry_requires_and_checks_the_expected_identity` |
| Legacy manifests | Valid legacy v1/v2 headers yield `Missing` with zero payload-range reads; old entries require recompute and repack | `tests/refill_safety.rs::legacy_v1_and_v2_fixtures_are_clean_misses_without_payload_reads` |
| Budget and malformed input | Over-budget manifests are not downloaded through refill; disallowed payload ranges are not downloaded; malformed v3 metadata and corrupt payloads fail closed | `tests/refill_safety.rs`, including `malformed_v3_manifest_fails_closed_before_payload_io` and `corrupt_payload_and_content_key_substitution_return_no_loaded_bytes` |
| Slow or unavailable G4 | Return `Missing` without calling the object store | `ops::tests` |
| Domain-isolation epoch shortage | Insufficient epoch budget for the complete victim set leaves members, domains, epoch, credits and leader/joiner notices unchanged | `plane::tests::isolate_domain_epoch_shortage_keeps_the_entire_batch_and_fills_unchanged` |
| Domain-isolation exact budget | Every victim is removed and every affected old fill is fenced; a one-victim overflow also makes no change | `plane::tests` |

These tests use trusted CPU callers and synthetic bytes. They do not authenticate an external tenant, prove KV model semantics, qualify orphan garbage collection, or exercise real RDMA/DPU drain. Hardware rows above remain `NOT_RUN_HARDWARE`.
