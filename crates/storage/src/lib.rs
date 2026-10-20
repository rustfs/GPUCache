// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! MemoryStore and a file-backed store for the V1 cache contract.
//!
//! The file backend writes ordinary files under a caller-chosen directory.
//! It does not format disks. RustFS is a separate cold-tier crate and is not
//! on this hot path. This crate does not link a GPU, RDMA, or DPU library.

#![forbid(unsafe_code)]

mod engine_spike;
mod file;
mod memory;
mod synthetic;
mod transfer;

pub use engine_spike::{EngineBlockContract, ENGINE_SPIKE_ABI_VERSION};
pub use file::{FileStore, PersistFault};
pub use memory::{MemoryStore, ReclaimRequest, StoreStats};
pub use synthetic::{decode_synthetic_kv, deterministic_recompute, encode_synthetic_kv};
pub use transfer::{import_committed_block, ImportReport};

#[cfg(test)]
mod props;
#[cfg(test)]
mod tests;
