// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! In-process MemoryStore for the V1 A cache contract.
//!
//! File, NVMe, and RustFS backends are not implemented here. This crate does
//! not link a GPU, RDMA, or DPU library.

#![forbid(unsafe_code)]

mod memory;
mod transfer;

pub use memory::{MemoryStore, ReclaimRequest, StoreStats};
pub use transfer::{import_committed_block, ImportReport};

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;
