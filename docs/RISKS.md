# Risks (V1 C)

| Risk | Status | Note |
| --- | --- | --- |
| Wrong-tenant or wrong-layout hit | Mitigated in `MemoryStore` tests | Physical key includes tenant and layout. Incompatible lookup returns a miss and no bytes. |
| Reclaim of a buffer that might still be copied | Mitigated by quarantine | Timeout and `KnownIdle` do not free a registered region. |
| Unbounded memory | Partially mitigated | Retained payloads and writers have quotas; FileStore can evict unpinned committed blocks. Reader pins and some auxiliary metadata remain uncapped. Protocol reply copies also need byte admission. Quarantine stays charged until fence or drain. |
| Idempotency window expiry | Accepted | After TTL, a request id may run again. Callers must not reuse ids they still care about. |
| `Drained` is only tenant-authenticated | Accepted for V1 A | Not a device proof. Documented in SECURITY.md. |
| No real engine bytes | `NOT_RUN_HARDWARE` | Synthetic frames can pass while a real vLLM layout still mismatches. |
| TCP exposure | Mitigated for the default bind | Loopback unless `--allow-remote`. Empty policy and bad fingerprints fail closed. No mTLS yet. |
| File backend | Mitigated for the test directory | Ordinary files only. Device paths are rejected. A corrupt file is a miss after restart. |
| LRU admission | Mitigated with a pin check | Unpinned committed blocks can be evicted. Pinned and quarantined blocks are not. After a missing `LRU` file, order falls back to ascending generation. |
| Partial prefix | Documented in ADR 0003 | Whole-block prefix only. A client that ignores `matched_prefix_blocks` can mis-align. Intra-block slice is a miss. |
| RustFS live probe | `NOT_RUN_HARDWARE` | Mock Put/Head/RangeGet/Delete passes. A mock is not a measured RustFS run. |
| No RDMA / DPU measurement | `NOT_RUN_HARDWARE` | Feature `rdma-nixl` does not link libnixl. Do not infer a speedup. GDS and BlueField stay in development. |
| Simulated registration treated as a NIC pin | Mitigated by labeling | `RegisteredPool` uses `Vec<u8>`. `shard-demo` prints `registration=SIMULATED`. |
| Shard directory treated as a replica | Mitigated by ADR 0005 | Membership fences fills. It does not copy payload bytes and it does not vote a quorum. |
| Cold refill over budget returned as a hit | Mitigated | `OverBudget` and `Missing` carry no payload. Checksum failure is an error. |
| License of a future DOCA or model stack | `NOT_RUN_LICENSE` | No DOCA binary and no model weights are in the tree. |
| GPU rental or DPU purchase | `NOT_RUN_BUDGET` | No paid API, GPU, or DPU was used. |
| Hot block loss | Accepted | No erasure coding and no durability replicas. The outcome is a clean miss, then recompute and repopulate. Cold durability is RustFS G4. See ADR 0002. |
| Short write or disk full reported as a durable hit | Mitigated | The in-memory commit is abandoned and the request id is forgotten. See `docs/V1C_TEST_MATRIX.md` |
| Hostile protocol bytes | Mitigated by decoder fuzz | `proptest` inside `cargo test`. Not a multi-hour libFuzzer run |
| Wrong model or tokenizer hit | Mitigated | Those identities miss. Comparison of synthetic tokens is exact. `tolerance=NONE` |
| Invented baseline numbers | Closed in this tree | `paired-baseline` and `v3-paired` print `NOT_RUN_HARDWARE` and contain no digits. TCO is `TCO_UNKNOWN` |
| Simulated DPU queue treated as BlueField | Mitigated by labels | `dpu-demo` prints `offload_active=false` and `NOT_RUN_HARDWARE`. Feature `dpu-bf3` links nothing |
| Hot-tier EC switched on through an ablation knob | Mitigated | `Ablation::set` rejects erasure coding. ADR 0002 |
| Integrity seal treated as a MAC | Documented | SHA-256 of a length-delimited buffer. Not an HMAC. ADR 0007 |
| Replay window eviction | Accepted | The oldest nonce can be reused after the cap. Not a permanent cache |
| Secret audit leaking a credential | Mitigated in tests | Events store kinds and generation ids, not fingerprints |
| Control-plane loss returning a stale hit | Mitigated on the simulated queue | Slots are hidden and new submits fail until `restore_control`. Old slots stay hidden |
| RustFS slowness blocking prefill | Mitigated in the hook | `refill_degraded` returns `Missing` and does not call the store |
| DOCA license and performance disclosure | `NOT_RUN_LICENSE` | No DOCA binary is linked. Review the actual EULA before linking or publishing numbers. See THIRD_PARTY_NOTICES.md |
| Calling this tree production-ready or PB-scale | Closed | Both labels are `NOT_CLAIMED` |

Minimum reproduction of the CPU contract is `cargo test --workspace --all-targets`. There is no hardware failure log because the hardware was not present.
