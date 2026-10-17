// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shard-local fill directory.
//!
//! One process is one shard. A single machine uses the same API with shard
//! id 0. The map is keyed by the SHA-256 of a physical byte key, one entry
//! per in-flight fill, not per token. Membership changes bump the epoch and
//! drop in-flight fills. There is no cross-shard quorum and no payload store.
//! Credits are an admission limit, not a replica set. A finished, failed, or
//! fenced fill leaves a bounded notice. That notice is not a hit and carries
//! no bytes. An epoch that cannot advance does not drop in-flight work.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use gpucache_core_types::{content_checksum, CacheError, QuotaKind, HARD_BYTES_CAP};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberView {
    pub node_id: u64,
    pub incarnation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardParams {
    pub credit_bytes: u64,
    pub max_bytes_per_tenant: u64,
    pub max_flights: usize,
    pub max_waiters: usize,
    pub max_members: usize,
}

impl ShardParams {
    pub fn new(
        credit_bytes: u64,
        max_bytes_per_tenant: u64,
        max_flights: usize,
        max_waiters: usize,
        max_members: usize,
    ) -> Result<Self, CacheError> {
        if credit_bytes == 0
            || max_bytes_per_tenant == 0
            || max_bytes_per_tenant > credit_bytes
            || max_flights == 0
            || max_waiters == 0
            || max_members == 0
        {
            return Err(CacheError::Malformed {
                reason: "shard config",
            });
        }
        Ok(Self {
            credit_bytes,
            max_bytes_per_tenant,
            max_flights,
            max_waiters,
            max_members,
        })
    }
}

/// Caller-owned fill request. The directory keeps the tenant id and the
/// physical-key checksum, not the key bytes and not a KV payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FillRequest {
    pub physical_key: Vec<u8>,
    pub tenant: Vec<u8>,
    pub bytes: u64,
    pub request: [u8; 16],
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    Leader { flight_id: u64, epoch: u64 },
    Joined { flight_id: u64, epoch: u64 },
}

/// Observable end of one fill. None of these variants carries payload bytes.
/// `LeaderFinished` is not a cache hit: the caller still has to `Get` a
/// committed block or recompute. A missing notice is [`CacheError::UnknownGeneration`],
/// which is also a recompute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillNotice {
    Pending,
    LeaderFinished,
    LeaderFailed,
    Fenced,
}

struct Flight {
    id: u64,
    epoch: u64,
    bytes: u64,
    tenant: Vec<u8>,
    leader: [u8; 16],
    waiters: BTreeSet<[u8; 16]>,
}

pub struct ShardDirectory {
    shard_id: u64,
    epoch: u64,
    members: Vec<MemberView>,
    flights: BTreeMap<[u8; 32], Flight>,
    by_request: BTreeMap<(Vec<u8>, [u8; 16]), [u8; 32]>,
    by_id: BTreeMap<u64, [u8; 32]>,
    credits_used: u64,
    credit_bytes: u64,
    tenant_used: BTreeMap<Vec<u8>, u64>,
    max_bytes_per_tenant: u64,
    max_flights: usize,
    max_waiters: usize,
    max_members: usize,
    next_flight: u64,
    notices: BTreeMap<(Vec<u8>, [u8; 16]), FillNotice>,
    notice_order: VecDeque<(Vec<u8>, [u8; 16])>,
    max_notices: usize,
}

impl ShardDirectory {
    /// Same methods as [`Self::open`]. Shard id is 0 and the only member is
    /// node 1 at incarnation 1.
    pub fn single_node(params: ShardParams) -> Self {
        Self::open(0, params)
    }

    pub fn open(shard_id: u64, params: ShardParams) -> Self {
        Self {
            shard_id,
            epoch: 1,
            members: vec![MemberView {
                node_id: shard_id.wrapping_add(1),
                incarnation: 1,
            }],
            flights: BTreeMap::new(),
            by_request: BTreeMap::new(),
            by_id: BTreeMap::new(),
            credits_used: 0,
            credit_bytes: params.credit_bytes,
            tenant_used: BTreeMap::new(),
            max_bytes_per_tenant: params.max_bytes_per_tenant,
            max_flights: params.max_flights,
            max_waiters: params.max_waiters,
            max_members: params.max_members,
            next_flight: 1,
            notices: BTreeMap::new(),
            notice_order: VecDeque::new(),
            max_notices: params
                .max_flights
                .saturating_mul(params.max_waiters.saturating_add(1))
                .max(1),
        }
    }

