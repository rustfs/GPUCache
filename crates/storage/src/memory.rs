// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use gpucache_core_types::{
    content_checksum, encode_logical_key, encode_physical_key, verify_checksum, Attestation,
    BlockState, CacheError, CacheIdentity, Checksum, Event, IdentityLimits, Lifecycle, QuotaKind,
    RequestId, ResourceLimits,
};
use gpucache_protocol::{
    ensure_deadline, fingerprints_equal, AuthContext, BatchReply, CommitItem, CommitOutcome,
    GetHit, GetItem, InvalidateItem, InvalidateOutcome, ItemResult, LookupItem, LookupOutcome,
    MissReason, ReleaseCancelItem, ReleaseOutcome, ReserveOutcome, ReservePutItem, TenantPolicy,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Charges {
    retained: bool,
    inflight: bool,
    registration: bool,
}

impl Charges {
    const NONE: Self = Self {
        retained: false,
        inflight: false,
        registration: false,
    };
}

#[derive(Clone, Copy, Debug)]
struct ChargeTarget {
    retained: bool,
    inflight: bool,
    registration: bool,
}

#[derive(Clone, Debug)]
struct Pin {
    request_id: RequestId,
    deadline_ms: u64,
    stuck: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailKind {
    Timeout,
    Cancelled,
    Fenced,
    Corrupt,
}

struct Slot {
    identity: CacheIdentity,
    physical: Vec<u8>,
    logical: Vec<u8>,
    life: Lifecycle,
    length: u64,
    checksum: Checksum,
    bytes: Option<Vec<u8>>,
    reserve_epoch: u64,
    lease_deadline_ms: u64,
    region_id: Option<u64>,
    pins: Vec<Pin>,
    charges: Charges,
    fail: Option<FailKind>,
}

#[derive(Clone, Debug)]
enum IdemKind {
    Reserve(Result<ReserveOutcome, CacheError>),
    Commit(Result<CommitOutcome, CacheError>),
    Release(Result<ReleaseOutcome, CacheError>),
    Invalidate(Result<InvalidateOutcome, CacheError>),
}

struct IdemRecord {
    fingerprint: Checksum,
    inserted_ms: u64,
    kind: IdemKind,
}

/// Explicit fenced reclaim. `observed_epoch` must equal the current store epoch.
pub struct ReclaimRequest {
    pub request_id: RequestId,
    pub epoch: u64,
    pub deadline_ms: u64,
    pub generation: u64,
    pub observed_epoch: u64,
}

/// Process-local shared KV store. Only `COMMITTED` compatible blocks are hits.
pub struct MemoryStore {
    limits: ResourceLimits,
    identity_limits: IdentityLimits,
    policies: HashMap<Vec<u8>, TenantPolicy>,
    slots: HashMap<u64, Slot>,
    by_physical: HashMap<Vec<u8>, u64>,
    logical: HashMap<Vec<u8>, Vec<Vec<u8>>>,
    idem: HashMap<RequestId, IdemRecord>,
    next_generation: u64,
    next_region: u64,
    epoch: u64,
    retained_bytes: u64,
    inflight_requests: u32,
    registrations: u32,
    tenant_retained: HashMap<Vec<u8>, u64>,
    tenant_inflight: HashMap<Vec<u8>, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreStats {
    pub retained_bytes: u64,
    pub registrations: u32,
    pub in_flight_requests: u32,
    pub quarantined: u32,
    pub committed: u32,
    pub idempotency_entries: u32,
}

impl MemoryStore {
    pub fn new(limits: ResourceLimits, policies: Vec<TenantPolicy>) -> Result<Self, CacheError> {
        limits.validate()?;
        let mut map = HashMap::with_capacity(policies.len());
        for policy in policies {
            if map
                .insert(policy.tenant.as_bytes().to_vec(), policy)
                .is_some()
            {
                return Err(CacheError::Malformed {
                    reason: "duplicate tenant policy",
                });
            }
        }
        Ok(Self {
            identity_limits: IdentityLimits::from_resources(&limits),
            limits,
            policies: map,
            slots: HashMap::new(),
            by_physical: HashMap::new(),
            logical: HashMap::new(),
            idem: HashMap::new(),
            next_generation: 1,
            next_region: 1,
            epoch: 1,
            retained_bytes: 0,
            inflight_requests: 0,
            registrations: 0,
            tenant_retained: HashMap::new(),
            tenant_inflight: HashMap::new(),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn stats(&self) -> StoreStats {
        let mut quarantined = 0;
        let mut committed = 0;
        for slot in self.slots.values() {
            match slot.life.state {
                BlockState::Quarantined => quarantined += 1,
                BlockState::Committed => committed += 1,
                _ => {}
            }
        }
        StoreStats {
            retained_bytes: self.retained_bytes,
            registrations: self.registrations,
            in_flight_requests: self.inflight_requests,
            quarantined,
            committed,
            idempotency_entries: u32::try_from(self.idem.len()).unwrap_or(u32::MAX),
        }
    }

    pub fn state_of(
        &mut self,
        auth: &AuthContext,
        generation: u64,
        now_ms: u64,
    ) -> Result<BlockState, CacheError> {
        self.poll(now_ms);
        self.authorize(auth, None)?;
        let slot = self
            .slots
            .get(&generation)
            .ok_or(CacheError::UnknownGeneration)?;
        self.same_tenant(auth, &slot.identity)?;
        Ok(slot.life.state)
    }

    /// Advance the store epoch and fence in-flight writers.
    ///
    /// Committed blocks stay readable. Registered regions whose completion is
    /// unknown stay allocated until [`Self::reclaim_fenced`].
    pub fn bump_epoch(&mut self, now_ms: u64) -> u64 {
        self.poll(now_ms);
        self.epoch = self.epoch.saturating_add(1);
        let generations: Vec<u64> = self.slots.keys().copied().collect();
        for generation in generations {
            let applied = self
                .slots
                .get_mut(&generation)
                .and_then(|slot| slot.life.apply(Event::Fence).ok());
            if applied.is_some() {
                if let Some(slot) = self.slots.get_mut(&generation) {
                    if slot.life.state == BlockState::Quarantined && slot.fail.is_none() {
                        slot.fail = Some(FailKind::Fenced);
                    }
                }
                let _ = self.reconcile(generation);
            }
        }
        self.epoch
    }

    pub fn reclaim_fenced(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        request: ReclaimRequest,
    ) -> Result<ReleaseOutcome, CacheError> {
        self.poll(now_ms);
        self.authorize(auth, None)?;
        let ReclaimRequest {
            request_id,
            epoch,
            deadline_ms,
            generation,
            observed_epoch,
        } = request;
        let fp = fingerprint(&[
            &[9u8][..],
            &epoch.to_le_bytes(),
            &generation.to_le_bytes(),
            &observed_epoch.to_le_bytes(),
        ]);
        if let Some(prev) = self.replay(request_id, &fp)? {
            return match prev {
                IdemKind::Release(result) => result,
                _ => Err(CacheError::IdempotencyConflict),
            };
        }
        self.preflight(
            &gpucache_protocol::ItemHeader {
                request_id,
                epoch,
                deadline_ms,
            },
            now_ms,
        )?;
        self.ensure_idem_room(now_ms)?;
        let result =
            self.reclaim_fenced_inner(auth, epoch, deadline_ms, generation, observed_epoch, now_ms);
        self.remember(request_id, fp, now_ms, IdemKind::Release(result.clone()))?;
        result
    }

    pub fn lookup(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<LookupItem>,
    ) -> Result<BatchReply<LookupOutcome>, CacheError> {
        self.prepare(
            auth,
            now_ms,
            &request_ids(items.iter().map(|i| i.header.request_id)),
        )?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let result = self.lookup_one(auth, now_ms, &item);
            out.push(ItemResult {
                request_id: item.header.request_id,
                body: result,
            });
        }
        Ok(self.reply(out))
    }

    pub fn reserve_put(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<ReservePutItem>,
    ) -> Result<BatchReply<ReserveOutcome>, CacheError> {
        self.prepare(
            auth,
            now_ms,
            &request_ids(items.iter().map(|i| i.header.request_id)),
        )?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let id = item.header.request_id;
            let result = self.reserve_one(auth, now_ms, &item);
            out.push(ItemResult {
                request_id: id,
                body: result,
            });
        }
        Ok(self.reply(out))
    }

    pub fn commit(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<CommitItem>,
    ) -> Result<BatchReply<CommitOutcome>, CacheError> {
        self.prepare(
            auth,
            now_ms,
            &request_ids(items.iter().map(|i| i.header.request_id)),
        )?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let id = item.header.request_id;
            let result = self.commit_one(auth, now_ms, &item);
            out.push(ItemResult {
                request_id: id,
                body: result,
            });
        }
        Ok(self.reply(out))
    }

    pub fn get(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<GetItem>,
    ) -> Result<BatchReply<GetHit>, CacheError> {
        self.prepare(
            auth,
            now_ms,
            &request_ids(items.iter().map(|i| i.header.request_id)),
        )?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let id = item.header.request_id;
            let result = self.get_one(auth, now_ms, &item);
            out.push(ItemResult {
                request_id: id,
                body: result,
            });
        }
        Ok(self.reply(out))
    }

    pub fn release_cancel(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<ReleaseCancelItem>,
    ) -> Result<BatchReply<ReleaseOutcome>, CacheError> {
        self.prepare(
            auth,
            now_ms,
            &request_ids(items.iter().map(|i| i.header.request_id)),
        )?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let id = item.header.request_id;
            let result = self.release_one(auth, now_ms, &item);
            out.push(ItemResult {
                request_id: id,
                body: result,
            });
        }
        Ok(self.reply(out))
    }

    pub fn invalidate(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<InvalidateItem>,
    ) -> Result<BatchReply<InvalidateOutcome>, CacheError> {
        self.prepare(
            auth,
            now_ms,
            &request_ids(items.iter().map(|i| i.header.request_id)),
        )?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let id = item.header.request_id;
            let result = self.invalidate_one(auth, now_ms, &item);
            out.push(ItemResult {
                request_id: id,
                body: result,
            });
        }
        Ok(self.reply(out))
    }

    fn reply<T>(&self, items: Vec<ItemResult<T>>) -> BatchReply<T> {
        BatchReply {
            store_epoch: self.epoch,
            items,
        }
    }

    fn prepare(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        ids: &[RequestId],
    ) -> Result<(), CacheError> {
        self.authorize(auth, None)?;
        if ids.is_empty() {
            return Err(CacheError::Malformed {
                reason: "empty batch",
            });
        }
        if ids.len() > self.limits.max_batch_items as usize {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::BatchItems,
            });
        }
        let mut seen = HashMap::<RequestId, ()>::new();
        for id in ids {
            if seen.insert(*id, ()).is_some() {
                return Err(CacheError::Malformed {
                    reason: "duplicate request_id",
                });
            }
        }
        self.poll(now_ms);
        Ok(())
    }

    fn lookup_one(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &LookupItem,
    ) -> Result<LookupOutcome, CacheError> {
        self.check_header(&item.header, now_ms)?;
        self.authorize(auth, Some(&item.identity))?;
        item.identity.validate(&self.identity_limits)?;
        let physical = encode_physical_key(&item.identity, &self.identity_limits)?;
        let logical = encode_logical_key(&item.identity, &self.identity_limits)?;
        if let Some(generation) = self.by_physical.get(&physical).copied() {
            let slot = self
                .slots
                .get(&generation)
                .ok_or(CacheError::UnknownGeneration)?;
            if slot.identity != item.identity {
                return Ok(LookupOutcome::Miss {
                    reason: MissReason::Incompatible,
                });
            }
            if slot.life.is_hit_visible() {
                return Ok(LookupOutcome::Hit {
                    generation,
                    length: slot.length,
                    checksum: slot.checksum,
                    valid_length: slot.identity.valid_length,
                });
            }
            return Ok(LookupOutcome::Miss {
                reason: miss_reason(slot.life.state),
            });
        }
        if self
            .logical
            .get(&logical)
            .map(|entries| !entries.is_empty())
            .unwrap_or(false)
        {
            return Ok(LookupOutcome::Miss {
                reason: MissReason::Incompatible,
            });
        }
        Ok(LookupOutcome::Miss {
            reason: MissReason::Absent,
        })
    }

    fn reserve_one(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &ReservePutItem,
    ) -> Result<ReserveOutcome, CacheError> {
        let fp = reserve_fingerprint(item)?;
        if let Some(prev) = self.replay(item.header.request_id, &fp)? {
            return match prev {
                IdemKind::Reserve(result) => result,
                _ => Err(CacheError::IdempotencyConflict),
            };
        }
        self.preflight(&item.header, now_ms)?;
        self.ensure_idem_room(now_ms)?;
        let result = self.reserve_new(auth, now_ms, item);
        self.remember(
            item.header.request_id,
            fp,
            now_ms,
            IdemKind::Reserve(result.clone()),
        )?;
        result
    }

    fn reserve_new(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &ReservePutItem,
    ) -> Result<ReserveOutcome, CacheError> {
        self.check_header(&item.header, now_ms)?;
        self.authorize(auth, Some(&item.identity))?;
        item.identity.validate(&self.identity_limits)?;
        if item.length == 0 || item.lease_ttl_ms == 0 {
            return Err(CacheError::Malformed {
                reason: "length and lease ttl are required",
            });
        }
        if item.length > self.limits.max_object_bytes {
            return Err(CacheError::ObjectTooLarge {
                limit: self.limits.max_object_bytes,
                actual: item.length,
            });
        }
        if let Some(payload) = &item.payload {
            if payload.len() as u64 != item.length {
                return Err(CacheError::LengthMismatch {
                    expected: item.length,
                    actual: payload.len() as u64,
                });
            }
        }
        if item.generation != 0 {
            return self.continue_put(auth, now_ms, item);
        }
        let physical = encode_physical_key(&item.identity, &self.identity_limits)?;
        let logical = encode_logical_key(&item.identity, &self.identity_limits)?;
        if let Some(existing) = self.by_physical.get(&physical).copied() {
            let state = self.slots[&existing].life.state;
            return Err(CacheError::InvalidTransition {
                from: state,
                event: "reserve",
            });
        }
        let lease_deadline_ms = now_ms.saturating_add(item.lease_ttl_ms);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let mut life = Lifecycle::absent();
        life.apply(Event::Reserve)?;
        let mut region_id = None;
        if item.payload.is_some() {
            life.apply(Event::BeginWrite)?;
            region_id = Some(self.alloc_region());
        }
        let slot = Slot {
            identity: item.identity.clone(),
            physical: physical.clone(),
            logical: logical.clone(),
            life,
            length: item.length,
            checksum: item.checksum,
            bytes: item.payload.clone(),
            reserve_epoch: self.epoch,
            lease_deadline_ms,
            region_id,
            pins: Vec::new(),
            charges: Charges::NONE,
            fail: None,
        };
        self.slots.insert(generation, slot);
        self.index_add(physical, logical, generation);
        if let Err(err) = self.reconcile(generation) {
            self.remove_slot(generation);
            return Err(err);
        }
        let state = self.slots[&generation].life.state;
        Ok(ReserveOutcome {
            generation,
            state,
            region_id,
            lease_deadline_ms,
        })
    }

    fn continue_put(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &ReservePutItem,
    ) -> Result<ReserveOutcome, CacheError> {
        let payload = item.payload.clone().ok_or(CacheError::Malformed {
            reason: "put requires payload",
        })?;
        let physical = encode_physical_key(&item.identity, &self.identity_limits)?;
        let generation = item.generation;
        let (state, reserve_epoch, matches) = {
            let slot = self
                .slots
                .get(&generation)
                .ok_or(CacheError::UnknownGeneration)?;
            self.same_tenant(auth, &slot.identity)?;
            (
                slot.life.state,
                slot.reserve_epoch,
                slot.physical == physical
                    && slot.checksum == item.checksum
                    && slot.length == item.length,
            )
        };
        if !matches {
            return Err(CacheError::IdempotencyConflict);
        }
        if state != BlockState::Reserved {
            return Err(CacheError::InvalidTransition {
                from: state,
                event: "begin_write",
            });
        }
        if reserve_epoch != self.epoch {
            return Err(CacheError::FencingRejected {
                reason: "reservation epoch is stale",
            });
        }
        if self.registrations >= self.limits.max_registrations {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::Registrations,
            });
        }
        let region_id = self.alloc_region();
        {
            let slot = self
                .slots
                .get_mut(&generation)
                .ok_or(CacheError::UnknownGeneration)?;
            slot.life.apply(Event::BeginWrite)?;
            slot.bytes = Some(payload);
            slot.region_id = Some(region_id);
            slot.lease_deadline_ms = now_ms.saturating_add(item.lease_ttl_ms);
        }
        self.reconcile(generation)?;
        Ok(ReserveOutcome {
            generation,
            state: self.slots[&generation].life.state,
            region_id: Some(region_id),
            lease_deadline_ms: self.slots[&generation].lease_deadline_ms,
        })
    }

    fn commit_one(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &CommitItem,
    ) -> Result<CommitOutcome, CacheError> {
        let fp = fingerprint(&[
            &[3u8][..],
            &item.header.epoch.to_le_bytes(),
            &item.generation.to_le_bytes(),
            &item.length.to_le_bytes(),
            &item.checksum.0,
        ]);
        if let Some(prev) = self.replay(item.header.request_id, &fp)? {
            return match prev {
                IdemKind::Commit(result) => result,
                _ => Err(CacheError::IdempotencyConflict),
            };
        }
        self.preflight(&item.header, now_ms)?;
        self.ensure_idem_room(now_ms)?;
        let result = self.commit_new(auth, now_ms, item);
        self.remember(
            item.header.request_id,
            fp,
            now_ms,
            IdemKind::Commit(result.clone()),
        )?;
        result
    }

    fn commit_new(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &CommitItem,
    ) -> Result<CommitOutcome, CacheError> {
        self.check_header(&item.header, now_ms)?;
        let generation = item.generation;
        let (reserve_epoch, state, fail, declared_len, declared_sum, payload) = {
            let slot = self
                .slots
                .get(&generation)
                .ok_or(CacheError::UnknownGeneration)?;
            self.same_tenant(auth, &slot.identity)?;
            (
                slot.reserve_epoch,
                slot.life.state,
                slot.fail,
                slot.length,
                slot.checksum,
                slot.bytes.clone().unwrap_or_default(),
            )
        };
        if reserve_epoch != self.epoch {
            return Err(CacheError::FencingRejected {
                reason: "commit epoch is stale",
            });
        }
        if state == BlockState::Quarantined {
            return Err(fail_error(fail, state));
        }
        if state != BlockState::Writing {
            return Err(CacheError::NotExposed { state });
        }
        if declared_len != item.length {
            self.reject_write(generation)?;
            return Err(CacheError::LengthMismatch {
                expected: declared_len,
                actual: item.length,
            });
        }
        if item.checksum != declared_sum || verify_checksum(&payload, declared_sum).is_err() {
            self.reject_write(generation)?;
            return Err(CacheError::ChecksumMismatch);
        }
        self.apply_event(generation, Event::Commit, None)?;
        Ok(CommitOutcome {
            generation,
            length: item.length,
            checksum: declared_sum,
        })
    }

    fn reject_write(&mut self, generation: u64) -> Result<(), CacheError> {
        // Completion is known: the bytes were inspected and rejected.
        self.apply_event(generation, Event::ChecksumRejected, None)
    }

    fn get_one(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &GetItem,
    ) -> Result<GetHit, CacheError> {
        self.check_header(&item.header, now_ms)?;
        self.authorize(auth, Some(&item.identity))?;
        if item.pin_ttl_ms == 0 {
            return Err(CacheError::Malformed {
                reason: "pin ttl required",
            });
        }
        let generation = item.generation;
        let (visible, fail, state, region_id, payload, checksum, length, same_identity) = {
            let slot = self
                .slots
                .get(&generation)
                .ok_or(CacheError::UnknownGeneration)?;
            self.same_tenant(auth, &slot.identity)?;
            (
                slot.life.is_hit_visible(),
                slot.fail,
                slot.life.state,
                slot.region_id,
                slot.bytes.clone().unwrap_or_default(),
                slot.checksum,
                slot.length,
                slot.identity == item.identity,
            )
        };
        if !same_identity {
            return Err(CacheError::Incompatible);
        }
        if !visible {
            return Err(fail_error(fail, state));
        }
        let region_id = region_id.ok_or(CacheError::Malformed {
            reason: "missing region",
        })?;
        if payload.len() as u64 != length || verify_checksum(&payload, checksum).is_err() {
            self.apply_event(generation, Event::CorruptHold, Some(FailKind::Corrupt))?;
            return Err(CacheError::ChecksumMismatch);
        }
        let lease_deadline_ms = now_ms.saturating_add(item.pin_ttl_ms);
        {
            let slot = self
                .slots
                .get_mut(&generation)
                .ok_or(CacheError::UnknownGeneration)?;
            slot.life.apply(Event::ReaderPin)?;
            slot.pins.push(Pin {
                request_id: item.header.request_id,
                deadline_ms: lease_deadline_ms,
                stuck: false,
            });
        }
        Ok(GetHit {
            generation,
            length,
            checksum,
            region_id,
            lease_deadline_ms,
            payload,
        })
    }

    fn release_one(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &ReleaseCancelItem,
    ) -> Result<ReleaseOutcome, CacheError> {
        let fp = release_fingerprint(item);
        if let Some(prev) = self.replay(item.header.request_id, &fp)? {
            return match prev {
                IdemKind::Release(result) => result,
                _ => Err(CacheError::IdempotencyConflict),
            };
        }
        self.preflight(&item.header, now_ms)?;
        self.ensure_idem_room(now_ms)?;
        let result = self.release_new(auth, now_ms, item);
        self.remember(
            item.header.request_id,
            fp,
            now_ms,
            IdemKind::Release(result.clone()),
        )?;
        result
    }

    fn release_new(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &ReleaseCancelItem,
    ) -> Result<ReleaseOutcome, CacheError> {
        self.check_header(&item.header, now_ms)?;
        let generation = item.generation;
        let slot = self
            .slots
            .get(&generation)
            .ok_or(CacheError::UnknownGeneration)?;
        self.same_tenant(auth, &slot.identity)?;
        if let Some(pin_owner) = item.pin_owner {
            return self.release_pin(item, generation, pin_owner);
        }
        let fail = match item.attestation {
            Attestation::Drained => None,
            Attestation::Unknown | Attestation::KnownIdle => Some(FailKind::Cancelled),
        };
        let event = Event::WriterCancel(item.attestation);
        self.apply_event(generation, event, fail)?;
        self.outcome_for(generation, item.generation)
    }

    fn release_pin(
        &mut self,
        item: &ReleaseCancelItem,
        generation: u64,
        pin_owner: RequestId,
    ) -> Result<ReleaseOutcome, CacheError> {
        let region = item.region_id.ok_or(CacheError::Malformed {
            reason: "pin region required",
        })?;
        {
            let slot = self
                .slots
                .get_mut(&generation)
                .ok_or(CacheError::UnknownGeneration)?;
            if slot.region_id != Some(region) {
                return Err(CacheError::Malformed {
                    reason: "region mismatch",
                });
            }
            let pos = slot
                .pins
                .iter()
                .position(|pin| pin.request_id == pin_owner)
                .ok_or(CacheError::UnknownGeneration)?;
            match item.attestation {
                Attestation::Drained => {
                    slot.pins.remove(pos);
                    slot.life.apply(Event::ReaderUnpin(Attestation::Drained))?;
                }
                Attestation::Unknown | Attestation::KnownIdle => {
                    slot.pins[pos].stuck = true;
                    slot.life.apply(Event::ReaderUnpin(item.attestation))?;
                }
            }
        }
        self.maybe_finish(generation)?;
        self.outcome_for(generation, generation)
    }

    fn invalidate_one(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &InvalidateItem,
    ) -> Result<InvalidateOutcome, CacheError> {
        let fp = fingerprint(&[
            &[6u8][..],
            &item.header.epoch.to_le_bytes(),
            &item.generation.to_le_bytes(),
        ]);
        if let Some(prev) = self.replay(item.header.request_id, &fp)? {
            return match prev {
                IdemKind::Invalidate(result) => result,
                _ => Err(CacheError::IdempotencyConflict),
            };
        }
        self.preflight(&item.header, now_ms)?;
        self.ensure_idem_room(now_ms)?;
        let result = self.invalidate_new(auth, now_ms, item);
        self.remember(
            item.header.request_id,
            fp,
            now_ms,
            IdemKind::Invalidate(result.clone()),
        )?;
        result
    }

    fn invalidate_new(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        item: &InvalidateItem,
    ) -> Result<InvalidateOutcome, CacheError> {
        self.check_header(&item.header, now_ms)?;
        let generation = item.generation;
        let slot = self
            .slots
            .get(&generation)
            .ok_or(CacheError::UnknownGeneration)?;
        self.same_tenant(auth, &slot.identity)?;
        if !slot.life.is_hit_visible() {
            return Err(CacheError::NotExposed {
                state: slot.life.state,
            });
        }
        self.apply_event(generation, Event::BeginEvict, None)?;
        self.maybe_finish(generation)?;
        let state = self
            .slots
            .get(&generation)
            .map(|s| s.life.state)
            .unwrap_or(BlockState::Absent);
        Ok(InvalidateOutcome { generation, state })
    }

    fn reclaim_fenced_inner(
        &mut self,
        auth: &AuthContext,
        epoch: u64,
        deadline_ms: u64,
        generation: u64,
        observed_epoch: u64,
        now_ms: u64,
    ) -> Result<ReleaseOutcome, CacheError> {
        self.check_header(
            &gpucache_protocol::ItemHeader {
                request_id: RequestId::from_counter(1).map_err(|_| CacheError::Malformed {
                    reason: "request_id",
                })?,
                epoch,
                deadline_ms,
            },
            now_ms,
        )?;
        if observed_epoch != self.epoch {
            return Err(CacheError::FencingRejected {
                reason: "observed epoch is stale",
            });
        }
        let slot = self
            .slots
            .get(&generation)
            .ok_or(CacheError::UnknownGeneration)?;
        self.same_tenant(auth, &slot.identity)?;
        if !slot.life.fenced {
            return Err(CacheError::FencingRejected {
                reason: "reclaim requires a newer epoch",
            });
        }
        let stuck = slot.pins.iter().filter(|pin| pin.stuck).count() as u32;
        if stuck > 0 {
            self.apply_event(generation, Event::DropFencedRefs { count: stuck }, None)?;
            if let Some(slot) = self.slots.get_mut(&generation) {
                slot.pins.retain(|pin| !pin.stuck);
            }
        }
        let state = self
            .slots
            .get(&generation)
            .map(|s| s.life.state)
            .unwrap_or(BlockState::Absent);
        if matches!(state, BlockState::Quarantined | BlockState::Evicting) {
            self.apply_event(generation, Event::ReclaimFenced, None)?;
        }
        self.outcome_for(generation, generation)
    }

    fn maybe_finish(&mut self, generation: u64) -> Result<(), CacheError> {
        let ready = self.slots.get(&generation).map(|slot| {
            slot.life.state == BlockState::Evicting
                && slot.life.refs == 0
                && !slot.life.reuse_blocked
        });
        if ready.unwrap_or(false) {
            self.apply_event(generation, Event::FinishEvict, None)?;
        }
        Ok(())
    }

    fn outcome_for(&self, generation: u64, reported: u64) -> Result<ReleaseOutcome, CacheError> {
        let (state, quarantined) = match self.slots.get(&generation) {
            None => (BlockState::Absent, false),
            Some(slot) => (
                slot.life.state,
                slot.life.state == BlockState::Quarantined || slot.life.reuse_blocked,
            ),
        };
        Ok(ReleaseOutcome {
            generation: reported,
            state,
            quarantined,
        })
    }

    fn apply_event(
        &mut self,
        generation: u64,
        event: Event,
        fail: Option<FailKind>,
    ) -> Result<(), CacheError> {
        {
            let slot = self
                .slots
                .get_mut(&generation)
                .ok_or(CacheError::UnknownGeneration)?;
            slot.life.apply(event)?;
            if slot.life.state == BlockState::Quarantined {
                if let Some(kind) = fail {
                    slot.fail = Some(kind);
                }
            }
        }
        self.reconcile(generation)
    }

    fn poll(&mut self, now_ms: u64) {
        self.expire_idem(now_ms);
        let expired: Vec<u64> = self
            .slots
            .iter()
            .filter(|(_, slot)| {
                matches!(slot.life.state, BlockState::Reserved | BlockState::Writing)
                    && now_ms >= slot.lease_deadline_ms
            })
            .map(|(generation, _)| *generation)
            .collect();
        for generation in expired {
            let _ = self.apply_event(generation, Event::WriterTimeout, Some(FailKind::Timeout));
        }
        let generations: Vec<u64> = self.slots.keys().copied().collect();
        for generation in generations {
            if let Some(slot) = self.slots.get_mut(&generation) {
                let mut newly_stuck = false;
                for pin in &mut slot.pins {
                    if !pin.stuck && now_ms >= pin.deadline_ms {
                        pin.stuck = true;
                        newly_stuck = true;
                    }
                }
                if newly_stuck {
                    let _ = slot.life.apply(Event::ReaderLeaseExpire);
                }
            }
        }
    }

    fn authorize(
        &self,
        auth: &AuthContext,
        identity: Option<&CacheIdentity>,
    ) -> Result<(), CacheError> {
        let policy = self
            .policies
            .get(auth.tenant.as_bytes())
            .ok_or(CacheError::Unauthorized)?;
        if !fingerprints_equal(&policy.credential_fingerprint, &auth.credential_fingerprint) {
            return Err(CacheError::Unauthorized);
        }
        if policy.trust_domain != auth.trust_domain {
            return Err(CacheError::Unauthorized);
        }
        if let Some(identity) = identity {
            self.same_tenant(auth, identity)?;
        }
        Ok(())
    }

    fn same_tenant(&self, auth: &AuthContext, identity: &CacheIdentity) -> Result<(), CacheError> {
        if identity.tenant != auth.tenant || identity.trust_domain != auth.trust_domain {
            return Err(CacheError::Unauthorized);
        }
        Ok(())
    }

    fn preflight(
        &self,
        header: &gpucache_protocol::ItemHeader,
        now_ms: u64,
    ) -> Result<(), CacheError> {
        self.check_header(header, now_ms)
    }

    fn check_header(
        &self,
        header: &gpucache_protocol::ItemHeader,
        now_ms: u64,
    ) -> Result<(), CacheError> {
        if header.epoch != self.epoch {
            return Err(CacheError::EpochMismatch {
                store: self.epoch,
                request: header.epoch,
            });
        }
        ensure_deadline(now_ms, header.deadline_ms)
    }

    fn alloc_region(&mut self) -> u64 {
        let id = self.next_region;
        self.next_region = self.next_region.saturating_add(1);
        id
    }

    fn index_add(&mut self, physical: Vec<u8>, logical: Vec<u8>, generation: u64) {
        self.by_physical.insert(physical.clone(), generation);
        let entries = self.logical.entry(logical).or_default();
        if !entries.contains(&physical) {
            entries.push(physical);
        }
    }

    fn remove_slot(&mut self, generation: u64) {
        let Some(slot) = self.slots.remove(&generation) else {
            return;
        };
        self.by_physical.remove(&slot.physical);
        if let Some(entries) = self.logical.get_mut(&slot.logical) {
            entries.retain(|key| key != &slot.physical);
            if entries.is_empty() {
                self.logical.remove(&slot.logical);
            }
        }
    }

    fn reconcile(&mut self, generation: u64) -> Result<(), CacheError> {
        let state = self
            .slots
            .get(&generation)
            .ok_or(CacheError::UnknownGeneration)?
            .life
            .state;
        self.apply_charge_delta(generation, charge_target(state))?;
        if state == BlockState::Absent {
            self.remove_slot(generation);
        }
        Ok(())
    }

    fn apply_charge_delta(
        &mut self,
        generation: u64,
        target: ChargeTarget,
    ) -> Result<(), CacheError> {
        let slot = self
            .slots
            .get(&generation)
            .ok_or(CacheError::UnknownGeneration)?;
        let tenant = slot.identity.tenant.as_bytes().to_vec();
        let len = slot.length;
        let current = slot.charges;
        let retained_delta = delta(current.retained, target.retained, len);
        let inflight_delta = delta(current.inflight, target.inflight, len);
        let request_delta: i32 = match (current.inflight, target.inflight) {
            (false, true) => 1,
            (true, false) => -1,
            _ => 0,
        };
        let registration_delta: i32 = match (current.registration, target.registration) {
            (false, true) => 1,
            (true, false) => -1,
            _ => 0,
        };
        let new_retained = add_i128(self.retained_bytes, retained_delta)?;
        let new_requests = add_i32(self.inflight_requests, request_delta)?;
        let new_regs = add_i32(self.registrations, registration_delta)?;
        let new_tenant_retained = add_i128(
            self.tenant_retained.get(&tenant).copied().unwrap_or(0),
            retained_delta,
        )?;
        let new_tenant_inflight = add_i128(
            self.tenant_inflight.get(&tenant).copied().unwrap_or(0),
            inflight_delta,
        )?;
        if target.retained && !current.retained {
            if new_retained > self.limits.max_retained_bytes {
                return Err(CacheError::QuotaExceeded {
                    kind: QuotaKind::RetainedBytes,
                });
            }
            if new_tenant_retained > self.limits.max_retained_bytes_per_tenant {
                return Err(CacheError::QuotaExceeded {
                    kind: QuotaKind::TenantRetainedBytes,
                });
            }
        }
        if target.inflight && !current.inflight {
            if new_requests > u64::from(self.limits.max_in_flight_requests) {
                return Err(CacheError::QuotaExceeded {
                    kind: QuotaKind::InFlightRequests,
                });
            }
            if new_tenant_inflight > self.limits.max_in_flight_bytes_per_tenant {
                return Err(CacheError::QuotaExceeded {
                    kind: QuotaKind::InFlightBytes,
                });
            }
        }
        if target.registration
            && !current.registration
            && new_regs > u64::from(self.limits.max_registrations)
        {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::Registrations,
            });
        }
        self.retained_bytes = new_retained;
        self.inflight_requests = u32::try_from(new_requests).unwrap_or(u32::MAX);
        self.registrations = u32::try_from(new_regs).unwrap_or(u32::MAX);
        set_counter(
            &mut self.tenant_retained,
            tenant.clone(),
            new_tenant_retained,
        );
        set_counter(&mut self.tenant_inflight, tenant, new_tenant_inflight);
        if let Some(slot) = self.slots.get_mut(&generation) {
            slot.charges = Charges {
                retained: target.retained,
                inflight: target.inflight,
                registration: target.registration,
            };
        }
        Ok(())
    }

    fn replay(
        &self,
        id: RequestId,
        fingerprint: &Checksum,
    ) -> Result<Option<IdemKind>, CacheError> {
        match self.idem.get(&id) {
            None => Ok(None),
            Some(record) if &record.fingerprint == fingerprint => Ok(Some(record.kind.clone())),
            Some(_) => Err(CacheError::IdempotencyConflict),
        }
    }

    fn ensure_idem_room(&mut self, now_ms: u64) -> Result<(), CacheError> {
        self.expire_idem(now_ms);
        if self.idem.len() >= self.limits.max_idempotency_entries as usize {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::IdempotencyTable,
            });
        }
        Ok(())
    }

    fn remember(
        &mut self,
        id: RequestId,
        fingerprint: Checksum,
        now_ms: u64,
        kind: IdemKind,
    ) -> Result<(), CacheError> {
        self.idem.insert(
            id,
            IdemRecord {
                fingerprint,
                inserted_ms: now_ms,
                kind,
            },
        );
        Ok(())
    }

    fn expire_idem(&mut self, now_ms: u64) {
        let ttl = self.limits.idempotency_ttl_ms;
        self.idem
            .retain(|_, record| now_ms.saturating_sub(record.inserted_ms) < ttl);
    }
}

