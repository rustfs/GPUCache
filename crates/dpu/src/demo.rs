// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! CPU contract for the storage-queue interface.
//!
//! Success is `status=DPU_CONTRACT_OK`. That line means the in-process
//! simulation matched. It does not mean a BlueField, DOCA, or NVMe device ran.

use core::fmt;

use gpucache_core_types::CacheError;

use crate::ablation::{Ablation, Knob};
use crate::queue::{
    dpu_feature_selected, open_doca, open_nvme_device, open_spdk, ArmAgent, StorageQueue,
};
use crate::target::planned_targets;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DpuReport {
    pub feature: &'static str,
}

impl fmt::Display for DpuReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "status=DPU_CONTRACT_OK")?;
        writeln!(f, "arm_process=STARTED")?;
        writeln!(f, "offload_active=false")?;
        writeln!(f, "offload=STORAGE_QUEUE")?;
        writeln!(f, "hardware=NOT_RUN_HARDWARE")?;
        writeln!(f, "simulated_complete=INTERFACE_ONLY")?;
        writeln!(f, "nvme=NOT_LINKED")?;
        writeln!(f, "doca=NOT_LINKED")?;
        writeln!(f, "spdk=NOT_LINKED")?;
        writeln!(f, "gds=NOT_RUN_HARDWARE")?;
        writeln!(f, "ec_hot_tier=OFF")?;
        writeln!(f, "p2p=NOT_RUN_HARDWARE")?;
        writeln!(f, "dpa=OFF")?;
        writeln!(f, "compression=OFF")?;
        writeln!(f, "crypto_offload=NOT_RUN_HARDWARE")?;
        writeln!(f, "feature_dpu_bf3={}", self.feature)?;
        writeln!(f, "topology=STORAGE_SIDE_BF3_TARGET")?;
        writeln!(f, "client_nic=CONNECTX_ORDINARY_TARGET")?;
        writeln!(f, "gpunetio_nic_mode=NOT_THIS_DAEMON")?;
        for row in planned_targets() {
            writeln!(f, "target_{}=NOT_RUN_HARDWARE", row.field)?;
        }
        Ok(())
    }
}

pub fn run_dpu_demo() -> Result<DpuReport, CacheError> {
    let agent = ArmAgent::start();
    if !agent.process_started() || agent.offload_active() {
        return Err(CacheError::Storage {
            reason: "dpu agent",
        });
    }
    let mut queue = StorageQueue::open_simulated(4096, 4)?;
    let write = queue.submit_write(b"tenant-a", b"kv-demo")?;
    let wrote = queue.complete(write)?;
    if !wrote.simulated || wrote.offload_active || wrote.hardware != "NOT_RUN_HARDWARE" {
        return Err(CacheError::Storage {
            reason: "dpu agent",
        });
    }
    let slot = wrote.slot.ok_or(CacheError::Storage {
        reason: "dpu agent",
    })?;
    let read = queue.submit_read(b"tenant-a", slot)?;
    let got = queue.complete(read)?;
    if got.payload.as_deref() != Some(b"kv-demo".as_slice()) || got.offload_active {
        return Err(CacheError::ChecksumMismatch);
    }
    if open_nvme_device().is_ok() || open_doca().is_ok() || open_spdk().is_ok() {
        return Err(CacheError::Storage {
            reason: "doca not linked",
        });
    }
    let mut ablation = Ablation::new();
    ablation.set(Knob::P2p, true)?;
    ablation.set(Knob::Crypto, true)?;
    if ablation.probe(Knob::P2p) != "NOT_RUN_HARDWARE"
        || ablation.probe(Knob::ErasureCoding) != "OFF"
        || ablation.set(Knob::ErasureCoding, true).is_ok()
    {
        return Err(CacheError::Malformed {
            reason: "hot tier ec",
        });
    }
    let feature = if dpu_feature_selected() {
        "SELECTED_NOT_LINKED"
    } else {
        "UNSELECTED"
    };
    Ok(DpuReport { feature })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_does_not_claim_a_device() {
        let text = run_dpu_demo().unwrap().to_string();
        assert!(text.contains("status=DPU_CONTRACT_OK"));
        assert!(text.contains("offload_active=false"));
        assert!(text.contains("hardware=NOT_RUN_HARDWARE"));
        assert!(text.contains("nvme=NOT_LINKED"));
        assert!(text.contains("doca=NOT_LINKED"));
        assert!(text.contains("ec_hot_tier=OFF"));
        assert!(text.contains("gpunetio_nic_mode=NOT_THIS_DAEMON"));
        assert!(!text.contains("production ready"));
        assert!(!text.contains("PB-scale"));
    }
}
