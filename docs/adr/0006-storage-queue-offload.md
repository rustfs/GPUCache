# ADR 0006: The first offload candidate is the storage queue

- Status: **Accepted as a candidate, not as a measured win**
- Date: 2026-10-15
- Milestone: V3

## Context

V3 asks for one offload, with a real submit and complete path, on a storage-side BlueField-3. The GPU client keeps an ordinary ConnectX. Host GPUNetIO NIC-mode samples are a different topology and are not this daemon.

V2 did not produce host CPU, power, or GPU-interference numbers. Those rows are `NOT_RUN_HARDWARE`. The spec says not to widen a hardware design without bottleneck evidence. This change was explicitly authorized as a shippable slice without a BlueField, so the choice below is a hypothesis the paired experiment can accept or reject. It is not evidence that the bottleneck was measured.

Candidates:

| Candidate | Why it was not selected as the first offload |
| --- | --- |
| NIC / network processing | Ordinary ConnectX RDMA already bypasses host staging once NIXL is linked. Calling that a DPU win would mix two different claims. V2 left NIXL unlinked on purpose. |
| Local directory and quota | Real CPU work, and it stays on the portable control plane so a DPU restart cannot become the only copy of membership. Fencing already lives in `ShardDirectory`. |
| Crypto | Integrity seals in this tree are SHA-256 checksums, not a cipher the DPU must terminate. Crypto offload is an ablation knob, default off. |
| Compression, P2P, DPA | No measured byte savings or path. Ablation knobs only. |
| Erasure coding | Rejected for the hot tier. See [0002-no-erasure-coding.md](0002-no-erasure-coding.md). The knob cannot be turned on. |

The V2 hop list shows the storage side as a host copy into `FileStore` plus the kernel page cache. The spec's first honest DPU data path is a DRAM slab plus NVMe I/O, owned by the Arm complex next to the drives.

## Decision

The single candidate offload is the **storage queue**: submit a read or write against a bounded DRAM slab, then complete it.

- `ArmAgent::start` records that a process exists. `offload_active` stays false. Process start is not the offload.
- `StorageQueue::submit_write` / `submit_read` and `complete` are the task interface. A failed or timed-out task does not expose bytes.
- `open_simulated` copies inside this process and labels the completion `NOT_RUN_HARDWARE` and `simulated=true`.
- `open_nvme_device`, `open_doca`, and `open_spdk` return a storage error in every build, including `--features dpu-bf3`. The feature has no crate dependencies.
- A future SPDK backend must use an independent reactor. `AffineQueue` refuses a submit attributed to a different reactor id. Affine objects are not Tokio tasks and must not move across Tokio workers. This tree does not start SPDK or Tokio.
- P2P, DPA, compression, and crypto can be marked on. Their probe is `NOT_RUN_HARDWARE` and they do not change bytes. Hot-tier EC stays off.

Planned DPU mode, BFB, OS, DOCA, firmware, and IOMMU limits are listed in [../V3_DPU_TARGETS.md](../V3_DPU_TARGETS.md) as targets. Versions are unset. Nothing was flashed.

## Consequences

- `gpucache-dpu` depends only on `gpucache-core-types`. Transport, storage, protocol, and cold do not depend on it. The TCP hot path is unchanged.
- `dpu-demo` prints `status=DPU_CONTRACT_OK` only after the simulated round trip. The same output says `hardware=NOT_RUN_HARDWARE` and `offload_active=false`.
- A later measurement may reject this candidate. The portable TCP and file backends stay. A no-go does not upgrade hardware.
- BF4 and DOCA features beyond this queue are not claimed.