fn charge_target(state: BlockState) -> ChargeTarget {
    match state {
        BlockState::Absent => ChargeTarget {
            retained: false,
            inflight: false,
            registration: false,
        },
        BlockState::Reserved => ChargeTarget {
            retained: true,
            inflight: true,
            registration: false,
        },
        BlockState::Writing => ChargeTarget {
            retained: true,
            inflight: true,
            registration: true,
        },
        BlockState::Committed | BlockState::Evicting | BlockState::Quarantined => ChargeTarget {
            retained: true,
            inflight: false,
            registration: true,
        },
    }
}

fn delta(from: bool, to: bool, len: u64) -> i128 {
    match (from, to) {
        (false, true) => i128::from(len),
        (true, false) => -i128::from(len),
        _ => 0,
    }
}

fn add_i128(base: u64, delta_value: i128) -> Result<u64, CacheError> {
    let next = i128::from(base) + delta_value;
    u64::try_from(next).map_err(|_| CacheError::Malformed {
        reason: "accounting underflow",
    })
}

fn add_i32(base: u32, delta_value: i32) -> Result<u64, CacheError> {
    let next = i64::from(base) + i64::from(delta_value);
    u64::try_from(next).map_err(|_| CacheError::Malformed {
        reason: "accounting underflow",
    })
}

