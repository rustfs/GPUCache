// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Rebalance, rolling upgrade, and fault-domain isolation on top of one shard.
//!
//! These paths fence in-flight fills. They do not copy payload bytes and they
//! do not add a replica. A fenced fill is a miss. The engine recomputes.

use std::collections::BTreeMap;

use gpucache_core_types::{CacheError, QuotaKind};

use crate::directory::{Admit, FillNotice, FillRequest, MemberView, ShardDirectory, ShardParams};

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

    /// Apply one complete membership view through the directory's atomic fence.
    /// Failed validation or epoch exhaustion keeps fault-domain assignments and
    /// pending fills intact. Draining upgrades do not accept membership changes.
    pub fn apply_membership(&mut self, members: &[MemberView]) -> Result<u64, CacheError> {
        if self.phase != Phase::Serving {
            return Err(CacheError::Cancelled);
        }
        let epoch = self.dir.apply_membership(members)?;
        self.domains
            .retain(|node, _| members.iter().any(|member| member.node_id == *node));
        Ok(epoch)
    }

    /// Remove every member in `domain`. The last remaining member stays.
    /// In-flight fills are fenced and become misses. No payload is copied.
    /// If the epoch cannot advance for the entire batch, nothing is changed.
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
        // Each removal fences once. Reserve the entire epoch range before
        // changing membership, domains, credits, or any in-flight notice.
        self.dir
            .epoch()
            .checked_add(victims.len() as u64)
            .ok_or(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests,
            })?;
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
        let generation =
            self.software_generation
                .checked_add(1)
                .ok_or(CacheError::FencingRejected {
                    reason: "software generation exhausted",
                })?;
        let epoch = self.dir.fence_epoch()?;
        self.software_generation = generation;
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

    fn three_member_plane(epoch: u64) -> ControlPlane {
        let mut plane = ControlPlane::single_node(params());
        plane.dir.note_member(2, 1).unwrap();
        plane.dir.note_member(3, 1).unwrap();
        plane.assign_domain(1, 7).unwrap();
        plane.assign_domain(2, 7).unwrap();
        plane.assign_domain(3, 9).unwrap();
        plane.dir.set_epoch_for_test(epoch);
        plane
    }

    fn begin_leader_and_waiter(plane: &mut ControlPlane) -> (u64, FillRequest, FillRequest) {
        let leader = fill(plane.epoch());
        let Admit::Leader { flight_id, .. } = plane.begin_or_join(&leader).unwrap() else {
            panic!("leader");
        };
        let mut waiter = leader.clone();
        waiter.request = 2u128.to_le_bytes();
        assert_eq!(
            plane.begin_or_join(&waiter).unwrap(),
            Admit::Joined {
                flight_id,
                epoch: plane.epoch()
            }
        );
        (flight_id, leader, waiter)
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
    fn exhausted_software_generation_does_not_fence_or_resume_a_draining_shard() {
        let mut plane = ControlPlane::single_node(params());
        let (flight_id, leader, waiter) = begin_leader_and_waiter(&mut plane);
        plane.software_generation = u64::MAX;
        plane.begin_upgrade().unwrap();
        assert_eq!(
            plane.finish_upgrade().err(),
            Some(CacheError::FencingRejected {
                reason: "software generation exhausted"
            })
        );
        assert!(!plane.serving());
        assert_eq!(plane.epoch(), leader.epoch);
        assert_eq!(plane.inflight(), 1);
        assert_eq!(plane.credits_used(), leader.bytes);
        for request in [&leader, &waiter] {
            assert_eq!(
                plane.fill_notice(&request.tenant, request.request).unwrap(),
                FillNotice::Pending
            );
        }
        plane.rollback().unwrap();
        plane
            .complete_flight(flight_id, leader.epoch, leader.request)
            .unwrap();
    }

    #[test]
    fn isolate_domain_refuses_to_remove_the_last_member() {
        let mut plane = ControlPlane::single_node(params());
        let node = plane.dir.members()[0].node_id;
        plane.assign_domain(node, 7).unwrap();
        plane.begin_or_join(&fill(1)).unwrap();
        assert_eq!(
            plane.isolate_domain(99).unwrap_err(),
            CacheError::Malformed {
                reason: "fault domain"
            }
        );
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

    #[test]
    fn public_membership_admission_enables_fault_domain_isolation_and_refuses_during_upgrade() {
        let mut plane = ControlPlane::single_node(params());
        let members = [
            MemberView {
                node_id: 1,
                incarnation: 1,
            },
            MemberView {
                node_id: 2,
                incarnation: 1,
            },
        ];
        plane.apply_membership(&members).unwrap();
        plane.assign_domain(1, 7).unwrap();
        plane.assign_domain(2, 9).unwrap();
        let (flight_id, leader, _) = begin_leader_and_waiter(&mut plane);
        plane.begin_upgrade().unwrap();
        assert_eq!(
            plane.apply_membership(&members[..1]),
            Err(CacheError::Cancelled)
        );
        assert_eq!(plane.inflight(), 1);
        plane.rollback().unwrap();
        let epoch = plane.isolate_domain(9).unwrap();
        assert!(epoch > leader.epoch);
        assert_eq!(plane.domains, BTreeMap::from([(1, 7)]));
        assert_eq!(plane.credits_used(), 0);
        assert!(matches!(
            plane.complete_flight(flight_id, leader.epoch, leader.request),
            Err(CacheError::EpochMismatch { .. })
        ));
    }

    #[test]
    fn failed_membership_batch_retains_fault_domains_and_pending_fills() {
        let mut plane = three_member_plane(u64::MAX);
        let (_, leader, waiter) = begin_leader_and_waiter(&mut plane);
        let domains = plane.domains.clone();
        let members = plane.dir.members().to_vec();
        assert!(plane.apply_membership(&members[..1]).is_err());
        assert_eq!(plane.dir.members(), members);
        assert_eq!(plane.domains, domains);
        assert_eq!(plane.credits_used(), leader.bytes);
        for request in [&leader, &waiter] {
            assert_eq!(
                plane.fill_notice(&request.tenant, request.request).unwrap(),
                FillNotice::Pending
            );
        }
    }

    #[test]
    fn isolate_domain_epoch_shortage_keeps_the_entire_batch_and_fills_unchanged() {
        let mut plane = three_member_plane(u64::MAX - 1);
        let (flight_id, leader, waiter) = begin_leader_and_waiter(&mut plane);
        let members = plane.dir.members().to_vec();
        let domains = plane.domains.clone();

        assert_eq!(
            plane.isolate_domain(7).unwrap_err(),
            CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests
            }
        );
        assert_eq!(plane.epoch(), u64::MAX - 1);
        assert_eq!(plane.dir.members(), members);
        assert_eq!(plane.domains, domains);
        assert_eq!(plane.inflight(), 1);
        assert_eq!(plane.credits_used(), leader.bytes);
        assert!(plane.serving());
        for request in [&leader, &waiter] {
            assert_eq!(
                plane.fill_notice(&request.tenant, request.request).unwrap(),
                FillNotice::Pending
            );
        }
        assert_eq!(
            plane.begin_or_join(&leader).unwrap(),
            Admit::Leader {
                flight_id,
                epoch: leader.epoch
            }
        );
        assert_eq!(
            plane.begin_or_join(&waiter).unwrap(),
            Admit::Joined {
                flight_id,
                epoch: waiter.epoch
            }
        );
        assert_eq!(plane.credits_used(), leader.bytes);
        plane
            .complete_flight(flight_id, leader.epoch, leader.request)
            .unwrap();
        assert_eq!(plane.credits_used(), 0);
        assert_eq!(plane.inflight(), 0);
    }

    #[test]
    fn isolate_domain_with_exact_epoch_budget_removes_every_victim_and_fences_fills() {
        let mut plane = three_member_plane(u64::MAX - 2);
        let (flight_id, leader, waiter) = begin_leader_and_waiter(&mut plane);

        assert_eq!(plane.isolate_domain(7).unwrap(), u64::MAX);
        assert_eq!(plane.epoch(), u64::MAX);
        assert_eq!(plane.dir.members().len(), 1);
        assert_eq!(plane.dir.members()[0].node_id, 3);
        assert_eq!(plane.domains, BTreeMap::from([(3, 9)]));
        assert_eq!(plane.inflight(), 0);
        assert_eq!(plane.credits_used(), 0);
        assert!(plane.serving());
        for request in [&leader, &waiter] {
            assert_eq!(
                plane.fill_notice(&request.tenant, request.request).unwrap(),
                FillNotice::Fenced
            );
        }
        assert_eq!(
            plane
                .complete_flight(flight_id, leader.epoch, leader.request)
                .unwrap_err(),
            CacheError::EpochMismatch {
                store: u64::MAX,
                request: leader.epoch
            }
        );
    }

    #[test]
    fn isolate_domain_at_epoch_max_keeps_a_single_victim_and_current_fills() {
        let mut plane = three_member_plane(u64::MAX);
        plane.assign_domain(2, 9).unwrap();
        let (flight_id, leader, waiter) = begin_leader_and_waiter(&mut plane);
        let members = plane.dir.members().to_vec();
        let domains = plane.domains.clone();

        assert_eq!(
            plane.isolate_domain(7).unwrap_err(),
            CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests
            }
        );
        assert_eq!(plane.epoch(), u64::MAX);
        assert_eq!(plane.dir.members(), members);
        assert_eq!(plane.domains, domains);
        assert_eq!(plane.inflight(), 1);
        assert_eq!(plane.credits_used(), leader.bytes);
        for request in [&leader, &waiter] {
            assert_eq!(
                plane.fill_notice(&request.tenant, request.request).unwrap(),
                FillNotice::Pending
            );
        }
        plane
            .complete_flight(flight_id, leader.epoch, leader.request)
            .unwrap();
        assert_eq!(plane.inflight(), 0);
        assert_eq!(plane.credits_used(), 0);
    }
}
