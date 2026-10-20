// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{MockObjectStore, ObjectMeta};
use gpucache_core_types::content_checksum;
use std::cell::Cell;

struct FaultStore {
    inner: MockObjectStore,
    head_fault: usize,
    heads: Cell<usize>,
    delete_fault: bool,
    deletes: usize,
}

impl ObjectStore for FaultStore {
    fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError> {
        self.inner.put(key, body)
    }
    fn head(&self, key: &str) -> Result<ObjectMeta, CacheError> {
        if key == "a" {
            self.heads.set(self.heads.get() + 1);
            if self.heads.get() == self.head_fault {
                return Err(CacheError::TimedOut);
            }
        }
        self.inner.head(key)
    }
    fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
        self.inner.range_get(key, offset, length)
    }
    fn delete(&mut self, key: &str) -> Result<(), CacheError> {
        self.deletes += 1;
        if key == "a" && self.delete_fault {
            self.delete_fault = false;
            return Err(CacheError::TimedOut);
        }
        self.inner.delete(key)
    }
}

fn record(key: &str, generation: u64, written_ms: u64) -> ObjectRecord {
    ObjectRecord {
        key: key.to_string(),
        written_ms,
        checksum: content_checksum(b"body"),
        tenant: b"tenant".to_vec(),
        key_generation: generation,
        manifest_version: MANIFEST_VERSION,
    }
}

#[test]
fn reclaim_head_delete_and_verification_failures_preserve_work_and_allow_fair_progress() {
    for (head_fault, delete_fault) in [(1, false), (0, true), (2, false)] {
        let mut store = FaultStore {
            inner: MockObjectStore::default(),
            head_fault,
            heads: Cell::new(0),
            delete_fault,
            deletes: 0,
        };
        let mut ops = ColdOps::new(Retention::new(10).unwrap());
        let generation = ops.keys_mut().issue(b"tenant").unwrap();
        for key in ["a", "b"] {
            store.put(key, b"body").unwrap();
            ops.record_put(record(key, generation, 0)).unwrap();
        }
        assert_eq!(ops.schedule_expired(10), 2);
        assert_eq!(ops.reclaim_one(&mut store), Err(CacheError::TimedOut));
        assert_eq!(ops.reclaim.len(), 2);
        assert_eq!(ops.records.len(), 2);
        assert_eq!(ops.schedule_expired(10), 0);
        assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Verified);
        assert!(store.inner.head("b").is_err());
        assert!(ops.records.contains_key("a"));
        assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Verified);
        assert!(store.inner.head("a").is_err());
        assert!(ops.records.is_empty());
        assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Idle);
    }
}

#[test]
fn mismatched_reclaim_record_never_deletes_unexpected_content() {
    let mut store = FaultStore {
        inner: MockObjectStore::default(),
        head_fault: 0,
        heads: Cell::new(0),
        delete_fault: false,
        deletes: 0,
    };
    let mut ops = ColdOps::new(Retention::new(10).unwrap());
    let generation = ops.keys_mut().issue(b"tenant").unwrap();
    store.put("a", b"different-owner-or-corrupt-body").unwrap();
    store.put("b", b"body").unwrap();
    ops.record_put(record("a", generation, 0)).unwrap();
    ops.record_put(record("b", generation, 0)).unwrap();
    ops.schedule_expired(10);
    assert_eq!(
        ops.reclaim_one(&mut store),
        Err(CacheError::ChecksumMismatch)
    );
    assert_eq!(store.deletes, 0);
    assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Verified);
    assert_eq!(
        ops.reclaim_one(&mut store),
        Err(CacheError::ChecksumMismatch)
    );
    assert_eq!(store.deletes, 1);
    assert_eq!(ops.records.len(), 1);
    assert_eq!(ops.reclaim.len(), 1);
    assert_eq!(
        store.inner.head("a").unwrap().checksum,
        content_checksum(b"different-owner-or-corrupt-body")
    );
}

#[test]
fn renewing_a_record_invalidates_its_previously_scheduled_expiration() {
    let mut store = MockObjectStore::default();
    let mut ops = ColdOps::new(Retention::new(10).unwrap());
    let generation = ops.keys_mut().issue(b"tenant").unwrap();
    store.put("a", b"body").unwrap();
    ops.record_put(record("a", generation, 0)).unwrap();
    assert_eq!(ops.schedule_expired(10), 1);
    ops.record_put(record("a", generation, 10)).unwrap();
    assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Idle);
    assert!(store.head("a").is_ok());
    assert_eq!(ops.schedule_expired(19), 0);
    assert_eq!(ops.schedule_expired(20), 1);
    assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Verified);
}

