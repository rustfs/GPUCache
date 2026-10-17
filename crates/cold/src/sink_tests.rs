// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{pack_blocks, MockObjectStore, ObjectMeta, PackedBlock};
use gpucache_core_types::{content_checksum, synthetic_identity};
use std::collections::BTreeMap;

#[derive(Clone)]
enum Fault {
    Before(CacheError),
    After(CacheError),
}

#[derive(Default)]
struct FaultStore {
    inner: MockObjectStore,
    faults: BTreeMap<String, VecDeque<Fault>>,
    puts: BTreeMap<String, usize>,
    deletes: usize,
}

impl FaultStore {
    fn fail(&mut self, key: &str, fault: Fault, count: usize) {
        self.faults
            .entry(key.to_string())
            .or_default()
            .extend(vec![fault; count]);
    }

    fn puts(&self, key: &str) -> usize {
        self.puts.get(key).copied().unwrap_or(0)
    }
}

impl ObjectStore for FaultStore {
    fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError> {
        *self.puts.entry(key.to_string()).or_default() += 1;
        let fault = self.faults.get_mut(key).and_then(VecDeque::pop_front);
        if let Some(Fault::Before(error)) = &fault {
            return Err(error.clone());
        }
        self.inner.put(key, body)?;
        if let Some(Fault::After(error)) = fault {
            return Err(error);
        }
        Ok(())
    }

    fn head(&self, key: &str) -> Result<ObjectMeta, CacheError> {
        self.inner.head(key)
    }
    fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
        self.inner.range_get(key, offset, length)
    }
    fn delete(&mut self, key: &str) -> Result<(), CacheError> {
        self.deletes += 1;
        self.inner.delete(key)
    }
}

fn pack(namespace: &str, byte: u8) -> Pack {
    let payload = vec![byte; 32];
    let mut identity = synthetic_identity(byte, 1);
    identity.prefix_chain[0].0[0] = byte;
    pack_blocks(
        namespace,
        &[PackedBlock {
            generation: u64::from(byte),
            identity,
            checksum: content_checksum(&payload),
            payload,
        }],
    )
    .unwrap()
}

