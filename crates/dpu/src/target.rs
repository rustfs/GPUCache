// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Planned BlueField-3 install targets. None of these versions were measured
//! or flashed by this tree. Host GPUNetIO NIC-mode samples are not this daemon.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannedTarget {
    pub field: &'static str,
    pub value: &'static str,
    pub measured: &'static str,
}

pub fn planned_targets() -> &'static [PlannedTarget] {
    const NOT_RUN: &str = "NOT_RUN_HARDWARE";
    &[
        PlannedTarget {
            field: "topology",
            value: "storage-side BlueField-3; the GPU client keeps an ordinary ConnectX",
            measured: NOT_RUN,
        },
        PlannedTarget {
            field: "not_this_daemon",
            value: "host GPUNetIO NIC-mode samples are not a DPU Arm storage daemon",
            measured: NOT_RUN,
        },
        PlannedTarget {
            field: "dpu_mode",
            value: "target: DPU mode so the Arm complex owns the NVMe namespaces; mode unset",
            measured: NOT_RUN,
        },
        PlannedTarget {
            field: "bfb",
            value: "target: BlueField BFB matched to that mode; version unset",
            measured: NOT_RUN,
        },
        PlannedTarget {
            field: "os",
            value: "target: Arm OS image from that BFB; version unset",
            measured: NOT_RUN,
        },
        PlannedTarget {
            field: "doca",
            value: "target: DOCA SDK matched to that BFB; version unset; not linked",
            measured: NOT_RUN,
        },
        PlannedTarget {
            field: "firmware",
            value: "target: BF3 firmware matched to that BFB; version unset",
            measured: NOT_RUN,
        },
        PlannedTarget {
            field: "iommu",
            value: "target: record IOMMU or SMMU, BAR, and registration limits at install; this tree does not change them",
            measured: NOT_RUN,
        },
        PlannedTarget {
            field: "data_path",
            value: "first honest path: DPU DRAM slab plus NVMe I/O; simulated in-process here",
            measured: NOT_RUN,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_unmeasured() {
        let rows = planned_targets();
        assert!(rows.len() >= 8);
        assert!(rows.iter().all(|row| row.measured == "NOT_RUN_HARDWARE"));
        assert!(rows.iter().any(|row| row.field == "not_this_daemon"));
        let text = rows
            .iter()
            .map(|row| row.value)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!text.contains("already implemented"));
        assert!(!text.contains("deployed"));
    }
}