fn set_counter(map: &mut HashMap<Vec<u8>, u64>, key: Vec<u8>, value: u64) {
    if value == 0 {
        map.remove(&key);
    } else {
        map.insert(key, value);
    }
}

fn miss_reason(state: BlockState) -> MissReason {
    match state {
        BlockState::Absent => MissReason::Absent,
        BlockState::Reserved | BlockState::Writing => MissReason::NotCommitted,
        BlockState::Committed => MissReason::NotCommitted,
        BlockState::Evicting => MissReason::Evicting,
        BlockState::Quarantined => MissReason::Quarantined,
    }
}

fn fail_error(fail: Option<FailKind>, state: BlockState) -> CacheError {
    match fail {
        Some(FailKind::Timeout) => CacheError::TimedOut,
        Some(FailKind::Cancelled) => CacheError::Cancelled,
        Some(FailKind::Fenced) => CacheError::FencingRejected {
            reason: "writer fenced",
        },
        Some(FailKind::Corrupt) => CacheError::ChecksumMismatch,
        None => CacheError::NotExposed { state },
    }
}

fn request_ids<I>(ids: I) -> Vec<RequestId>
where
    I: Iterator<Item = RequestId>,
{
    ids.collect()
}

fn fingerprint(parts: &[&[u8]]) -> Checksum {
    let mut buf = Vec::new();
    for part in parts {
        let len = u32::try_from(part.len()).unwrap_or(u32::MAX);
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(part);
    }
    content_checksum(&buf)
}

fn reserve_fingerprint(item: &ReservePutItem) -> Result<Checksum, CacheError> {
    let identity = item.identity.to_bytes()?;
    let payload_sum = item
        .payload
        .as_ref()
        .map(|bytes| content_checksum(bytes).0)
        .unwrap_or([0u8; 32]);
    Ok(fingerprint(&[
        &[2u8][..],
        &item.header.epoch.to_le_bytes(),
        &item.generation.to_le_bytes(),
        &item.length.to_le_bytes(),
        &item.checksum.0,
        &identity,
        &payload_sum,
        &item.lease_ttl_ms.to_le_bytes(),
    ]))
}

fn release_fingerprint(item: &ReleaseCancelItem) -> Checksum {
    let region = item.region_id.unwrap_or(0).to_le_bytes();
    let pin = item.pin_owner.map(|id| id.0).unwrap_or([0u8; 16]);
    let att = match item.attestation {
        Attestation::KnownIdle => 1u8,
        Attestation::Unknown => 2,
        Attestation::Drained => 3,
    };
    fingerprint(&[
        &[5u8][..],
        &item.header.epoch.to_le_bytes(),
        &item.generation.to_le_bytes(),
        &region,
        &pin,
        &[att],
    ])
}