#[test]
fn permanent_failure_is_isolated_without_blocking_healthy_work() {
    let bad = ColdJob::from_pack(&pack("denied", 1));
    let good = ColdJob::from_pack(&pack("healthy", 2));
    let bad_weight = bad.byte_weight();
    let mut sink = AsyncSink::new(2, bad_weight + good.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(&bad.object_key, Fault::Before(CacheError::Unauthorized), 8);
    sink.enqueue(bad.clone()).unwrap();
    sink.enqueue(good.clone()).unwrap();
    let report = sink.drain_report(&mut store, usize::MAX).unwrap();
    assert_eq!(
        report,
        DrainReport {
            attempted: 2,
            completed: 1,
            failed: 1,
            first_error: Some(CacheError::Unauthorized),
            ..DrainReport::default()
        }
    );
    assert_eq!((sink.queued(), sink.failed_count()), (0, 1));
    assert!(store.head(&good.manifest_key).is_ok());
    assert_eq!((sink.queued_bytes(), sink.failed_bytes()), (0, bad_weight));
    assert_eq!(
        sink.take_failed().unwrap(),
        FailedColdJob {
            job: bad,
            error: CacheError::Unauthorized,
            attempts: 1,
        }
    );
    assert_eq!(sink.retained_bytes(), 0);
    assert!(sink.take_failed().is_none());
    sink.enqueue(good).unwrap();
}

#[test]
fn transient_failure_rotates_and_each_round_attempts_a_job_once() {
    let bad = ColdJob::from_pack(&pack("retry", 1));
    let good = ColdJob::from_pack(&pack("healthy", 2));
    let mut sink = AsyncSink::new(2, bad.byte_weight() + good.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(&bad.object_key, Fault::Before(CacheError::TimedOut), 1);
    sink.enqueue(bad.clone()).unwrap();
    sink.enqueue(good.clone()).unwrap();
    let first = sink.drain_report(&mut store, 1).unwrap();
    assert_eq!((first.attempted, first.requeued), (1, 1));
    assert_eq!(
        sink.retained_bytes(),
        bad.byte_weight() + good.byte_weight()
    );
    assert_eq!(sink.drain_report(&mut store, 1).unwrap().completed, 1);
    assert!(store.head(&good.manifest_key).is_ok());
    assert_eq!(store.puts(&bad.object_key), 1);
    assert_eq!(sink.queued_bytes(), bad.byte_weight());
    assert_eq!(
        sink.drain_report(&mut store, usize::MAX).unwrap().completed,
        1
    );
    assert_eq!(store.puts(&bad.object_key), 2);
    assert_eq!(sink.retained_bytes(), 0);
}

#[test]
fn retry_exhaustion_is_bounded_and_failed_jobs_keep_both_credits() {
    let job = ColdJob::from_pack(&pack("unavailable", 1));
    let mut sink = AsyncSink::new(1, job.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(
        &job.object_key,
        Fault::Before(CacheError::Storage {
            reason: "object store",
        }),
        9,
    );
    sink.enqueue(job.clone()).unwrap();
    for attempt in 1..=MAX_WRITE_ATTEMPTS {
        assert_eq!(
            sink.drain_report(&mut store, usize::MAX).unwrap().attempted,
            1
        );
        assert_eq!(store.puts(&job.object_key), usize::from(attempt));
        assert_eq!(sink.retained_bytes(), job.byte_weight());
    }
    assert_eq!((sink.queued(), sink.failed_count()), (0, 1));
    assert_eq!(
        sink.drain_report(&mut store, usize::MAX).unwrap(),
        DrainReport::default()
    );
    assert!(matches!(
        sink.enqueue(job.clone()),
        Err(CacheError::QuotaExceeded { .. })
    ));
    assert_eq!(sink.take_failed().unwrap().attempts, MAX_WRITE_ATTEMPTS);
    assert_eq!(sink.retained_bytes(), 0);
    sink.enqueue(job).unwrap();
}

#[test]
fn failed_jobs_share_the_byte_budget_even_with_free_job_slots() {
    let job = ColdJob::from_pack(&pack("budget", 1));
    let weight = job.byte_weight();
    let mut sink = AsyncSink::new(3, weight).unwrap();
    let mut store = FaultStore::default();
    store.fail(&job.object_key, Fault::Before(CacheError::Unauthorized), 1);
    sink.enqueue(job.clone()).unwrap();
    sink.drain_report(&mut store, 1).unwrap();
    assert_eq!(sink.failed_bytes(), weight);
    assert!(matches!(
        sink.enqueue(job.clone()),
        Err(CacheError::QuotaExceeded { .. })
    ));
    sink.take_failed().unwrap();
    sink.enqueue(job).unwrap();
}

#[test]
fn failed_jobs_share_the_job_limit_even_with_free_byte_credit() {
    let job = ColdJob::from_pack(&pack("job-budget", 1));
    let mut sink = AsyncSink::new(1, job.byte_weight() * 2).unwrap();
    let mut store = FaultStore::default();
    store.fail(&job.object_key, Fault::Before(CacheError::Unauthorized), 1);
    sink.enqueue(job.clone()).unwrap();
    sink.drain_report(&mut store, 1).unwrap();
    assert!(sink.retained_bytes() < sink.max_bytes);
    assert!(matches!(
        sink.enqueue(job.clone()),
        Err(CacheError::QuotaExceeded { .. })
    ));
    sink.take_failed().unwrap();
    sink.enqueue(job).unwrap();
}

#[test]
fn compatibility_drain_reports_error_after_other_jobs_make_progress() {
    let bad = ColdJob::from_pack(&pack("denied", 1));
    let good = ColdJob::from_pack(&pack("healthy", 2));
    let mut sink = AsyncSink::new(2, bad.byte_weight() + good.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(&bad.object_key, Fault::Before(CacheError::Unauthorized), 1);
    sink.enqueue(bad).unwrap();
    sink.enqueue(good.clone()).unwrap();
    assert_eq!(sink.drain(&mut store, 2), Err(CacheError::Unauthorized));
    assert!(store.head(&good.manifest_key).is_ok());
    assert_eq!(sink.failed_count(), 1);
}

#[test]
fn ambiguous_manifest_put_preserves_a_published_object_and_retries_idempotently() {
    let pack = pack("ack-lost", 1);
    let job = ColdJob::from_pack(&pack);
    let mut sink = AsyncSink::new(1, job.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(&pack.manifest_key, Fault::After(CacheError::TimedOut), 1);
    sink.enqueue(job).unwrap();
    assert_eq!(sink.drain(&mut store, 1), Err(CacheError::TimedOut));
    assert!(store.head(&pack.manifest_key).is_ok());
    assert_eq!(
        store
            .range_get(&pack.object_key, 0, pack.object_bytes.len() as u64)
            .unwrap(),
        pack.object_bytes
    );
    assert_eq!(store.deletes, 0);
    assert_eq!(sink.drain(&mut store, 1), Ok(1));
    assert_eq!((sink.retained_bytes(), store.deletes), (0, 0));
}

#[test]
fn failed_manifest_before_commit_retains_the_unreferenced_object() {
    let pack = pack("unpublished", 1);
    let job = ColdJob::from_pack(&pack);
    let mut sink = AsyncSink::new(1, job.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(&pack.manifest_key, Fault::Before(CacheError::TimedOut), 1);
    sink.enqueue(job).unwrap();
    assert_eq!(sink.drain(&mut store, 1), Err(CacheError::TimedOut));
    assert!(store.head(&pack.object_key).is_ok());
    assert!(store.head(&pack.manifest_key).is_err());
    assert_eq!(store.deletes, 0);
    assert_eq!(sink.drain(&mut store, 1), Ok(1));
}

#[test]
fn object_put_ack_loss_does_not_publish_or_delete_prematurely() {
    let pack = pack("object-ack-lost", 1);
    let job = ColdJob::from_pack(&pack);
    let mut sink = AsyncSink::new(1, job.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(&pack.object_key, Fault::After(CacheError::TimedOut), 1);
    sink.enqueue(job).unwrap();
    assert_eq!(sink.drain(&mut store, 1), Err(CacheError::TimedOut));
    assert!(store.head(&pack.object_key).is_ok());
    assert!(store.head(&pack.manifest_key).is_err());
    assert_eq!(sink.drain(&mut store, 1), Ok(1));
    assert_eq!(store.deletes, 0);
}

#[test]
fn interleaved_retries_in_one_namespace_cannot_overwrite_another_pack() {
    let old = pack("shared", 1);
    let new = pack("shared", 2);
    assert_ne!(old.object_key, new.object_key);
    assert_ne!(old.manifest_key, new.manifest_key);
    let old_job = ColdJob::from_pack(&old);
    let new_job = ColdJob::from_pack(&new);
    let mut sink = AsyncSink::new(2, old_job.byte_weight() + new_job.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(&old.manifest_key, Fault::Before(CacheError::TimedOut), 1);
    sink.enqueue(old_job).unwrap();
    sink.enqueue(new_job).unwrap();
    let first = sink.drain_report(&mut store, usize::MAX).unwrap();
    assert_eq!(
        (first.attempted, first.completed, first.requeued),
        (2, 1, 1)
    );
    assert_eq!(sink.drain(&mut store, 1), Ok(1));
    for pack in [old, new] {
        assert_eq!(
            store
                .range_get(&pack.manifest_key, 0, pack.manifest_bytes.len() as u64)
                .unwrap(),
            pack.manifest_bytes
        );
        assert_eq!(
            store
                .range_get(&pack.object_key, 0, pack.object_bytes.len() as u64)
                .unwrap(),
            pack.object_bytes
        );
    }
}

#[test]
fn malformed_public_jobs_do_not_consume_admission_credits() {
    let job = ColdJob::from_pack(&pack("validated", 1));
    let mut sink = AsyncSink::new(1, job.byte_weight() * 2).unwrap();
    for mutated in 0..4 {
        let mut bad = job.clone();
        match mutated {
            0 => bad.object_key = "mutable/object".to_string(),
            1 => bad.manifest_key = "mutable/manifest".to_string(),
            2 => bad.object_bytes[0] ^= 1,
            _ => bad.manifest_bytes[0] ^= 1,
        }
        assert!(sink.enqueue(bad).is_err());
        assert_eq!((sink.queued(), sink.retained_bytes()), (0, 0));
    }
    sink.enqueue(job).unwrap();
}

#[test]
fn cancellation_releases_pending_and_failed_credits_and_stays_closed() {
    let bad = ColdJob::from_pack(&pack("denied", 1));
    let good = ColdJob::from_pack(&pack("healthy", 2));
    let mut sink = AsyncSink::new(2, bad.byte_weight() + good.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    store.fail(&bad.object_key, Fault::Before(CacheError::Unauthorized), 1);
    sink.enqueue(bad).unwrap();
    sink.enqueue(good.clone()).unwrap();
    sink.drain_report(&mut store, 1).unwrap();
    assert_eq!((sink.queued(), sink.failed_count()), (1, 1));
    sink.cancel();
    assert_eq!(
        (sink.queued(), sink.failed_count(), sink.retained_bytes()),
        (0, 0, 0)
    );
    assert!(sink.take_failed().is_none());
    assert_eq!(sink.drain(&mut store, 4), Err(CacheError::Cancelled));
    assert_eq!(sink.drain_report(&mut store, 4), Err(CacheError::Cancelled));
    assert_eq!(sink.enqueue(good.clone()), Err(CacheError::Cancelled));
    assert_eq!(store.puts(&good.object_key), 0);
}

#[test]
fn zero_work_and_empty_queue_do_not_touch_the_store() {
    let job = ColdJob::from_pack(&pack("idle", 1));
    let mut sink = AsyncSink::new(1, job.byte_weight()).unwrap();
    let mut store = FaultStore::default();
    assert_eq!(
        sink.drain_report(&mut store, usize::MAX).unwrap(),
        DrainReport::default()
    );
    sink.enqueue(job.clone()).unwrap();
    assert_eq!(
        sink.drain_report(&mut store, 0).unwrap(),
        DrainReport::default()
    );
    assert_eq!(sink.retained_bytes(), job.byte_weight());
    assert!(store.puts.is_empty());
}

#[test]
fn metadata_and_payload_admissions_are_bounded() {
    assert!(AsyncSink::new(0, 1).is_err());
    assert!(AsyncSink::new(MAX_SINK_JOBS + 1, 1).is_err());
    assert!(AsyncSink::new(1, 0).is_err());
    let job = ColdJob::from_pack(&pack("bounds", 1));
    let mut sink = AsyncSink::new(1, job.byte_weight() - 1).unwrap();
    assert!(matches!(
        sink.enqueue(job),
        Err(CacheError::ObjectTooLarge { .. })
    ));
    assert_eq!(sink.retained_bytes(), 0);
}
