// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Optional G4 cold tier in front of RustFS.
//!
//! The hot TCP path does not call this crate. A miss still means the engine
//! recomputes. Without an authorized RustFS endpoint the live probe is
//! `NOT_RUN_HARDWARE`; the in-process mock still checks Put, Head, RangeGet,
//! and Delete plus object packing.

#![forbid(unsafe_code)]

mod http;
mod mock;
mod pack;
mod sigv4;

pub use http::{live_probe, ProbeReport, ProbeStatus};
pub use mock::MockObjectStore;
pub use pack::{contract_smoke, pack_blocks, ObjectMeta, ObjectStore, PackedBlock, PayloadRange};
