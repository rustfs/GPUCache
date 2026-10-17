# V3 test matrix

Command for the CPU rows:

```bash
cargo test --workspace --all-targets
cargo test -p gpucache-dpu --all-targets --features dpu-bf3
cargo run -p gpucache-dpu --bin dpu-demo
cargo run -p gpucache-bench --bin v3-paired
```

Default features do not need DOCA, SPDK, libnixl, or a DPU. `--features dpu-bf3` still refuses `open_doca`, `open_spdk`, and `open_nvme_device`.

`test-aarch64` and `test-aarch64-qemu` in CI remain CPU runs. This slice adds no sleeps and no threads, so it does not add a qemu-user timer race. An aarch64 pass is not a BlueField pass. Qemu-user is not a performance run.

| Check | Result in this tree | Label |
| --- | --- | --- |
| `dpu-demo` simulated round trip, devices closed, EC off | Expected `DPU_CONTRACT_OK` with `offload_active=false` | CPU contract. Hardware `NOT_RUN_HARDWARE` |
| Storage-queue submit, complete, fail, wrong tenant, checksum flip | Unit tests in `gpucache-dpu` | CPU contract |
| Unknown completion, late completion, repeated faults, explicit drain, tenant invalidate/reclaim and referenced slots | Deterministic storage-queue regression tests; unknown tasks retain credit | CPU contract, not hardware DMA quiescence |
| Task/slot ceilings, pending slot-id reservation, restart id fencing, control-loss preservation | Storage-queue regression tests | CPU contract |
| Arm `start` does not set offload | Unit test | CPU contract |
| Reactor id mismatch | `AffineQueue` unit test | Contract only. SPDK `NOT_RUN_HARDWARE` |
| Ablation knobs and hot-tier EC refusal | Unit test | EC stays off |
| Fault matrix | `StorageQueue::apply_fault` tests | CPU contract. Hardware faults `NOT_RUN_HARDWARE` |
| ACL, replay, seal, secret rotation, audit contents | `gpucache-protocol` tests | CPU contract. mTLS `NOT_RUN` (not linked) |
| Replay capacity/clock rewind/domain isolation, retired secret revival, redacted credentials and malformed identities | Protocol and core-type regression tests | CPU hooks; not a TCP security rollout |
| TCP response operation, epoch, item count/order and request id association | Fake-peer regression tests; malformed exchanges poison the connection | Real localhost TCP with synthetic bytes |
| Rate limit and noisy neighbor | `RateGate` tests | CPU contract. Caller clock, no sleep |
| Rebalance, rolling upgrade, rollback, fault domain | `ControlPlane` tests | CPU contract. No payload copy |
| G4 retention, verified delete, cold key retire, backfill cap, slow RustFS | `gpucache-cold` tests | Mock `PASS`. Live RustFS `NOT_RUN_HARDWARE` |
| Unknown manifest version | `read_manifest` test | Rejected. Not a hit |
| V3 paired report | No digits. NIC RDMA required. `TCO_UNKNOWN`. `go_gate=NOT_EVALUATED` | `NOT_RUN_HARDWARE` |
| Real BF3 / DOCA / SPDK / NVMe device | Not present | `NOT_RUN_HARDWARE` |
| DOCA license review before any linkage or perf publication | Not done | `NOT_RUN_LICENSE` |
| 24 hour soak, wall power, PB-scale | Not run | `NOT_RUN_HARDWARE` / `NOT_RUN_BUDGET` |
| vLLM or SGLang KV round trip | Not run | `NOT_RUN_HARDWARE` |

The [VM runner](VM_REGRESSION.md) executes the required CPU gates and writes versioned JSON, JUnit and logs. Its own tests cover command failure, timeouts, descendant cleanup, exact output contracts, dependency-boundary bypasses and hardware-profile refusal. CPU regression forces the live RustFS endpoint empty, including overrides from Cargo configuration.

`status=DPU_CONTRACT_OK` means the simulation matched. It is not a device pass and not `production ready`.
