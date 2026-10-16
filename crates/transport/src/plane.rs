// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Rebalance, rolling upgrade, and fault-domain isolation on top of one shard.
//!
//! These paths fence in-flight fills. They do not copy payload bytes and they
//! do not add a replica. A fenced fill is a miss. The engine recomputes.

use std::collections::BTreeMap;

use gpucache_core_types::CacheError;

use crate::directory::{Admit, FillNotice, FillRequest, ShardDirectory, ShardParams};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Serving,
    Draining,
}

pub struct RebalancePlan {
    pub from_shard: u64,
    pub to_shard: u64,
    pub source_epoch: u64,
    pub destination: ShardDirectory,
}

pub struct UpgradeStamp {
    pub generation: u64,
    pub epoch: u64,
    pub fenced: bool,
}

pub struct ControlPlane {
    dir: ShardDirectory,
    params: ShardParams,
    domains: BTreeMap<u64, u16>,
    phase: Phase,
    software_generation: u64,
    upgraded: bool,
}

impl ControlPlane {
    pub fn single_node(params: ShardParams) -> Self {
        Self {
            dir: ShardDirectory::single_node(params.clone()),
            params,
            domains: BTreeMap::new(),
            phase: Phase::Serving,
            software_generation: 1,
            upgraded: false,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.dir.epoch()
    }

    pub fn inflight(&self) -> usize {
        self.dir.inflight()
    }

    pub fn credits_used(&self) -> u64 {
        self.dir.credits_used()
    }

    pub fn software_generation(&self) -> u64 {
        self.software_generation
    }

    pub fn serving(&self) -> bool {
        self.phase == Phase::Serving
    }

    pub fn begin_or_join(&mut self, req: &FillRequest) -> Result<Admit, CacheError> {
        if self.phase != Phase::Serving {
            return Err(CacheError::Cancelled);
        }
        self.dir.begin_or_join(req)
    }

    pub fn complete_flight(
        &mut self,
        flight_id: u64,
        request_epoch: u64,
        request: [u8; 16],
    ) -> Result<(), CacheError> {
        self.dir.complete_flight(flight_id, request_epoch, request)
    }

    pub fn fill_notice(&self, tenant: &[u8], request: [u8; 16]) -> Result<FillNotice, CacheError> {
        self.dir.fill_notice(tenant, request)
    }

    pub fn assign_domain(&mut self, node_id: u64, domain: u16) -> Result<(), CacheError> {
        if !self
            .dir
            .members()
            .iter()
            .any(|member| member.node_id == node_id)
        {
            return Err(CacheError::Malformed {
                reason: "fault domain",
            });
        }
        self.domains.insert(node_id, domain);
        Ok(())
    }

    /// Remove every member in `domain`. The last remaining member stays.
    /// In-flight fills are fenced and become misses. No payload is copied.
    pub fn isolate_domain(&mut self, domain: u16) -> Result<u64, CacheError> {
        let victims: Vec<u64> = self
            .dir
            .members()
            .iter()
            .filter(|member| self.domains.get(&member.node_id) == Some(&domain))
            .map(|member| member.node_id)
            .collect();
        if victims.is_empty() {
            return Err(CacheError::Malformed {
                reason: "fault domain",
            });
        }
        if victims.len() >= self.dir.members().len() {
            return Err(CacheError::FencingRejected { reason: "member" });
        }
        let mut epoch = self.dir.epoch();
        for node_id in victims {
            epoch = self.dir.remove_member(node_id)?;
            self.domains.remove(&node_id);
        }
        Ok(epoch)
    }

    /// Fence this shard and hand back an empty directory for `to_shard`.
    /// The destination has no flights. That is a miss, not a moved hit.
    pub fn rebalance_to(&mut self, to_shard: u64) -> Result<RebalancePlan, CacheError> {
        if self.phase != Phase::Serving {
            return Err(CacheError::Cancelled);
        }
        if to_shard == self.dir.shard_id() {
            return Err(CacheError::Malformed {
                reason: "rebalance",
            });
        }
        let source_epoch = self.dir.fence_epoch()?;
        let destination = ShardDirectory::open(to_shard, self.params.clone());
        Ok(RebalancePlan {
            from_shard: self.dir.shard_id(),
            to_shard,
            source_epoch,
            destination,
        })
    }

    pub fn begin_upgrade(&mut self) -> Result<(), CacheError> {
        if self.phase != Phase::Serving {
            return Err(CacheError::Malformed { reason: "upgrade" });
        }
        self.phase = Phase::Draining;
        Ok(())
    }

    pub fn finish_upgrade(&mut self) -> Result<UpgradeStamp, CacheError> {
        if self.phase != Phase::Draining {
            return Err(CacheError::Malformed { reason: "upgrade" });
        }
        let epoch = self.dir.fence_epoch()?;
        self.software_generation = self.software_generation.saturating_add(1);
        self.upgraded = true;
        self.phase = Phase::Serving;
        Ok(UpgradeStamp {
            generation: self.software_generation,
            epoch,
            fenced: true,
        })
    }

    /// From `Draining`, resume serving without a fence so an in-flight fill
    /// can still complete. After `finish_upgrade`, fence again so a writer
    /// on the old epoch misses. The generation counter does not go backwards.
    pub fn rollback(&mut self) -> Result<UpgradeStamp, CacheError> {
        if self.phase == Phase::Draining {
            self.phase = Phase::Serving;
            return Ok(UpgradeStamp {
                generation: self.software_generation,
                epoch: self.dir.epoch(),
                fenced: false,
            });
        }
        if !self.upgraded {
            return Err(CacheError::Malformed { reason: "upgrade" });
        }
        let epoch = self.dir.fence_epoch()?;
        Ok(UpgradeStamp {
            generation: self.software_generation,
            epoch,
            fenced: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ShardParams {
        ShardParams::new(100, 80, 4, 2, 4).unwrap()
    }

    fn fill(epoch: u64) -> FillRequest {
        FillRequest {
            physical_key: b"phys-a".to_vec(),
            tenant: b"tenant-a".to_vec(),
            bytes: 10,
            request: 1u128.to_le_bytes(),
            epoch,
        }
    }

    #[test]
    fn rebalance_fences_the_source_and_starts_an_empty_destination() {
        let mut plane = ControlPlane::single_node(params());
        let admit = plane.begin_or_join(&fill(1)).unwrap();
        let Admit::Leader { flight_id, epoch } = admit else {
            panic!("leader");
        };
        let plan = plane.rebalance_to(2).unwrap();
        assert_eq!(plan.from_shard, 0);
        assert_eq!(plan.to_shard, 2);
        assert_eq!(plan.destination.shard_id(), 2);
        assert_eq!(plan.destination.inflight(), 0);
        assert_eq!(plane.inflight(), 0);
        assert_eq!(plane.credits_used(), 0);
        let err = plane
            .complete_flight(flight_id, epoch, 1u128.to_le_bytes())
            .unwrap_err();
        assert!(matches!(err, CacheError::EpochMismatch { .. }));
        assert_eq!(plan.source_epoch, plane.epoch());
        assert_eq!(
            plane.fill_notice(b"tenant-a", 1u128.to_le_bytes()).unwrap(),
            FillNotice::Fenced
        );
    }

    #[test]
    fn rolling_upgrade_rejects_new_work_and_rollback_before_finish_keeps_the_fill() {
        let mut plane = ControlPlane::single_node(params());
        let admit = plane.begin_or_join(&fill(1)).unwrap();
        let Admit::Leader { flight_id, .. } = admit else {
            panic!("leader");
        };
        plane.begin_upgrade().unwrap();
        assert_eq!(
            plane.begin_or_join(&fill(1)).unwrap_err(),
            CacheError::Cancelled
        );
        let stamp = plane.rollback().unwrap();
        assert!(!stamp.fenced);
        assert!(plane.serving());
        plane
            .complete_flight(flight_id, 1, 1u128.to_le_bytes())
            .unwrap();
        assert_eq!(plane.inflight(), 0);
    }

    #[test]
    fn finish_upgrade_fences_and_a_later_rollback_fences_again() {
        let mut plane = ControlPlane::single_node(params());
        plane.begin_or_join(&fill(1)).unwrap();
        plane.begin_upgrade().unwrap();
        let finished = plane.finish_upgrade().unwrap();
        assert!(finished.fenced);
        assert_eq!(finished.generation, 2);
        assert_eq!(plane.inflight(), 0);
        plane.begin_or_join(&fill(plane.epoch())).unwrap();
        let rolled = plane.rollback().unwrap();
        assert!(rolled.fenced);
        assert_eq!(rolled.generation, 2);
        assert_eq!(plane.inflight(), 0);
        assert_eq!(plane.credits_used(), 0);
    }

    #[test]
    fn isolate_domain_refuses_to_remove_the_last_member() {
        let mut plane = ControlPlane::single_node(params());
        let node = plane.dir.members()[0].node_id;
        plane.assign_domain(node, 7).unwrap();
        plane.begin_or_join(&fill(1)).unwrap();
        let err = plane.isolate_domain(7).unwrap_err();
        assert_eq!(err, CacheError::FencingRejected { reason: "member" });
        assert_eq!(plane.inflight(), 1);
        assert_eq!(plane.epoch(), 1);
        plane.assign_domain(node, 1).unwrap();
        plane.dir.note_member(9, 1).unwrap();
        plane.assign_domain(9, 7).unwrap();
        let epoch = plane.isolate_domain(7).unwrap();
        assert!(epoch > 1);
        assert_eq!(plane.inflight(), 0);
        assert!(plane.dir.members().iter().all(|member| member.node_id != 9));
    }
}