#[test]
fn cold_key_tenant_and_history_admission_is_bounded_and_atomic() {
    let mut keys = ColdKeys::with_limits(1, 2).unwrap();
    let first = keys.issue(b"tenant").unwrap();
    assert!(matches!(
        keys.issue(b"other"),
        Err(CacheError::QuotaExceeded { .. })
    ));
    assert_eq!((keys.next.len(), keys.items.len()), (1, 1));
    assert!(!keys.next.contains_key(b"other".as_slice()));
    let second = keys.rotate(b"tenant").unwrap();
    keys.retire(b"tenant", first).unwrap();
    for _ in 0..128 {
        assert!(matches!(
            keys.rotate(b"tenant"),
            Err(CacheError::QuotaExceeded { .. })
        ));
        assert_eq!((keys.next.len(), keys.items.len()), (1, 2));
        assert_eq!(keys.next[b"tenant".as_slice()], second);
        assert_eq!(keys.allow_write(b"tenant", second), Ok(()));
        assert_eq!(
            keys.allow_read(b"tenant", first),
            Err(CacheError::Unauthorized)
        );
    }
    assert_eq!(keys.issue(b"tenant"), Err(CacheError::IdempotencyConflict));
    assert!(ColdKeys::with_limits(0, 1).is_err());
    assert!(ColdKeys::with_limits(1, 0).is_err());
    assert!(ColdKeys::with_limits(MAX_COLD_TENANTS + 1, 1).is_err());
    assert!(ColdKeys::with_limits(1, MAX_COLD_KEY_HISTORY + 1).is_err());
}

#[test]
fn exhausted_cold_key_generation_does_not_demote_current_authority() {
    let mut keys = ColdKeys::with_limits(1, 2).unwrap();
    let current = keys.issue(b"tenant").unwrap();
    keys.next.insert(b"tenant".to_vec(), u64::MAX);
    assert_eq!(
        keys.rotate(b"tenant"),
        Err(CacheError::FencingRejected {
            reason: "cold key generation exhausted"
        })
    );
    assert_eq!(keys.allow_write(b"tenant", current), Ok(()));
    assert_eq!(keys.items.len(), 1);
    assert_eq!(keys.next[b"tenant".as_slice()], u64::MAX);
}

#[test]
fn full_record_table_keeps_failed_reclaim_credit_and_allows_replacement() {
    let mut store = FaultStore {
        inner: MockObjectStore::default(),
        head_fault: 1,
        heads: Cell::new(0),
        delete_fault: false,
        deletes: 0,
    };
    let retention = Retention::new(10).unwrap();
    let mut ops = ColdOps::with_capacity(retention, 2).unwrap();
    let generation = ops.keys_mut().issue(b"tenant").unwrap();
    for key in ["a", "b"] {
        store.put(key, b"body").unwrap();
        ops.record_put(record(key, generation, 0)).unwrap();
    }
    assert_eq!(ops.schedule_expired(10), 2);
    assert_eq!(ops.reclaim_one(&mut store), Err(CacheError::TimedOut));
    let scheduled = ops.reclaim.clone();
    for _ in 0..128 {
        assert!(matches!(
            ops.record_put(record("c", generation, 0)),
            Err(CacheError::QuotaExceeded { .. })
        ));
        assert_eq!(ops.records.len(), 2);
        assert_eq!(ops.reclaim, scheduled);
        assert_eq!(ops.schedule_expired(10), 0);
    }
    // An authenticated replacement does not grow metadata at capacity and must
    // cancel its stale expiry. Failed admissions above did not alter this queue.
    ops.record_put(record("a", generation, 100)).unwrap();
    assert_eq!(ops.records.len(), 2);
    assert_eq!(ops.reclaim.len(), 1);
    assert_eq!(ops.reclaim_one(&mut store).unwrap(), ReclaimStep::Verified);
    assert!(store.head("a").is_ok());
    ops.record_put(record("c", generation, 100)).unwrap();
    assert_eq!(ops.records.len(), 2);
    assert_eq!(ops.schedule_expired(110), 2);
    assert_eq!(ops.schedule_expired(110), 0);
    assert_eq!(ops.reclaim.len(), 2);
    assert!(ColdOps::with_capacity(retention, 0).is_err());
    assert!(ColdOps::with_capacity(retention, MAX_COLD_RECORDS + 1).is_err());
}
