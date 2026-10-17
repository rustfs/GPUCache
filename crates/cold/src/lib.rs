// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Optional G4 cold tier in front of RustFS.
//!
//! The hot TCP path does not call this crate. A miss still means the engine
//! recomputes. Without an authorized RustFS endpoint the live probe is
//! `NOT_RUN_HARDWARE`; the in-process mock still checks Put, Head, RangeGet,
//! and Delete plus object packing.

#![forbid(unsafe_code)]

mod demo;
mod http;
mod mock;
mod ops;
mod pack;
mod refill;
mod sigv4;
mod sink;

pub use demo::{run_cold_demo, ColdReport};
pub use http::{live_probe, ProbeReport, ProbeStatus};
pub use mock::MockObjectStore;
pub use ops::{
    enqueue_backfill, refill_degraded, refill_for_tenant, verify_deleted, BackfillLimit,
    ColdAvailability, ColdKeys, ColdOps, ObjectRecord, ReclaimStep, Retention,
};
pub use pack::{
    contract_smoke, manifest_version, pack_blocks, read_manifest, Manifest, ObjectMeta,
    ObjectStore, Pack, PackedBlock, PayloadRange, MANIFEST_VERSION,
};
pub use refill::{refill_generation, refill_matching, RefillBudget, RefillOutcome};
pub use sink::{AsyncSink, ColdJob, DrainReport, FailedColdJob, MAX_SINK_JOBS, MAX_WRITE_ATTEMPTS};
