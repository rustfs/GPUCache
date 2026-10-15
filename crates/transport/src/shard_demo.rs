// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Single-shard fencing, simulated registration, and the TCP staging path.
//!
//! One process is shard 0. RDMA is not opened. No timing is printed.

use core::fmt;

use gpucache_core_types::CacheError;

use crate::directory::{Admit, FillRequest, ShardDirectory, ShardParams};
use crate::iface::{rdma_probe, NixlTransport, TcpTransport};
use crate::registered::{RegisteredPool, SimCompletion};
use crate::stage::stage_through_tcp;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardReport;

impl fmt::Display for ShardReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "status=SHARD_OK")?;
        writeln!(f, "topology=SINGLE_SHARD")?;
        writeln!(f, "flight=JOINED")?;
        writeln!(f, "credits=SINGLE_CHARGE")?;
        writeln!(f, "stale_epoch=FENCED")?;
        writeln!(f, "unknown_completion=QUARANTINED")?;
        writeln!(f, "lease_expiry=HELD")?;
        writeln!(f, "tcp_path=HOST_COPY")?;
        writeln!(f, "dma_posted=NO")?;
        writeln!(f, "rdma={rdma}", rdma = rdma_probe())?;
        writeln!(f, "registration=SIMULATED")?;
        Ok(())
    }
}

pub fn run_shard_demo() -> Result<ShardReport, CacheError> {
    let params = ShardParams::new(1024, 1024, 8, 4, 4)?;
    let mut dir = ShardDirectory::single_node(params);
    if dir.shard_id() != 0 || dir.members().len() != 1 {
        return Err(CacheError::Malformed {
            reason: "shard config",
        });
    }
    let leader_req = FillRequest {
        physical_key: b"phys-demo".to_vec(),
        tenant: b"tenant-a".to_vec(),
        bytes: 32,
        request: 1u128.to_le_bytes(),
        epoch: dir.epoch(),
    };
    let joined_req = FillRequest {
        request: 2u128.to_le_bytes(),
        ..leader_req.clone()
    };
    let leader = dir.begin_or_join(&leader_req)?;
    let joined = dir.begin_or_join(&joined_req)?;
    if !matches!(leader, Admit::Leader { .. }) || !matches!(joined, Admit::Joined { .. }) {
        return Err(CacheError::Malformed {
            reason: "shard fill",
        });
    }
    if dir.credits_used() != 32 || dir.inflight() != 1 {
        return Err(CacheError::Malformed { reason: "credits" });
    }
    let Admit::Leader { flight_id, epoch } = leader else {
        return Err(CacheError::Malformed {
            reason: "shard fill",
        });
    };
    let bumped = dir.note_member(dir.members()[0].node_id, 2)?;
    if bumped <= epoch || dir.inflight() != 0 || dir.credits_used() != 0 {
        return Err(CacheError::FencingRejected { reason: "epoch" });
    }
    if dir
        .complete_flight(flight_id, epoch, leader_req.request)
        .is_ok()
    {
        return Err(CacheError::FencingRejected { reason: "epoch" });
    }

    let mut pool = RegisteredPool::new(1, 16)?;
    let batch = pool.checkout_batch(1)?;
    let id = batch.slots()[0];
    pool.complete_batch(&batch, SimCompletion::Unknown)?;
    pool.on_lease_expiry(id)?;
    if pool.stats().quarantined != 1 || pool.stats().idle_unbound != 0 {
        return Err(CacheError::NotReclaimable {
            from: gpucache_core_types::BlockState::Quarantined,
        });
    }

    let mut tcp = TcpTransport::new(64)?;
    let mut stage_pool = RegisteredPool::new(1, 16)?;
    let receipt = stage_through_tcp(&mut stage_pool, &mut tcp, &[b"stage"])?;
    if receipt.dma_posted || receipt.host_copies != 1 {
        return Err(CacheError::Malformed { reason: "transfer" });
    }
    if NixlTransport::open().is_ok() || rdma_probe() != "NOT_RUN_HARDWARE" {
        return Err(CacheError::Storage {
            reason: "rdma not linked",
        });
    }
    Ok(ShardReport)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_report_has_no_device_claim() {
        let text = run_shard_demo().unwrap().to_string();
        assert!(text.contains("status=SHARD_OK"));
        assert!(text.contains("topology=SINGLE_SHARD"));
        assert!(text.contains("rdma=NOT_RUN_HARDWARE"));
        assert!(text.contains("registration=SIMULATED"));
        assert!(text.contains("dma_posted=NO"));
        assert!(!text.contains("GB/s"));
        assert!(!text.contains("TTFT"));
    }
}
