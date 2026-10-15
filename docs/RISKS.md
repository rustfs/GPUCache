# Risks (V1 A)

| Risk | Status | Note |
| --- | --- | --- |
| Wrong-tenant or wrong-layout hit | Mitigated in `MemoryStore` tests | Physical key includes tenant and layout. Incompatible lookup returns a miss and no bytes. |
| Reclaim of a buffer that might still be copied | Mitigated by quarantine | Timeout and `KnownIdle` do not free a registered region. |
| Unbounded memory | Mitigated by quotas | Admission fails instead of evicting. Quarantine still holds bytes until fence or drain, which is intentional and counted against the quota. |
| Idempotency window expiry | Accepted | After TTL, a request id may run again. Callers must not reuse ids they still care about. |
| `Drained` is only tenant-authenticated | Accepted for V1 A | Not a device proof. Documented in SECURITY.md. |
| No real engine bytes | `NOT_RUN_HARDWARE` | Synthetic frames can pass while a real vLLM layout still mismatches. |
| No TCP, NVMe, or RustFS | Not started | V1 B. A mock must not be described as a measured RustFS run. |
| No RDMA / DPU measurement | `NOT_RUN_HARDWARE` | Do not infer a speedup from this milestone. |
| License of a future DOCA or model stack | `NOT_RUN_LICENSE` | No DOCA binary and no model weights are in the tree. |
| GPU rental or DPU purchase | `NOT_RUN_BUDGET` | No paid API, GPU, or DPU was used. |
| Hot block loss | Accepted | No erasure coding and no durability replicas. The outcome is a clean miss, then recompute and repopulate. Cold durability is RustFS G4. See ADR 0002. |

Minimum reproduction of the CPU contract is `cargo test --workspace --all-targets`. There is no hardware failure log because the hardware was not present.
