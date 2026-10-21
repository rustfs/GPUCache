// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;

use gpucache_cold::{
    pack_blocks, refill_degraded, refill_for_tenant, refill_generation, refill_matching,
    ColdAvailability, MockObjectStore, ObjectMeta, ObjectStore, PackedBlock, RefillBudget,
    RefillOutcome,
};
use gpucache_core_types::{content_checksum, synthetic_identity, CacheError, QuotaKind};

struct ReadFaultStore {
    inner: MockObjectStore,
    fail_at: usize,
    error: CacheError,
    calls: RefCell<Vec<String>>,
}

impl ReadFaultStore {
    fn read(&self, label: &str) -> Result<(), CacheError> {
        self.calls.borrow_mut().push(label.to_string());
        if self.calls.borrow().len() == self.fail_at {
            return Err(self.error.clone());
        }
        Ok(())
    }
}

impl ObjectStore for ReadFaultStore {
    fn put(&mut self, _key: &str, _body: &[u8]) -> Result<(), CacheError> {
        panic!("refill must not publish")
    }
    fn head(&self, key: &str) -> Result<ObjectMeta, CacheError> {
        self.read("head")?;
        self.inner.head(key)
    }
    fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
        self.read(if key.ends_with("/manifest") {
            "manifest"
        } else {
            "object"
        })?;
        self.inner.range_get(key, offset, length)
    }
    fn delete(&mut self, _key: &str) -> Result<(), CacheError> {
        panic!("refill must not delete")
    }
}

#[test]
fn actual_read_outages_at_every_stage_are_clean_misses_without_retries_or_mutation() {
    let identity = synthetic_identity(1, 1);
    let payload = vec![7; 32];
    let pack = pack_blocks(
        "read-outage",
        &[PackedBlock {
            generation: 1,
            identity: identity.clone(),
            checksum: content_checksum(&payload),
            payload,
        }],
    )
    .unwrap();
    let budget = RefillBudget::new(1024 * 1024, 1).unwrap();
    let mut inner = MockObjectStore::default();
    inner.put(&pack.object_key, &pack.object_bytes).unwrap();
    inner.put(&pack.manifest_key, &pack.manifest_bytes).unwrap();
    let errors = [
        CacheError::TimedOut,
        CacheError::Storage {
            reason: "object missing",
        },
        CacheError::Storage {
            reason: "object unavailable",
        },
        CacheError::Storage {
            reason: "object store",
        },
        CacheError::QuotaExceeded {
            kind: QuotaKind::InFlightBytes,
        },
    ];
    for fail_at in 1..=3 {
        for error in &errors {
            for entry in 0..4 {
                let store = ReadFaultStore {
                    inner: inner.clone(),
                    fail_at,
                    error: error.clone(),
                    calls: RefCell::new(Vec::new()),
                };
                let result = match entry {
                    0 => refill_generation(&store, &pack.manifest_key, 1, &budget, &identity),
                    1 => refill_matching(&store, &pack.manifest_key, 1, &budget, &identity),
                    2 => refill_degraded(
                        &store,
                        &pack.manifest_key,
                        1,
                        &budget,
                        ColdAvailability::Ready,
                        &identity,
                    ),
                    _ => refill_for_tenant(
                        &store,
                        &pack.manifest_key,
                        1,
                        &budget,
                        ColdAvailability::Ready,
                        &identity,
                    ),
                };
                assert_eq!(result.unwrap(), RefillOutcome::Missing);
                assert_eq!(store.calls.borrow().len(), fail_at);
                assert_eq!(
                    store
                        .inner
                        .range_get(&pack.object_key, 0, pack.object_bytes.len() as u64)
                        .unwrap(),
                    pack.object_bytes
                );
                assert_eq!(
                    store
                        .inner
                        .range_get(&pack.manifest_key, 0, pack.manifest_bytes.len() as u64)
                        .unwrap(),
                    pack.manifest_bytes
                );
            }
        }
    }
}

#[test]
fn backend_validation_errors_remain_distinguishable_from_outages() {
    for error in [
        CacheError::Unauthorized,
        CacheError::ChecksumMismatch,
        CacheError::Storage {
            reason: "store corrupt",
        },
        CacheError::Malformed { reason: "cold key" },
    ] {
        let store = ReadFaultStore {
            inner: MockObjectStore::default(),
            fail_at: 1,
            error: error.clone(),
            calls: RefCell::new(Vec::new()),
        };
        assert_eq!(
            refill_generation(
                &store,
                "ns/manifest",
                1,
                &RefillBudget::new(1024, 1).unwrap(),
                &synthetic_identity(1, 1)
            ),
            Err(error)
        );
        assert_eq!(store.calls.borrow().len(), 1);
    }
}

#[test]
fn declared_slow_and_unavailable_skip_all_reads_for_both_wrappers() {
    for availability in [ColdAvailability::Slow, ColdAvailability::Unavailable] {
        let store = ReadFaultStore {
            inner: MockObjectStore::default(),
            fail_at: 1,
            error: CacheError::Unauthorized,
            calls: RefCell::new(Vec::new()),
        };
        let budget = RefillBudget::new(1024, 1).unwrap();
        let identity = synthetic_identity(1, 1);
        assert_eq!(
            refill_degraded(&store, "ns/manifest", 1, &budget, availability, &identity).unwrap(),
            RefillOutcome::Missing
        );
        assert_eq!(
            refill_for_tenant(&store, "ns/manifest", 1, &budget, availability, &identity).unwrap(),
            RefillOutcome::Missing
        );
        assert!(store.calls.borrow().is_empty());
    }
}
