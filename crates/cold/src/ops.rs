// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! G4 retention, manifest checks, async reclaim, tenant key generations,
//! and a backfill byte budget.
//!
//! Live RustFS stays outside this module. A slow or unavailable cold tier
//! returns [`RefillOutcome::Missing`] without reading the store, so the
//! caller recomputes and is not blocked. Expired objects are deleted only
//! after the following head shows them gone.

use std::collections::{BTreeMap, VecDeque};

use gpucache_core_types::{CacheError, CacheIdentity, Checksum, QuotaKind};

use crate::pack::{key_ok, ObjectStore, MANIFEST_VERSION};
use crate::refill::{refill_generation, refill_matching, RefillBudget, RefillOutcome};
use crate::sink::{AsyncSink, ColdJob};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    max_age_ms: u64,
}

impl Retention {
    pub fn new(max_age_ms: u64) -> Result<Self, CacheError> {
        if max_age_ms == 0 {
            return Err(CacheError::Malformed {
                reason: "retention",
            });
        }
        Ok(Self { max_age_ms })
    }

    pub fn expired(&self, written_ms: u64, now_ms: u64) -> bool {
        now_ms.saturating_sub(written_ms) >= self.max_age_ms
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColdAvailability {
    Ready,
    Slow,
    Unavailable,
}

/// Refill only for the complete expected identity from an authenticated caller.
/// The historical name does not provide a tenant-only or unauthenticated path.
pub fn refill_for_tenant(
    store: &dyn ObjectStore,
    manifest_key: &str,
    generation: u64,
    budget: &RefillBudget,
    availability: ColdAvailability,
    expected: &CacheIdentity,
) -> Result<RefillOutcome, CacheError> {
    match availability {
        ColdAvailability::Slow | ColdAvailability::Unavailable => Ok(RefillOutcome::Missing),
        ColdAvailability::Ready => {
            refill_matching(store, manifest_key, generation, budget, expected)
        }
    }
}

/// Availability wrapper around the same mandatory identity-matching refill.
pub fn refill_degraded(
    store: &dyn ObjectStore,
    manifest_key: &str,
    generation: u64,
    budget: &RefillBudget,
    availability: ColdAvailability,
    expected: &CacheIdentity,
) -> Result<RefillOutcome, CacheError> {
    match availability {
        ColdAvailability::Slow | ColdAvailability::Unavailable => Ok(RefillOutcome::Missing),
        ColdAvailability::Ready => {
            refill_generation(store, manifest_key, generation, budget, expected)
        }
    }
}

pub fn verify_deleted(store: &dyn ObjectStore, key: &str) -> Result<(), CacheError> {
    if !key_ok(key) {
        return Err(CacheError::Malformed { reason: "cold key" });
    }
    match store.head(key) {
        Err(CacheError::Storage {
            reason: "object missing",
        }) => Ok(()),
        Ok(_) => Err(CacheError::Storage {
            reason: "delete unverified",
        }),
        Err(err) => Err(err),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRecord {
    pub key: String,
    pub written_ms: u64,
    pub checksum: Checksum,
    pub tenant: Vec<u8>,
    pub key_generation: u64,
    pub manifest_version: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimStep {
    Idle,
    Verified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyState {
    Current,
    Overlap,
    Retired,
}

struct ColdKey {
    tenant: Vec<u8>,
    generation: u64,
    state: KeyState,
}

pub struct ColdKeys {
    items: Vec<ColdKey>,
    next: BTreeMap<Vec<u8>, u64>,
}

impl ColdKeys {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            next: BTreeMap::new(),
        }
    }

    pub fn issue(&mut self, tenant: &[u8]) -> Result<u64, CacheError> {
        self.ensure_tenant(tenant)?;
        if self.next.get(tenant).copied().unwrap_or(0) != 0 {
            return Err(CacheError::IdempotencyConflict);
        }
        Ok(self.insert(tenant, KeyState::Current))
    }

    pub fn rotate(&mut self, tenant: &[u8]) -> Result<u64, CacheError> {
        self.ensure_tenant(tenant)?;
        if !self
            .items
            .iter()
            .any(|item| item.tenant.as_slice() == tenant && item.state == KeyState::Current)
        {
            return Err(CacheError::UnknownGeneration);
        }
        for item in &mut self.items {
            if item.tenant.as_slice() == tenant && item.state == KeyState::Current {
                item.state = KeyState::Overlap;
            }
        }
        Ok(self.insert(tenant, KeyState::Current))
    }

    pub fn retire(&mut self, tenant: &[u8], generation: u64) -> Result<(), CacheError> {
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.tenant.as_slice() == tenant && item.generation == generation)
        else {
            return Err(CacheError::UnknownGeneration);
        };
        if item.state != KeyState::Overlap {
            return Err(CacheError::FencingRejected { reason: "secret" });
        }
        item.state = KeyState::Retired;
        Ok(())
    }

    pub fn allow_write(&self, tenant: &[u8], generation: u64) -> Result<(), CacheError> {
        self.allow(tenant, generation, true)
    }

    pub fn allow_read(&self, tenant: &[u8], generation: u64) -> Result<(), CacheError> {
        self.allow(tenant, generation, false)
    }

    fn allow(&self, tenant: &[u8], generation: u64, write: bool) -> Result<(), CacheError> {
        let Some(item) = self
            .items
            .iter()
            .find(|item| item.tenant.as_slice() == tenant && item.generation == generation)
        else {
            return Err(CacheError::Unauthorized);
        };
        let ok = match item.state {
            KeyState::Current => true,
            KeyState::Overlap => !write,
            KeyState::Retired => false,
        };
        if ok {
            Ok(())
        } else {
            Err(CacheError::Unauthorized)
        }
    }

    fn ensure_tenant(&self, tenant: &[u8]) -> Result<(), CacheError> {
        if tenant.is_empty() || tenant.len() > 128 {
            return Err(CacheError::Malformed { reason: "cold key" });
        }
        Ok(())
    }

    fn insert(&mut self, tenant: &[u8], state: KeyState) -> u64 {
        let generation = self
            .next
            .get(tenant)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.next.insert(tenant.to_vec(), generation);
        self.items.push(ColdKey {
            tenant: tenant.to_vec(),
            generation,
            state,
        });
        generation
    }
}

impl Default for ColdKeys {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ColdOps {
    retention: Retention,
    records: BTreeMap<String, ObjectRecord>,
    reclaim: VecDeque<String>,
    keys: ColdKeys,
}

impl ColdOps {
    pub fn new(retention: Retention) -> Self {
        Self {
            retention,
            records: BTreeMap::new(),
            reclaim: VecDeque::new(),
            keys: ColdKeys::new(),
        }
    }

    pub fn keys(&self) -> &ColdKeys {
        &self.keys
    }

    pub fn keys_mut(&mut self) -> &mut ColdKeys {
        &mut self.keys
    }

    pub fn record_put(&mut self, record: ObjectRecord) -> Result<(), CacheError> {
        if !key_ok(&record.key) || record.tenant.is_empty() {
            return Err(CacheError::Malformed { reason: "cold key" });
        }
        if record.manifest_version != MANIFEST_VERSION {
            return Err(CacheError::Storage {
                reason: "store corrupt",
            });
        }
        self.keys
            .allow_write(&record.tenant, record.key_generation)?;
        self.records.insert(record.key.clone(), record);
        Ok(())
    }

    pub fn allow_read(&self, tenant: &[u8], generation: u64) -> Result<(), CacheError> {
        self.keys.allow_read(tenant, generation)
    }

    pub fn schedule_expired(&mut self, now_ms: u64) -> usize {
        let mut added = 0usize;
        let due: Vec<String> = self
            .records
            .iter()
            .filter(|(_, record)| self.retention.expired(record.written_ms, now_ms))
            .map(|(key, _)| key.clone())
            .filter(|key| !self.reclaim.contains(key))
            .collect();
        for key in due {
            self.reclaim.push_back(key);
            added = added.saturating_add(1);
        }
        added
    }

    pub fn reclaim_one(&mut self, store: &mut dyn ObjectStore) -> Result<ReclaimStep, CacheError> {
        let Some(key) = self.reclaim.pop_front() else {
            return Ok(ReclaimStep::Idle);
        };
        let Some(record) = self.records.get(&key).cloned() else {
            return Ok(ReclaimStep::Idle);
        };
        match store.head(&key) {
            Err(CacheError::Storage {
                reason: "object missing",
            }) => {
                self.records.remove(&key);
                return Ok(ReclaimStep::Verified);
            }
            Err(err) => return Err(err),
            Ok(meta) if meta.checksum != record.checksum => {
                store.delete(&key)?;
                verify_deleted(store, &key)?;
                self.records.remove(&key);
                return Err(CacheError::ChecksumMismatch);
            }
            Ok(_) => {}
        }
        store.delete(&key)?;
        if let Err(err) = verify_deleted(store, &key) {
            self.reclaim.push_front(key);
            return Err(err);
        }
        self.records.remove(&key);
        Ok(ReclaimStep::Verified)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackfillLimit {
    max_bytes: u64,
    used: u64,
}

impl BackfillLimit {
    pub fn new(max_bytes: u64) -> Result<Self, CacheError> {
        if max_bytes == 0 {
            return Err(CacheError::Malformed { reason: "backfill" });
        }
        Ok(Self { max_bytes, used: 0 })
    }

    pub fn remaining(&self) -> u64 {
        self.max_bytes.saturating_sub(self.used)
    }

    pub fn used(&self) -> u64 {
        self.used
    }
}

pub fn enqueue_backfill(
    sink: &mut AsyncSink,
    job: ColdJob,
    limit: &mut BackfillLimit,
) -> Result<(), CacheError> {
    let weight = job.byte_weight();
    if weight == 0 {
        return Err(CacheError::Malformed { reason: "backfill" });
    }
    if weight > limit.remaining() {
        return Err(CacheError::QuotaExceeded {
            kind: QuotaKind::InFlightBytes,
        });
    }
    sink.enqueue(job)?;
    limit.used = limit.used.saturating_add(weight);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{pack_blocks, PackedBlock};
    use crate::MockObjectStore;
    use gpucache_core_types::{content_checksum, synthetic_identity};

    struct StickyStore {
        inner: MockObjectStore,
    }

    impl ObjectStore for StickyStore {
        fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError> {
            self.inner.put(key, body)
        }
        fn head(&self, key: &str) -> Result<crate::pack::ObjectMeta, CacheError> {
            self.inner.head(key)
        }
        fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
            self.inner.range_get(key, offset, length)
        }
        fn delete(&mut self, _key: &str) -> Result<(), CacheError> {
            Ok(())
        }
    }

    fn record(key: &str, generation: u64, written_ms: u64) -> ObjectRecord {
        ObjectRecord {
            key: key.to_string(),
            written_ms,
            checksum: content_checksum(b"body"),
            tenant: b"tenant-a".to_vec(),
            key_generation: generation,
            manifest_version: MANIFEST_VERSION,
        }
    }

    #[test]
    fn retention_reclaim_verifies_delete_and_a_lie_is_not_success() {
        let retention = Retention::new(100).unwrap();
        assert!(!retention.expired(0, 99));
        assert!(retention.expired(0, 100));
        assert!(!retention.expired(50, 40));
        let mut ops = ColdOps::new(retention);
        let generation = ops.keys_mut().issue(b"tenant-a").unwrap();
        let mut store = MockObjectStore::default();
        store.put("ns/object", b"body").unwrap();
        ops.record_put(record("ns/object", generation, 0)).unwrap();
        assert_eq!(ops.schedule_expired(50), 0);
        assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Idle);
        assert_eq!(ops.schedule_expired(100), 1);
        assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Verified);
        assert!(store.head("ns/object").is_err());

        let mut sticky = StickyStore {
            inner: MockObjectStore::default(),
        };
        sticky.put("ns/stuck", b"body").unwrap();
        ops.record_put(record("ns/stuck", generation, 0)).unwrap();
        ops.schedule_expired(200);
        assert_eq!(
            ops.reclaim_one(&mut sticky).unwrap_err(),
            CacheError::Storage {
                reason: "delete unverified"
            }
        );
        assert!(sticky.head("ns/stuck").is_ok());
    }

    #[test]
    fn retired_cold_key_cannot_read_or_write() {
        let mut keys = ColdKeys::new();
        let first = keys.issue(b"tenant-a").unwrap();
        let second = keys.rotate(b"tenant-a").unwrap();
        keys.allow_read(b"tenant-a", first).unwrap();
        assert_eq!(
            keys.allow_write(b"tenant-a", first).unwrap_err(),
            CacheError::Unauthorized
        );
        keys.retire(b"tenant-a", first).unwrap();
        assert_eq!(
            keys.allow_read(b"tenant-a", first).unwrap_err(),
            CacheError::Unauthorized
        );
        keys.allow_write(b"tenant-a", second).unwrap();
        let mut ops = ColdOps::new(Retention::new(10).unwrap());
        let generation = ops.keys_mut().issue(b"tenant-b").unwrap();
        let mut bad = record("ns/object", generation, 0);
        bad.manifest_version = 9;
        assert!(ops.record_put(bad).is_err());
    }

    #[test]
    fn backfill_stops_at_the_byte_budget() {
        let payload = vec![1u8; 8];
        let mut identity = synthetic_identity(1, 1);
        identity.prefix_chain[0].0[0] = 1;
        let block = PackedBlock {
            generation: 1,
            identity,
            checksum: content_checksum(&payload),
            payload,
        };
        let pack = pack_blocks("bf", &[block]).unwrap();
        let job = ColdJob::from_pack(&pack);
        let weight = job.byte_weight();
        let mut sink = AsyncSink::new(4, weight.saturating_mul(4)).unwrap();
        let mut limit = BackfillLimit::new(weight).unwrap();
        enqueue_backfill(&mut sink, job.clone(), &mut limit).unwrap();
        assert_eq!(
            enqueue_backfill(&mut sink, job, &mut limit).unwrap_err(),
            CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            }
        );
        assert_eq!(sink.queued(), 1);
        assert_eq!(limit.used(), weight);
    }

    #[test]
    fn slow_or_down_rustfs_is_a_miss_and_does_not_call_the_store() {
        let mut store = MockObjectStore::default();
        store.put("ns/manifest", b"not-a-manifest").unwrap();
        let budget = RefillBudget::new(1024, 1).unwrap();
        for availability in [ColdAvailability::Slow, ColdAvailability::Unavailable] {
            let outcome = refill_degraded(
                &store,
                "ns/manifest",
                1,
                &budget,
                availability,
                &synthetic_identity(1, 1),
            )
            .unwrap();
            assert_eq!(outcome, RefillOutcome::Missing);
        }
        assert!(store.head("ns/manifest").is_ok());
    }

    struct ClosedStore;

    impl ObjectStore for ClosedStore {
        fn put(&mut self, _key: &str, _body: &[u8]) -> Result<(), CacheError> {
            Err(CacheError::Storage {
                reason: "object store",
            })
        }
        fn head(&self, _key: &str) -> Result<crate::pack::ObjectMeta, CacheError> {
            Err(CacheError::Storage {
                reason: "object store",
            })
        }
        fn range_get(&self, _key: &str, _offset: u64, _length: u64) -> Result<Vec<u8>, CacheError> {
            Err(CacheError::Storage {
                reason: "object store",
            })
        }
        fn delete(&mut self, _key: &str) -> Result<(), CacheError> {
            Err(CacheError::Storage {
                reason: "object store",
            })
        }
    }

    #[test]
    fn degraded_tenant_refill_does_not_touch_the_store() {
        let budget = RefillBudget::new(64, 1).unwrap();
        let store = ClosedStore;
        for availability in [ColdAvailability::Slow, ColdAvailability::Unavailable] {
            assert_eq!(
                refill_for_tenant(
                    &store,
                    "ns/manifest",
                    1,
                    &budget,
                    availability,
                    &synthetic_identity(1, 1),
                )
                .unwrap(),
                RefillOutcome::Missing
            );
        }
    }
}
