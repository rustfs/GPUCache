// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Decoupled cold writes and a budget-capped refill.
//!
//! `AsyncSink` is a bounded queue drained by the caller. It is not a Tokio
//! runtime and it does not run on the hot TCP path. `refill_generation`
//! returns payload bytes only after a checksum match that fits the budget.
//! Over budget is an empty outcome: the engine recomputes.

use std::collections::VecDeque;

use gpucache_core_types::{content_checksum, CacheError, QuotaKind};

use crate::pack::{key_ok, read_manifest, Manifest, ObjectStore, Pack};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdJob {
    pub object_key: String,
    pub object_bytes: Vec<u8>,
    pub manifest_key: String,
    pub manifest_bytes: Vec<u8>,
}

impl ColdJob {
    pub fn from_pack(pack: &Pack) -> Self {
        Self {
            object_key: pack.object_key.clone(),
            object_bytes: pack.object_bytes.clone(),
            manifest_key: pack.manifest_key.clone(),
            manifest_bytes: pack.manifest_bytes.clone(),
        }
    }

    pub fn byte_weight(&self) -> u64 {
        self.object_bytes.len() as u64 + self.manifest_bytes.len() as u64
    }

    fn weight(&self) -> u64 {
        self.byte_weight()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefillBudget {
    /// Total downloaded manifest plus payload bytes per refill.
    pub max_bytes: u64,
    pub max_ranges: u32,
}

impl RefillBudget {
    pub fn new(max_bytes: u64, max_ranges: u32) -> Result<Self, CacheError> {
        if max_bytes == 0 || max_ranges == 0 {
            return Err(CacheError::Malformed {
                reason: "refill budget",
            });
        }
        Ok(Self {
            max_bytes,
            max_ranges,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefillOutcome {
    Loaded {
        payload: Vec<u8>,
    },
    /// Manifest or payload exceeds the budget. No payload bytes.
    OverBudget,
    /// Clean miss. The caller recomputes.
    Missing,
}

pub struct AsyncSink {
    queue: VecDeque<ColdJob>,
    max_jobs: usize,
    max_bytes: u64,
    queued_bytes: u64,
    cancelled: bool,
}

impl AsyncSink {
    pub fn new(max_jobs: usize, max_bytes: u64) -> Result<Self, CacheError> {
        if max_jobs == 0 || max_bytes == 0 {
            return Err(CacheError::Malformed {
                reason: "cold sink",
            });
        }
        Ok(Self {
            queue: VecDeque::new(),
            max_jobs,
            max_bytes,
            queued_bytes: 0,
            cancelled: false,
        })
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn enqueue(&mut self, job: ColdJob) -> Result<(), CacheError> {
        if self.cancelled {
            return Err(CacheError::Cancelled);
        }
        if !key_ok(&job.object_key) || !key_ok(&job.manifest_key) {
            return Err(CacheError::Malformed { reason: "cold key" });
        }
        let add = job.weight();
        if add > self.max_bytes {
            return Err(CacheError::ObjectTooLarge {
                limit: self.max_bytes,
                actual: add,
            });
        }
        let next = self.queued_bytes.saturating_add(add);
        if self.queue.len() >= self.max_jobs || next > self.max_bytes {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            });
        }
        self.queued_bytes = next;
        self.queue.push_back(job);
        Ok(())
    }

    /// Put at most `max_jobs` queued packs. The object is written first, then
    /// the manifest, so a manifest is not visible without its object. A
    /// failed object or manifest write puts that job back at the front of the
    /// queue. A failed manifest write also deletes the object just written.
    pub fn drain(
        &mut self,
        store: &mut dyn ObjectStore,
        max_jobs: usize,
    ) -> Result<u32, CacheError> {
        if self.cancelled {
            return Err(CacheError::Cancelled);
        }
        let mut done = 0u32;
        while (done as usize) < max_jobs {
            let Some(job) = self.queue.pop_front() else {
                break;
            };
            let weight = job.weight();
            self.queued_bytes = self.queued_bytes.saturating_sub(weight);
            if let Err(err) = store.put(&job.object_key, &job.object_bytes) {
                self.requeue_front(job);
                return Err(err);
            }
            if let Err(err) = store.put(&job.manifest_key, &job.manifest_bytes) {
                let _ = store.delete(&job.object_key);
                self.requeue_front(job);
                return Err(err);
            }
            done = done.saturating_add(1);
        }
        Ok(done)
    }

    fn requeue_front(&mut self, job: ColdJob) {
        self.queued_bytes = self.queued_bytes.saturating_add(job.weight());
        self.queue.push_front(job);
    }

    /// Drop queued work. Later enqueue and drain fail with `Cancelled`.
    pub fn cancel(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        self.cancelled = true;
    }
}

/// Fetch one generation from a packed object if it fits `budget`.
///
/// The manifest is not downloaded when its length exceeds the budget. The
/// payload range is not downloaded when its length exceeds the budget.
/// This path checks the payload checksum. It does not check the tenant.
/// Call [`refill_matching`] when a wrong tenant must not return bytes.
pub fn refill_generation(
    store: &dyn ObjectStore,
    manifest_key: &str,
    generation: u64,
    budget: &RefillBudget,
) -> Result<RefillOutcome, CacheError> {
    refill_inner(store, manifest_key, generation, budget, None)
}

/// Same budget rules as [`refill_generation`], plus the tenant recorded on
/// the manifest range. A mismatch is [`CacheError::Unauthorized`] and does
/// not download the payload.
pub fn refill_matching(
    store: &dyn ObjectStore,
    manifest_key: &str,
    generation: u64,
    budget: &RefillBudget,
    tenant: &[u8],
) -> Result<RefillOutcome, CacheError> {
    if tenant.is_empty() || tenant.len() > 128 {
        return Err(CacheError::Malformed { reason: "cold key" });
    }
    refill_inner(store, manifest_key, generation, budget, Some(tenant))
}

fn refill_inner(
    store: &dyn ObjectStore,
    manifest_key: &str,
    generation: u64,
    budget: &RefillBudget,
    tenant: Option<&[u8]>,
) -> Result<RefillOutcome, CacheError> {
    if !key_ok(manifest_key) {
        return Err(CacheError::Malformed { reason: "cold key" });
    }
    let meta = match store.head(manifest_key) {
        Ok(meta) => meta,
        Err(CacheError::Storage {
            reason: "object missing",
        }) => return Ok(RefillOutcome::Missing),
        Err(err) => return Err(err),
    };
    if meta.len > budget.max_bytes {
        return Ok(RefillOutcome::OverBudget);
    }
    let manifest_bytes = store.range_get(manifest_key, 0, meta.len)?;
    if manifest_bytes.len() as u64 != meta.len || content_checksum(&manifest_bytes) != meta.checksum
    {
        return Err(CacheError::ChecksumMismatch);
    }
    let manifest = read_manifest(&manifest_bytes)?;
    if manifest.ranges.len() as u32 > budget.max_ranges {
        return Ok(RefillOutcome::OverBudget);
    }
    let Some(range) = manifest
        .ranges
        .iter()
        .find(|range| range.generation == generation)
    else {
        return Ok(RefillOutcome::Missing);
    };
    if let Some(expected) = tenant {
        if range.tenant.as_slice() != expected {
            return Err(CacheError::Unauthorized);
        }
    }
    if range.length == 0 || range.length > budget.max_bytes.saturating_sub(meta.len) {
        return Ok(RefillOutcome::OverBudget);
    }
    load_range(store, &manifest, range.offset, range.length, range.checksum)
}

fn load_range(
    store: &dyn ObjectStore,
    manifest: &Manifest,
    offset: u64,
    length: u64,
    checksum: gpucache_core_types::Checksum,
) -> Result<RefillOutcome, CacheError> {
    let payload = match store.range_get(&manifest.object_key, offset, length) {
        Ok(payload) => payload,
        Err(CacheError::Storage {
            reason: "object missing",
        }) => return Ok(RefillOutcome::Missing),
        Err(err) => return Err(err),
    };
    if payload.len() as u64 != length || content_checksum(&payload) != checksum {
        return Err(CacheError::ChecksumMismatch);
    }
    Ok(RefillOutcome::Loaded { payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{pack_blocks, PackedBlock};
    use crate::MockObjectStore;
    use gpucache_core_types::{content_checksum, synthetic_identity};
    use std::cell::Cell;

    fn block(generation: u64, byte: u8, len: usize) -> PackedBlock {
        let payload = vec![byte; len];
        let mut identity = synthetic_identity(byte, 1);
        identity.prefix_chain[0].0[0] = byte;
        PackedBlock {
            generation,
            identity,
            checksum: content_checksum(&payload),
            payload,
        }
    }

    struct CountStore {
        inner: MockObjectStore,
        manifest_ranges: Cell<u32>,
        object_ranges: Cell<u32>,
    }

    impl ObjectStore for CountStore {
        fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError> {
            self.inner.put(key, body)
        }

        fn head(&self, key: &str) -> Result<crate::ObjectMeta, CacheError> {
            self.inner.head(key)
        }

        fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
            if key.ends_with("/manifest") {
                self.manifest_ranges.set(self.manifest_ranges.get() + 1);
            }
            if key.ends_with("/object") {
                self.object_ranges.set(self.object_ranges.get() + 1);
            }
            self.inner.range_get(key, offset, length)
        }

        fn delete(&mut self, key: &str) -> Result<(), CacheError> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn drain_then_refill_checks_the_budget_before_the_payload() {
        let blocks = vec![block(1, 1, 32), block(2, 2, 4096)];
        let pack = pack_blocks("refill", &blocks).unwrap();
        let mut store = CountStore {
            inner: MockObjectStore::default(),
            manifest_ranges: Cell::new(0),
            object_ranges: Cell::new(0),
        };
        let mut sink = AsyncSink::new(2, 8 * 1024 * 1024).unwrap();
        sink.enqueue(ColdJob::from_pack(&pack)).unwrap();
        assert_eq!(sink.drain(&mut store, 1).unwrap(), 1);
        let manifest_len = pack.manifest_bytes.len() as u64;
        assert!(manifest_len < 4096);
        let tight = RefillBudget::new(manifest_len, 16).unwrap();
        let skipped = refill_generation(&store, &pack.manifest_key, 2, &tight).unwrap();
        assert_eq!(skipped, RefillOutcome::OverBudget);
        assert_eq!(store.manifest_ranges.get(), 1);
        assert_eq!(store.object_ranges.get(), 0);
        let wide = RefillBudget::new(8 * 1024 * 1024, 16).unwrap();
        let loaded = refill_generation(&store, &pack.manifest_key, 2, &wide).unwrap();
        assert_eq!(
            loaded,
            RefillOutcome::Loaded {
                payload: blocks[1].payload.clone(),
            }
        );
        assert_eq!(store.object_ranges.get(), 1);
    }

    #[test]
    fn cancel_drops_the_queue_and_writes_nothing() {
        let blocks = vec![block(1, 3, 8), block(2, 4, 8)];
        let pack = pack_blocks("cancel", &blocks).unwrap();
        let mut sink = AsyncSink::new(1, 1024 * 1024).unwrap();
        sink.enqueue(ColdJob::from_pack(&pack)).unwrap();
        let again = sink.enqueue(ColdJob::from_pack(&pack)).unwrap_err();
        assert!(matches!(
            again,
            CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes
            }
        ));
        sink.cancel();
        assert_eq!(sink.queued(), 0);
        let mut store = MockObjectStore::default();
        assert_eq!(
            sink.drain(&mut store, 4).unwrap_err(),
            CacheError::Cancelled
        );
        assert!(matches!(
            store.head(&pack.object_key),
            Err(CacheError::Storage {
                reason: "object missing"
            })
        ));
    }

    #[test]
    fn huge_manifest_is_not_downloaded() {
        let blocks = vec![block(1, 5, 8), block(2, 6, 8)];
        let pack = pack_blocks("tiny", &blocks).unwrap();
        let mut store = CountStore {
            inner: MockObjectStore::default(),
            manifest_ranges: Cell::new(0),
            object_ranges: Cell::new(0),
        };
        store.put(&pack.manifest_key, &pack.manifest_bytes).unwrap();
        let budget = RefillBudget::new(1, 16).unwrap();
        let outcome = refill_generation(&store, &pack.manifest_key, 1, &budget).unwrap();
        assert_eq!(outcome, RefillOutcome::OverBudget);
        assert_eq!(store.manifest_ranges.get(), 0);
        assert_eq!(store.object_ranges.get(), 0);
    }
    #[test]
    fn refill_charges_manifest_and_payload_to_one_budget() {
        let pack = pack_blocks("total-budget", &[block(1, 1, 32)]).unwrap();
        let mut store = CountStore {
            inner: MockObjectStore::default(),
            manifest_ranges: Cell::new(0),
            object_ranges: Cell::new(0),
        };
        store.put(&pack.manifest_key, &pack.manifest_bytes).unwrap();
        store.put(&pack.object_key, &pack.object_bytes).unwrap();
        let total = pack.manifest_bytes.len() as u64 + 32;
        assert_eq!(
            refill_generation(
                &store,
                &pack.manifest_key,
                1,
                &RefillBudget::new(total - 1, 1).unwrap()
            )
            .unwrap(),
            RefillOutcome::OverBudget
        );
        assert_eq!(store.object_ranges.get(), 0);
        assert!(matches!(
            refill_generation(
                &store,
                &pack.manifest_key,
                1,
                &RefillBudget::new(total, 1).unwrap()
            )
            .unwrap(),
            RefillOutcome::Loaded { .. }
        ));
        assert_eq!(store.object_ranges.get(), 1);
    }

    struct FailStore {
        inner: MockObjectStore,
        fail_object: bool,
        fail_manifest: bool,
    }

    impl ObjectStore for FailStore {
        fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError> {
            if key.ends_with("/object") && self.fail_object {
                return Err(CacheError::Storage {
                    reason: "object store",
                });
            }
            if key.ends_with("/manifest") && self.fail_manifest {
                return Err(CacheError::Storage {
                    reason: "object store",
                });
            }
            self.inner.put(key, body)
        }

        fn head(&self, key: &str) -> Result<crate::ObjectMeta, CacheError> {
            self.inner.head(key)
        }

        fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
            self.inner.range_get(key, offset, length)
        }

        fn delete(&mut self, key: &str) -> Result<(), CacheError> {
            self.inner.delete(key)
        }
    }

    #[test]
    fn failed_drain_keeps_the_job_and_hides_a_partial_object() {
        let blocks = vec![block(1, 7, 8), block(2, 8, 8)];
        let pack = pack_blocks("retry", &blocks).unwrap();
        let mut sink = AsyncSink::new(2, 8 * 1024 * 1024).unwrap();
        sink.enqueue(ColdJob::from_pack(&pack)).unwrap();
        let queued_bytes = sink.queued_bytes;
        let mut store = FailStore {
            inner: MockObjectStore::default(),
            fail_object: true,
            fail_manifest: false,
        };
        assert_eq!(
            sink.drain(&mut store, 1).unwrap_err(),
            CacheError::Storage {
                reason: "object store"
            }
        );
        assert_eq!(sink.queued(), 1);
        assert_eq!(sink.queued_bytes, queued_bytes);
        assert!(store.head(&pack.object_key).is_err());
        store.fail_object = false;
        store.fail_manifest = true;
        assert_eq!(
            sink.drain(&mut store, 1).unwrap_err(),
            CacheError::Storage {
                reason: "object store"
            }
        );
        assert_eq!(sink.queued(), 1);
        assert!(store.head(&pack.object_key).is_err());
        assert!(store.head(&pack.manifest_key).is_err());
        store.fail_manifest = false;
        assert_eq!(sink.drain(&mut store, 1).unwrap(), 1);
        assert_eq!(sink.queued(), 0);
        assert!(store.head(&pack.manifest_key).is_ok());
    }

    #[test]
    fn wrong_tenant_refill_does_not_download_the_payload() {
        let pack = pack_blocks("tenant", &[block(1, 1, 32)]).unwrap();
        let mut store = CountStore {
            inner: MockObjectStore::default(),
            manifest_ranges: Cell::new(0),
            object_ranges: Cell::new(0),
        };
        store.put(&pack.manifest_key, &pack.manifest_bytes).unwrap();
        store.put(&pack.object_key, &pack.object_bytes).unwrap();
        let budget = RefillBudget::new(8 * 1024 * 1024, 4).unwrap();
        assert_eq!(
            refill_matching(&store, &pack.manifest_key, 1, &budget, b"other-tenant").unwrap_err(),
            CacheError::Unauthorized
        );
        assert_eq!(store.manifest_ranges.get(), 1);
        assert_eq!(store.object_ranges.get(), 0);
        let loaded = refill_matching(
            &store,
            &pack.manifest_key,
            1,
            &budget,
            pack.ranges[0].tenant.as_slice(),
        )
        .unwrap();
        assert_eq!(
            loaded,
            RefillOutcome::Loaded {
                payload: vec![1u8; 32],
            }
        );
        assert_eq!(store.object_ranges.get(), 1);
    }
}