    pub fn shard_id(&self) -> u64 {
        self.shard_id
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn members(&self) -> &[MemberView] {
        &self.members
    }

    pub fn credits_used(&self) -> u64 {
        self.credits_used
    }

    pub fn inflight(&self) -> usize {
        self.flights.len()
    }

    /// Drop in-flight fills and bump the epoch. Credits return to the pool.
    /// Callers must treat the dropped fills as misses. This is not a copy.
    /// When the epoch cannot advance, in-flight fills stay and the error is
    /// returned instead.
    pub fn fence_epoch(&mut self) -> Result<u64, CacheError> {
        self.fence_inflight()
    }

    pub fn check_epoch(&self, request_epoch: u64) -> Result<(), CacheError> {
        if request_epoch != self.epoch {
            return Err(CacheError::EpochMismatch {
                store: self.epoch,
                request: request_epoch,
            });
        }
        Ok(())
    }

    /// Record a membership change. An unchanged incarnation is a no-op.
    /// Any real change bumps the epoch and fails in-flight fills.
    pub fn note_member(&mut self, node_id: u64, incarnation: u64) -> Result<u64, CacheError> {
        if incarnation == 0 {
            return Err(CacheError::Malformed {
                reason: "membership",
            });
        }
        if let Some(pos) = self
            .members
            .iter()
            .position(|member| member.node_id == node_id)
        {
            if self.members[pos].incarnation == incarnation {
                return Ok(self.epoch);
            }
            if incarnation < self.members[pos].incarnation {
                return Err(CacheError::FencingRejected { reason: "member" });
            }
            self.ensure_epoch_can_advance()?;
            self.members[pos].incarnation = incarnation;
            return self.fence_inflight();
        }
        if self.members.len() >= self.max_members {
            return Err(CacheError::Malformed {
                reason: "membership",
            });
        }
        self.ensure_epoch_can_advance()?;
        self.members.push(MemberView {
            node_id,
            incarnation,
        });
        self.fence_inflight()
    }

    pub fn remove_member(&mut self, node_id: u64) -> Result<u64, CacheError> {
        let Some(pos) = self
            .members
            .iter()
            .position(|member| member.node_id == node_id)
        else {
            return Err(CacheError::Malformed {
                reason: "membership",
            });
        };
        if self.members.len() == 1 {
            return Err(CacheError::FencingRejected { reason: "member" });
        }
        self.ensure_epoch_can_advance()?;
        self.members.remove(pos);
        self.fence_inflight()
    }

    pub fn begin_or_join(&mut self, req: &FillRequest) -> Result<Admit, CacheError> {
        self.check_epoch(req.epoch)?;
        validate_fill(req)?;
        let key = flight_key(&req.physical_key);
        let request_key = (req.tenant.clone(), req.request);
        if let Some(existing) = self.by_request.get(&request_key).copied() {
            if existing != key {
                return Err(CacheError::IdempotencyConflict);
            }
            let flight = &self.flights[&existing];
            if flight.bytes != req.bytes {
                return Err(CacheError::IdempotencyConflict);
            }
            if flight.leader == req.request {
                return Ok(Admit::Leader {
                    flight_id: flight.id,
                    epoch: flight.epoch,
                });
            }
            return Ok(Admit::Joined {
                flight_id: flight.id,
                epoch: flight.epoch,
            });
        }
        if let Some(flight) = self.flights.get_mut(&key) {
            if flight.tenant.as_slice() != req.tenant.as_slice() {
                return Err(CacheError::Unauthorized);
            }
            if flight.bytes != req.bytes {
                return Err(CacheError::IdempotencyConflict);
            }
            if flight.waiters.len() >= self.max_waiters {
                return Err(CacheError::QuotaExceeded {
                    kind: QuotaKind::InFlightRequests,
                });
            }
            flight.waiters.insert(req.request);
            self.by_request.insert(request_key, key);
            return Ok(Admit::Joined {
                flight_id: flight.id,
                epoch: flight.epoch,
            });
        }
        if self.flights.len() >= self.max_flights || self.next_flight == u64::MAX {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests,
            });
        }
        self.reserve(&req.tenant, req.bytes)?;
        let id = self.next_flight;
        self.next_flight += 1;
        self.by_request.insert(request_key, key);
        self.by_id.insert(id, key);
        self.flights.insert(
            key,
            Flight {
                id,
                epoch: self.epoch,
                bytes: req.bytes,
                tenant: req.tenant.clone(),
                leader: req.request,
                waiters: BTreeSet::new(),
            },
        );
        Ok(Admit::Leader {
            flight_id: id,
            epoch: self.epoch,
        })
    }

    pub fn complete_flight(
        &mut self,
        flight_id: u64,
        request_epoch: u64,
        request: [u8; 16],
    ) -> Result<(), CacheError> {
        self.finish(
            flight_id,
            request_epoch,
            request,
            FillNotice::LeaderFinished,
        )
    }

    pub fn fail_flight(
        &mut self,
        flight_id: u64,
        request_epoch: u64,
        request: [u8; 16],
    ) -> Result<(), CacheError> {
        self.finish(flight_id, request_epoch, request, FillNotice::LeaderFailed)
    }

    /// Status of one leader or joiner. In-flight work is `Pending`. A terminal
    /// notice is not a payload. An evicted or unknown request is not a hit.
    pub fn fill_notice(&self, tenant: &[u8], request: [u8; 16]) -> Result<FillNotice, CacheError> {
        let key = (tenant.to_vec(), request);
        if self.by_request.contains_key(&key) {
            return Ok(FillNotice::Pending);
        }
        self.notices
            .get(&key)
            .copied()
            .ok_or(CacheError::UnknownGeneration)
    }

    fn finish(
        &mut self,
        flight_id: u64,
        request_epoch: u64,
        request: [u8; 16],
        notice: FillNotice,
    ) -> Result<(), CacheError> {
        self.check_epoch(request_epoch)?;
        let Some(key) = self.by_id.get(&flight_id).copied() else {
            return Err(CacheError::UnknownGeneration);
        };
        let flight = &self.flights[&key];
        if flight.leader != request || flight.epoch != request_epoch {
            return Err(CacheError::FencingRejected { reason: "epoch" });
        }
        let tenant = flight.tenant.clone();
        let bytes = flight.bytes;
        let flight = self
            .flights
            .remove(&key)
            .ok_or(CacheError::UnknownGeneration)?;
        self.by_id.remove(&flight_id);
        self.by_request.remove(&(tenant.clone(), flight.leader));
        self.record_notice(tenant.clone(), flight.leader, notice);
        for waiter in flight.waiters {
            self.by_request.remove(&(tenant.clone(), waiter));
            self.record_notice(tenant.clone(), waiter, notice);
        }
        self.release(&tenant, bytes)
    }

    fn fence_inflight(&mut self) -> Result<u64, CacheError> {
        let next = self.epoch.checked_add(1).ok_or(CacheError::QuotaExceeded {
            kind: QuotaKind::InFlightRequests,
        })?;
        self.epoch = next;
        self.by_request.clear();
        self.by_id.clear();
        let flights = std::mem::take(&mut self.flights);
        for (_, flight) in flights {
            self.record_notice(flight.tenant.clone(), flight.leader, FillNotice::Fenced);
            for waiter in &flight.waiters {
                self.record_notice(flight.tenant.clone(), *waiter, FillNotice::Fenced);
            }
            let _ = self.release(&flight.tenant, flight.bytes);
        }
        Ok(self.epoch)
    }

    fn ensure_epoch_can_advance(&self) -> Result<(), CacheError> {
        if self.epoch == u64::MAX {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests,
            });
        }
        Ok(())
    }

    fn record_notice(&mut self, tenant: Vec<u8>, request: [u8; 16], notice: FillNotice) {
        let key = (tenant, request);
        if self.notices.contains_key(&key) {
            self.notice_order.retain(|existing| existing != &key);
        }
        self.notices.insert(key.clone(), notice);
        self.notice_order.push_back(key);
        while self.notices.len() > self.max_notices {
            let Some(old) = self.notice_order.pop_front() else {
                break;
            };
            self.notices.remove(&old);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_epoch_for_test(&mut self, epoch: u64) {
        self.epoch = epoch;
    }

    fn reserve(&mut self, tenant: &[u8], bytes: u64) -> Result<(), CacheError> {
        let used = self.tenant_used.get(tenant).copied().unwrap_or(0);
        let tenant_next = used.checked_add(bytes).ok_or(CacheError::QuotaExceeded {
            kind: QuotaKind::TenantRetainedBytes,
        })?;
        if tenant_next > self.max_bytes_per_tenant {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::TenantRetainedBytes,
            });
        }
        let total_next = self
            .credits_used
            .checked_add(bytes)
            .ok_or(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            })?;
        if total_next > self.credit_bytes {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            });
        }
        self.tenant_used.insert(tenant.to_vec(), tenant_next);
        self.credits_used = total_next;
        Ok(())
    }

    fn release(&mut self, tenant: &[u8], bytes: u64) -> Result<(), CacheError> {
        let used = self.tenant_used.get(tenant).copied().unwrap_or(0);
        let Some(next) = used.checked_sub(bytes) else {
            return Err(CacheError::Storage { reason: "credits" });
        };
        if next == 0 {
            self.tenant_used.remove(tenant);
        } else {
            self.tenant_used.insert(tenant.to_vec(), next);
        }
        self.credits_used = self
            .credits_used
            .checked_sub(bytes)
            .ok_or(CacheError::Storage { reason: "credits" })?;
        Ok(())
    }
}

