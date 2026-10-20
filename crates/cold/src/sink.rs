// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Bounded, caller-driven cold writes, separate from the hot TCP path.

use std::collections::VecDeque;

use gpucache_core_types::{CacheError, QuotaKind};

use crate::pack::{validate_pack_job, ObjectStore, Pack};

/// Hard metadata bound shared by pending and failed jobs.
pub const MAX_SINK_JOBS: usize = 4096;
/// Total write attempts per job, including its first attempt.
pub const MAX_WRITE_ATTEMPTS: u8 = 3;

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
}

struct PendingJob {
    job: ColdJob,
    attempts: u8,
}

/// A terminal failure that can be removed independently of healthy work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedColdJob {
    pub job: ColdJob,
    pub error: CacheError,
    pub attempts: u8,
}

/// Per-call progress, including partial success. No unbounded error log is kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub attempted: u32,
    pub completed: u32,
    pub requeued: u32,
    pub failed: u32,
    pub first_error: Option<CacheError>,
}

pub struct AsyncSink {
    queue: VecDeque<PendingJob>,
    failures: VecDeque<FailedColdJob>,
    max_jobs: usize,
    max_bytes: u64,
    queued_bytes: u64,
    failed_bytes: u64,
    cancelled: bool,
}

impl AsyncSink {
    pub fn new(max_jobs: usize, max_bytes: u64) -> Result<Self, CacheError> {
        if max_jobs == 0 || max_jobs > MAX_SINK_JOBS || max_bytes == 0 {
            return Err(CacheError::Malformed {
                reason: "cold sink",
            });
        }
        Ok(Self {
            queue: VecDeque::new(),
            failures: VecDeque::new(),
            max_jobs,
            max_bytes,
            queued_bytes: 0,
            failed_bytes: 0,
            cancelled: false,
        })
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }
    pub fn failed_count(&self) -> usize {
        self.failures.len()
    }
    pub fn queued_bytes(&self) -> u64 {
        self.queued_bytes
    }
    pub fn failed_bytes(&self) -> u64 {
        self.failed_bytes
    }
    pub fn retained_bytes(&self) -> u64 {
        self.queued_bytes + self.failed_bytes
    }

    pub fn enqueue(&mut self, job: ColdJob) -> Result<(), CacheError> {
        if self.cancelled {
            return Err(CacheError::Cancelled);
        }
        let add = job.byte_weight();
        if add > self.max_bytes {
            return Err(CacheError::ObjectTooLarge {
                limit: self.max_bytes,
                actual: add,
            });
        }
        let next = self
            .retained_bytes()
            .checked_add(add)
            .ok_or(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            })?;
        if self.queue.len() + self.failures.len() >= self.max_jobs || next > self.max_bytes {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            });
        }
        // Public fields cannot bypass immutable publication keys or checksums.
        validate_pack_job(
            &job.object_key,
            &job.object_bytes,
            &job.manifest_key,
            &job.manifest_bytes,
        )?;
        self.queued_bytes += add;
        self.queue.push_back(PendingJob { job, attempts: 0 });
        Ok(())
    }

    /// Attempt at most `max_jobs` jobs present at call entry, each at most once.
    /// Transient failures go to the tail; permanent or exhausted failures are
    /// isolated. Both queues share the original job and byte admission limits.
    ///
    /// A failed PUT may have committed before its acknowledgement was lost.
    /// Never delete its object here: its manifest may already reference it.
    /// Content-derived keys make retries idempotent and prevent older jobs
    /// from overwriting another pack in the same namespace.
    pub fn drain_report(
        &mut self,
        store: &mut dyn ObjectStore,
        max_jobs: usize,
    ) -> Result<DrainReport, CacheError> {
        if self.cancelled {
            return Err(CacheError::Cancelled);
        }
        let limit = max_jobs.min(self.queue.len());
        let mut report = DrainReport::default();
        for _ in 0..limit {
            let mut pending = self.queue.pop_front().expect("bounded queue snapshot");
            pending.attempts += 1;
            report.attempted += 1;
            let job = &pending.job;
            let result = store
                .put(&job.object_key, &job.object_bytes)
                .and_then(|()| store.put(&job.manifest_key, &job.manifest_bytes));
            match result {
                Ok(()) => {
                    self.queued_bytes -= pending.job.byte_weight();
                    report.completed += 1;
                }
                Err(error) => {
                    if report.first_error.is_none() {
                        report.first_error = Some(error.clone());
                    }
                    if crate::failure::transient(&error) && pending.attempts < MAX_WRITE_ATTEMPTS {
                        self.queue.push_back(pending);
                        report.requeued += 1;
                    } else {
                        let weight = pending.job.byte_weight();
                        self.queued_bytes -= weight;
                        self.failed_bytes += weight;
                        self.failures.push_back(FailedColdJob {
                            job: pending.job,
                            error,
                            attempts: pending.attempts,
                        });
                        report.failed += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Compatibility wrapper. The whole bounded round runs before returning
    /// its first error. Use `drain_report` to observe partial success directly.
    pub fn drain(
        &mut self,
        store: &mut dyn ObjectStore,
        max_jobs: usize,
    ) -> Result<u32, CacheError> {
        let report = self.drain_report(store, max_jobs)?;
        match report.first_error {
            Some(error) => Err(error),
            None => Ok(report.completed),
        }
    }

    /// Transfer ownership of one failed job and release exactly its credits.
    /// Taking a job does not retry it or delete any possibly published object.
    pub fn take_failed(&mut self) -> Option<FailedColdJob> {
        let failed = self.failures.pop_front()?;
        self.failed_bytes -= failed.job.byte_weight();
        Some(failed)
    }

    /// Drop all pending and failed work. Later enqueue and drain are cancelled.
    pub fn cancel(&mut self) {
        self.queue.clear();
        self.failures.clear();
        self.queued_bytes = 0;
        self.failed_bytes = 0;
        self.cancelled = true;
    }
}

#[cfg(test)]
#[path = "sink_tests.rs"]
mod tests;
