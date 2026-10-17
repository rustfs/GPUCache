# V3 failure matrix

Each row is a clean miss. The engine recomputes and may fill again. The hook returns no payload, does not report a wrong hit. The decision flag is not proof of sustained write availability: hidden slots retain their byte charges. `StorageQueue::apply_fault` applies the DPU-local rows to the simulated slab. RustFS rows do not wipe the hot slab; `refill_degraded` returns `Missing` without calling the store.

| Fault | What the hook does | Result |
| --- | --- | --- |
| DPU restart | Explicitly fence simulated tasks and drop slab slots; keep task/slot ids monotonic | Clean miss. Old ids are not reusable. A lost control-plane authority still requires `restore_control` |
| GPU reset | Hide every slot and quarantine in-flight tasks. Completion is unknown, so credits and bytes stay charged | Clean miss. New writes require remaining slab and queue capacity |
| NIC flap | Same hide. An in-flight transfer is not a hit | Clean miss. Hidden bytes can exhaust the slab |
| Storage loss | Hide slots. Hidden bytes are not a durability replica and are not returned | Clean miss, then recompute |
| NVMe timeout | Quarantine all pending tasks because the fault carries no task id; preserve healthy committed slots | No incomplete payload is exposed. Queue/byte credits remain charged until explicit drain |
| Control-plane loss | Hide slots and refuse new submits with `Cancelled` | No read while membership is unknown. `restore_control` permits admission if capacity remains. Old slots stay hidden and charged |
| RustFS slow | Do not wait and do not call the object store | `RefillOutcome::Missing` |
| RustFS unavailable | Same as slow | `RefillOutcome::Missing` |

Checksum mismatch hides the simulated slot after the failing read. A wrong tenant is `Unauthorized` and receives no bytes.

Lease expiry is still not proof that a copy stopped. The simulated slab does not free a slot only because a timer fired. Restart and the other faults above are explicit fencing events, not a lease callback.

This matrix is a CPU contract. It is not a fault campaign on BlueField, GPU, NIC, or RustFS hardware.

`quarantine` retains a task whose completion is unknown. `attest_drained` releases only a quarantined task after the trusted queue owner confirms drain; a late `complete` or `fail` cannot publish or free it. `invalidate` and `reclaim_invalidated` are tenant-scoped. Active or quarantined readers prevent slot reclaim. `fail` describes a confirmed failed completion, not an unknown timeout.

Pending writes reserve slab bytes and slot ids at admission. Task and slot metadata each have a hard ceiling of 4096. Pending/quarantined writes and committed/hidden slots share the configured byte cap. Returned payload vectors are caller-owned and are not a complete process-RSS bound.

These explicit CPU drain/reclaim operations and restart fences are not permission to free real DMA buffers. Hardware integration still needs verified transport quiescence and device fencing. Other faults and DPU restart do not silently restore lost control authority.
