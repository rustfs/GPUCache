# V3 DPU targets

These rows are install targets for a future storage-side BlueField-3. They are not measurements. This repository did not purchase a DPU, flash a BFB, change IOMMU or ACS, format NVMe, or link DOCA. `planned_targets()` in `gpucache-dpu` prints the same fields with `measured=NOT_RUN_HARDWARE`.

| Field | Target | Recorded value |
| --- | --- | --- |
| Topology | Storage-side BlueField-3 next to the NVMe. The GPU host keeps an ordinary ConnectX. | Not assembled. `NOT_RUN_HARDWARE` |
| What this daemon is not | An Arm process on the DPU that serves the DRAM slab and the NVMe queue | Host GPUNetIO NIC-mode samples are not this process. Do not label them as the storage daemon |
| DPU mode | DPU mode, so the embedded CPU owns the NVMe namespaces used by the cache | Unset. This tree does not switch modes |
| BFB | The BlueField boot image paired with that mode | Version unset |
| OS | The Arm OS from that BFB | Version unset |
| DOCA | The DOCA SDK paired with that BFB | Version unset. Not linked. Feature `dpu-bf3` is an empty Cargo feature |
| Firmware | The BF3 firmware paired with that BFB | Version unset |
| IOMMU / BAR / registration | Limits written down at install time | Unset. This tree does not change BIOS, IOMMU, or ACS |
| First data path | DRAM slab plus NVMe submit/complete | In-process simulation only. `open_nvme_device` returns `nvme not linked` |
| SPDK, if it is ever linked | One reactor thread per affine queue. The queue is not a Tokio task and is not moved across workers | Not linked. `AffineQueue::submit_on` checks the reactor id only |
| Soft-RoCE | Functional smoke on a developer hypervisor, if someone runs it later | Not run here. Not a performance result. Not the V3 baseline |

An aarch64 `cargo test` is a CPU build of the same Rust core. It does not execute this table.

Public wording: BlueField-3, GDS, and DOCA are **in development / `NOT_RUN_HARDWARE` / optional feature**. They are not already implemented and not deployed.
