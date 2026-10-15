# V3 failure matrix

Each row is a clean miss. The engine recomputes and may fill again. The hook returns no payload, does not report a wrong hit. The decision flag is not proof of sustained write availability: hidden slots retain their byte charges. `StorageQueue::apply_fault` applies the DPU-local rows to the simulated slab. RustFS rows do not wipe the hot slab; `refill_degraded` returns `Missing` without calling the store.

| Fault | What the hook does | Result |
| --- | --- | --- |
| DPU restart | Drop in-flight tasks and drop slab slots | Clean miss. A new write can complete. Old slot ids are not readable |
| GPU reset | Hide every slot and drop in-flight tasks. Client completion is unknown, so bytes stay unreadable | Clean miss. New writes require remaining slab capacity |
| NIC flap | Same hide. An in-flight transfer is not a hit | Clean miss. Hidden bytes can exhaust the slab |
| Storage loss | Hide slots. Hidden bytes are not a durability replica and are not returned | Clean miss, then recompute |
| NVMe timeout | Drop one in-flight task. Do not expose it | Clean miss. Later submits still run |
| Control-plane loss | Hide slots and refuse new submits with `Cancelled` | No read while membership is unknown. `restore_control` permits admission if capacity remains. Old slots stay hidden and charged |
| RustFS slow | Do not wait and do not call the object store | `RefillOutcome::Missing` |
| RustFS unavailable | Same as slow | `RefillOutcome::Missing` |

Checksum mismatch hides the simulated slot after the failing read. A wrong tenant is `Unauthorized` and receives no bytes.

Lease expiry is still not proof that a copy stopped. The simulated slab does not free a slot only because a timer fired. Restart and the other faults above are explicit fencing events, not a lease callback.

This matrix is a CPU contract. It is not a fault campaign on BlueField, GPU, NIC, or RustFS hardware.

A full hidden slab has no drain/reclaim API yet. In this CPU simulation, an explicit DPU restart clears it. Do not translate that into permission to free real DMA buffers: hardware integration needs verified drain and device fencing. Pending write bytes and committed slab bytes share the configured byte cap.
