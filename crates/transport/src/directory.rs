// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Shard-local fill directory.
//!
//! One process is one shard. A single machine uses the same API with shard
//! id 0. The map is keyed by the SHA-256 of a physical byte key, one entry
//! per in-flight fill, not per token. Membership changes bump the epoch and
//! drop in-flight fills. There is no cross-shard quorum and no payload store.
//! Credits are an admission limit, not a replica set.

use std::collections::{BTreeMap, BTreeSet};

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
    credits_used: u64,
    credit_bytes: u64,
    tenant_used: BTreeMap<Vec<u8>, u64>,
    max_bytes_per_tenant: u64,
    max_flights: usize,
    max_waiters: usize,
    max_members: usize,
    next_flight: u64,
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
            credits_used: 0,
            credit_bytes: params.credit_bytes,
            tenant_used: BTreeMap::new(),
            max_bytes_per_tenant: params.max_bytes_per_tenant,
            max_flights: params.max_flights,
            max_waiters: params.max_waiters,
            max_members: params.max_members,
            next_flight: 1,
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
    pub fn fence_epoch(&mut self) -> u64 {
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
            self.members[pos].incarnation = incarnation;
            return Ok(self.fence_inflight());
        }
        if self.members.len() >= self.max_members {
            return Err(CacheError::Malformed {
                reason: "membership",
            });
        }
        self.members.push(MemberView {
            node_id,
            incarnation,
        });
        Ok(self.fence_inflight())
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
        self.members.remove(pos);
        Ok(self.fence_inflight())
    }

    pub fn begin_or_join(&mut self, req: &FillRequest) -> Result<Admit, CacheError> {
        self.check_epoch(req.epoch)?;
        validate_fill(req)?;
        let key = flight_key(&req.physical_key);
        if let Some(existing) = self.key_for_request(req.request) {
            if existing != key {
                return Err(CacheError::IdempotencyConflict);
            }
            let flight = &self.flights[&existing];
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
            if flight.waiters.len() >= self.max_waiters {
                return Err(CacheError::QuotaExceeded {
                    kind: QuotaKind::InFlightRequests,
                });
            }
            flight.waiters.insert(req.request);
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
        self.finish(flight_id, request_epoch, request)
    }

    pub fn fail_flight(
        &mut self,
        flight_id: u64,
        request_epoch: u64,
        request: [u8; 16],
    ) -> Result<(), CacheError> {
        self.finish(flight_id, request_epoch, request)
    }

    fn finish(
        &mut self,
        flight_id: u64,
        request_epoch: u64,
        request: [u8; 16],
    ) -> Result<(), CacheError> {
        self.check_epoch(request_epoch)?;
        let Some(key) = self
            .flights
            .iter()
            .find(|(_, flight)| flight.id == flight_id)
            .map(|(key, _)| *key)
        else {
            return Err(CacheError::UnknownGeneration);
        };
        let flight = &self.flights[&key];
        if flight.leader != request || flight.epoch != request_epoch {
            return Err(CacheError::FencingRejected { reason: "epoch" });
        }
        let tenant = flight.tenant.clone();
        let bytes = flight.bytes;
        self.flights.remove(&key);
        self.release(&tenant, bytes)
    }

    fn fence_inflight(&mut self) -> u64 {
        self.epoch = self.epoch.saturating_add(1);
        let flights = std::mem::take(&mut self.flights);
        for (_, flight) in flights {
            let _ = self.release(&flight.tenant, flight.bytes);
        }
        self.epoch
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

    fn key_for_request(&self, request: [u8; 16]) -> Option<[u8; 32]> {
        self.flights.iter().find_map(|(key, flight)| {
            if flight.leader == request || flight.waiters.contains(&request) {
                Some(*key)
            } else {
                None
            }
        })
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
}