fn flight_key(physical_key: &[u8]) -> [u8; 32] {
    content_checksum(physical_key).0
}

fn validate_fill(req: &FillRequest) -> Result<(), CacheError> {
    if req.physical_key.is_empty() || req.tenant.is_empty() || req.bytes == 0 {
        return Err(CacheError::Malformed {
            reason: "shard fill",
        });
    }
    if req.physical_key.len() > HARD_BYTES_CAP {
        return Err(CacheError::ObjectTooLarge {
            limit: HARD_BYTES_CAP as u64,
            actual: req.physical_key.len() as u64,
        });
    }
    if req.tenant.len() > 128 {
        return Err(CacheError::Malformed {
            reason: "shard fill",
        });
    }
    if req.request == [0u8; 16] {
        return Err(CacheError::Malformed {
            reason: "request_id must not be all zeros",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ShardParams {
        ShardParams::new(100, 80, 4, 2, 4).unwrap()
    }

    fn fill(key: &[u8], tenant: &[u8], bytes: u64, request: u128, epoch: u64) -> FillRequest {
        FillRequest {
            physical_key: key.to_vec(),
            tenant: tenant.to_vec(),
            bytes,
            request: request.to_le_bytes(),
            epoch,
        }
    }

    #[test]
    fn single_node_is_one_shard_and_join_charges_once() {
        let mut dir = ShardDirectory::single_node(params());
        assert_eq!(dir.shard_id(), 0);
        assert_eq!(dir.members().len(), 1);
        assert_eq!(dir.epoch(), 1);
        let leader = dir
            .begin_or_join(&fill(b"phys-a", b"tenant-a", 40, 1, 1))
            .unwrap();
        let joined = dir
            .begin_or_join(&fill(b"phys-a", b"tenant-a", 40, 2, 1))
            .unwrap();
        assert!(matches!(leader, Admit::Leader { epoch: 1, .. }));
        assert!(matches!(joined, Admit::Joined { epoch: 1, .. }));
        assert_eq!(dir.credits_used(), 40);
        assert_eq!(dir.inflight(), 1);
        let Admit::Leader { flight_id, .. } = leader else {
            panic!("leader");
        };
        dir.complete_flight(flight_id, 1, 1u128.to_le_bytes())
            .unwrap();
        assert_eq!(dir.credits_used(), 0);
        assert_eq!(dir.inflight(), 0);
    }

    #[test]
    fn two_shard_values_do_not_share_flights() {
        let mut left = ShardDirectory::open(0, params());
        let right = ShardDirectory::open(1, params());
        left.begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 1, 1))
            .unwrap();
        assert_eq!(left.inflight(), 1);
        assert_eq!(right.inflight(), 0);
        assert_eq!(right.shard_id(), 1);
        assert_ne!(left.members()[0].node_id, right.members()[0].node_id);
    }

    #[test]
    fn stale_epoch_releases_credits_and_hides_the_fill() {
        let mut dir = ShardDirectory::single_node(params());
        let admit = dir
            .begin_or_join(&fill(b"phys-a", b"tenant-a", 40, 1, 1))
            .unwrap();
        let Admit::Leader { flight_id, epoch } = admit else {
            panic!("leader");
        };
        let bumped = dir.note_member(1, 2).unwrap();
        assert_eq!(bumped, 2);
        assert_eq!(dir.credits_used(), 0);
        assert_eq!(dir.inflight(), 0);
        let err = dir
            .complete_flight(flight_id, epoch, 1u128.to_le_bytes())
            .unwrap_err();
        assert_eq!(
            err,
            CacheError::EpochMismatch {
                store: 2,
                request: 1,
            }
        );
        dir.begin_or_join(&fill(b"phys-a", b"tenant-a", 40, 3, 2))
            .unwrap();
        assert_eq!(dir.credits_used(), 40);
    }

    #[test]
    fn other_tenant_cannot_join_and_quota_does_not_drop_the_leader() {
        let mut dir = ShardDirectory::single_node(params());
        dir.begin_or_join(&fill(b"phys-a", b"tenant-a", 40, 1, 1))
            .unwrap();
        let foreign = dir
            .begin_or_join(&fill(b"phys-a", b"tenant-b", 40, 2, 1))
            .unwrap_err();
        assert_eq!(foreign, CacheError::Unauthorized);
        let quota = dir
            .begin_or_join(&fill(b"phys-b", b"tenant-a", 50, 3, 1))
            .unwrap_err();
        assert_eq!(
            quota,
            CacheError::QuotaExceeded {
                kind: QuotaKind::TenantRetainedBytes,
            }
        );
        assert_eq!(dir.credits_used(), 40);
        assert_eq!(dir.inflight(), 1);
    }

    #[test]
    fn repeated_leader_does_not_charge_twice_and_last_member_stays() {
        let mut dir = ShardDirectory::single_node(params());
        dir.begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 1, 1))
            .unwrap();
        dir.begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 1, 1))
            .unwrap();
        assert_eq!(dir.credits_used(), 10);
        let err = dir.remove_member(1).unwrap_err();
        assert_eq!(err, CacheError::FencingRejected { reason: "member" });
        assert_eq!(dir.epoch(), 1);
        assert_eq!(dir.inflight(), 1);
    }

    #[test]
    fn joiner_cannot_finish_and_leader_fail_releases_credits() {
        let mut dir = ShardDirectory::single_node(params());
        let leader = dir
            .begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 1, 1))
            .unwrap();
        dir.begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 2, 1))
            .unwrap();
        let Admit::Leader { flight_id, .. } = leader else {
            panic!("leader");
        };
        let err = dir
            .complete_flight(flight_id, 1, 2u128.to_le_bytes())
            .unwrap_err();
        assert_eq!(err, CacheError::FencingRejected { reason: "epoch" });
        assert_eq!(dir.credits_used(), 10);
        dir.fail_flight(flight_id, 1, 1u128.to_le_bytes()).unwrap();
        assert_eq!(dir.credits_used(), 0);
        assert_eq!(dir.inflight(), 0);
        let conflict = dir
            .begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 1, 1))
            .unwrap();
        let Admit::Leader { flight_id, .. } = conflict else {
            panic!("leader");
        };
        let err = dir
            .begin_or_join(&fill(b"phys-b", b"tenant-a", 10, 1, 1))
            .unwrap_err();
        assert_eq!(err, CacheError::IdempotencyConflict);
        dir.fail_flight(flight_id, 1, 1u128.to_le_bytes()).unwrap();
    }

    #[test]
    fn unchanged_incarnation_does_not_fence() {
        let mut dir = ShardDirectory::single_node(params());
        dir.begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 1, 1))
            .unwrap();
        assert_eq!(dir.note_member(1, 1).unwrap(), 1);
        assert_eq!(dir.inflight(), 1);
        assert_eq!(dir.credits_used(), 10);
    }
    #[test]
    fn tenant_and_size_are_checked_before_replay_or_join() {
        let mut dir = ShardDirectory::single_node(params());
        dir.begin_or_join(&fill(b"a", b"tenant-a", 10, 1, 1))
            .unwrap();
        assert_eq!(
            dir.begin_or_join(&fill(b"a", b"tenant-b", 10, 1, 1)),
            Err(CacheError::Unauthorized)
        );
        assert_eq!(
            dir.begin_or_join(&fill(b"a", b"tenant-a", 20, 1, 1)),
            Err(CacheError::IdempotencyConflict)
        );
        assert_eq!(
            dir.begin_or_join(&fill(b"a", b"tenant-a", 20, 2, 1)),
            Err(CacheError::IdempotencyConflict)
        );
        dir.begin_or_join(&fill(b"b", b"tenant-b", 10, 1, 1))
            .unwrap();
        assert_eq!(dir.credits_used(), 20);
    }

    #[test]
    fn indexes_are_reclaimed_on_completion_failure_and_fencing() {
        let mut dir = ShardDirectory::single_node(params());
        for finish in 0..3 {
            let epoch = dir.epoch();
            let Admit::Leader { flight_id, .. } = dir
                .begin_or_join(&fill(b"a", b"tenant-a", 10, 1, epoch))
                .unwrap()
            else {
                panic!("leader")
            };
            dir.begin_or_join(&fill(b"a", b"tenant-a", 10, 2, epoch))
                .unwrap();
            match finish {
                0 => dir
                    .complete_flight(flight_id, epoch, 1u128.to_le_bytes())
                    .unwrap(),
                1 => dir
                    .fail_flight(flight_id, epoch, 1u128.to_le_bytes())
                    .unwrap(),
                _ => {
                    dir.fence_epoch().unwrap();
                }
            }
            assert!(dir.by_request.is_empty());
            assert!(dir.by_id.is_empty());
            assert_eq!(dir.credits_used(), 0);
        }
    }

    #[test]
    fn stale_membership_does_not_fence_current_work() {
        let mut dir = ShardDirectory::single_node(params());
        let epoch = dir.note_member(1, 2).unwrap();
        dir.begin_or_join(&fill(b"a", b"tenant-a", 10, 1, epoch))
            .unwrap();
        assert_eq!(
            dir.note_member(1, 1),
            Err(CacheError::FencingRejected { reason: "member" })
        );
        assert_eq!(dir.epoch(), epoch);
        assert_eq!(dir.inflight(), 1);
    }

    #[test]
    fn finish_and_failure_are_notices_without_bytes_and_a_fence_is_not_success() {
        let mut dir = ShardDirectory::single_node(params());
        let leader = dir
            .begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 1, 1))
            .unwrap();
        dir.begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 2, 1))
            .unwrap();
        assert_eq!(
            dir.fill_notice(b"tenant-a", 2u128.to_le_bytes()).unwrap(),
            FillNotice::Pending
        );
        let Admit::Leader { flight_id, .. } = leader else {
            panic!("leader");
        };
        dir.fail_flight(flight_id, 1, 1u128.to_le_bytes()).unwrap();
        assert_eq!(dir.inflight(), 0);
        assert_eq!(dir.credits_used(), 0);
        assert_eq!(
            dir.fill_notice(b"tenant-a", 1u128.to_le_bytes()).unwrap(),
            FillNotice::LeaderFailed
        );
        assert_eq!(
            dir.fill_notice(b"tenant-a", 2u128.to_le_bytes()).unwrap(),
            FillNotice::LeaderFailed
        );
        let again = dir
            .begin_or_join(&fill(b"phys-b", b"tenant-a", 10, 3, 1))
            .unwrap();
        let Admit::Leader { flight_id, .. } = again else {
            panic!("leader");
        };
        dir.begin_or_join(&fill(b"phys-b", b"tenant-a", 10, 4, 1))
            .unwrap();
        dir.complete_flight(flight_id, 1, 3u128.to_le_bytes())
            .unwrap();
        assert_eq!(
            dir.fill_notice(b"tenant-a", 4u128.to_le_bytes()).unwrap(),
            FillNotice::LeaderFinished
        );
        assert_eq!(dir.credits_used(), 0);
        let fenced = dir
            .begin_or_join(&fill(b"phys-c", b"tenant-a", 10, 5, 1))
            .unwrap();
        dir.begin_or_join(&fill(b"phys-c", b"tenant-a", 10, 6, 1))
            .unwrap();
        let Admit::Leader { flight_id, epoch } = fenced else {
            panic!("leader");
        };
        dir.fence_epoch().unwrap();
        assert_eq!(
            dir.complete_flight(flight_id, epoch, 5u128.to_le_bytes())
                .unwrap_err(),
            CacheError::EpochMismatch {
                store: 2,
                request: 1,
            }
        );
        assert_eq!(
            dir.fill_notice(b"tenant-a", 5u128.to_le_bytes()).unwrap(),
            FillNotice::Fenced
        );
        assert_eq!(
            dir.fill_notice(b"tenant-a", 6u128.to_le_bytes()).unwrap(),
            FillNotice::Fenced
        );
        assert_ne!(
            dir.fill_notice(b"tenant-a", 5u128.to_le_bytes()).unwrap(),
            FillNotice::LeaderFinished
        );
        assert_eq!(
            dir.fill_notice(b"tenant-a", 9u128.to_le_bytes())
                .unwrap_err(),
            CacheError::UnknownGeneration
        );
    }

    #[test]
    fn notice_table_drops_the_oldest_instead_of_growing() {
        let mut dir = ShardDirectory::single_node(ShardParams::new(100, 100, 1, 1, 2).unwrap());
        assert_eq!(dir.max_notices, 2);
        for request in 1u128..=3 {
            let epoch = dir.epoch();
            let Admit::Leader { flight_id, .. } = dir
                .begin_or_join(&fill(b"phys", b"tenant-a", 10, request, epoch))
                .unwrap()
            else {
                panic!("leader");
            };
            dir.complete_flight(flight_id, epoch, request.to_le_bytes())
                .unwrap();
        }
        assert_eq!(
            dir.fill_notice(b"tenant-a", 1u128.to_le_bytes())
                .unwrap_err(),
            CacheError::UnknownGeneration
        );
        assert_eq!(
            dir.fill_notice(b"tenant-a", 3u128.to_le_bytes()).unwrap(),
            FillNotice::LeaderFinished
        );
        assert_eq!(dir.notices.len(), 2);
    }

    #[test]
    fn epoch_overflow_does_not_drop_the_fill_or_publish_a_new_member() {
        let mut dir = ShardDirectory::single_node(params());
        dir.set_epoch_for_test(u64::MAX);
        let admit = dir
            .begin_or_join(&fill(b"phys-a", b"tenant-a", 10, 1, u64::MAX))
            .unwrap();
        let Admit::Leader { flight_id, .. } = admit else {
            panic!("leader");
        };
        let err = dir.note_member(1, 2).unwrap_err();
        assert_eq!(
            err,
            CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests,
            }
        );
        assert_eq!(dir.epoch(), u64::MAX);
        assert_eq!(dir.inflight(), 1);
        assert_eq!(dir.credits_used(), 10);
        assert_eq!(dir.members()[0].incarnation, 1);
        assert_eq!(
            dir.fill_notice(b"tenant-a", 1u128.to_le_bytes()).unwrap(),
            FillNotice::Pending
        );
        let err = dir.fence_epoch().unwrap_err();
        assert_eq!(
            err,
            CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests,
            }
        );
        assert_eq!(dir.inflight(), 1);
        dir.complete_flight(flight_id, u64::MAX, 1u128.to_le_bytes())
            .unwrap();
        assert_eq!(dir.inflight(), 0);
        assert_eq!(dir.credits_used(), 0);
        assert_eq!(
            dir.fill_notice(b"tenant-a", 1u128.to_le_bytes()).unwrap(),
            FillNotice::LeaderFinished
        );
    }
}
