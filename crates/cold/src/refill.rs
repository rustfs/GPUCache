// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Budgeted refill with mandatory complete identity matching.
//!
//! The caller supplies an expected identity from its authenticated tenant and
//! trust-domain context. This library does not authenticate that caller. Raw
//! ObjectStore access is a trusted internal capability, not a tenant API.

use gpucache_core_types::{content_checksum, CacheError, CacheIdentity};

use crate::pack::{
    identity_digest, key_ok, manifest_version, read_manifest, validate_manifest_binding,
    ObjectStore, MANIFEST_VERSION,
};

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
    /// Manifest or payload exceeds the combined budget. No payload bytes.
    OverBudget,
    /// Missing or incompatible cache entry. The caller recomputes.
    Missing,
}

/// Fetch a generation only for its complete expected cache identity.
///
/// The identity must come from an authenticated upstream request. A generation
/// alone is not an identity, and this function has no unguarded overload.
pub fn refill_generation(
    store: &dyn ObjectStore,
    manifest_key: &str,
    generation: u64,
    budget: &RefillBudget,
    expected: &CacheIdentity,
) -> Result<RefillOutcome, CacheError> {
    refill_matching(store, manifest_key, generation, budget, expected)
}

/// Match tenant and complete canonical identity before any payload download.
/// Tenant mismatch is Unauthorized; other identity mismatch is a clean miss.
/// Legacy manifests lack the required binding and are clean misses too.
pub fn refill_matching(
    store: &dyn ObjectStore,
    manifest_key: &str,
    generation: u64,
    budget: &RefillBudget,
    expected: &CacheIdentity,
) -> Result<RefillOutcome, CacheError> {
    let expected_digest = identity_digest(expected)?;
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
    // Check the complete header first. An invalid/truncated magic is corrupt,
    // not a legacy miss. Versions 1/2 cannot prove a complete identity match.
    if manifest_version(&manifest_bytes)? != MANIFEST_VERSION {
        return Ok(RefillOutcome::Missing);
    }
    let manifest = read_manifest(&manifest_bytes)?;
    validate_manifest_binding(manifest_key, &manifest_bytes, &manifest)?;
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
    if range.tenant.as_slice() != expected.tenant.as_bytes() {
        return Err(CacheError::Unauthorized);
    }
    if range.identity_digest != expected_digest {
        return Ok(RefillOutcome::Missing);
    }
    if range.length > budget.max_bytes.saturating_sub(meta.len) {
        return Ok(RefillOutcome::OverBudget);
    }
    let payload = match store.range_get(&manifest.object_key, range.offset, range.length) {
        Ok(payload) => payload,
        Err(CacheError::Storage {
            reason: "object missing",
        }) => return Ok(RefillOutcome::Missing),
        Err(err) => return Err(err),
    };
    if payload.len() as u64 != range.length || content_checksum(&payload) != range.checksum {
        return Err(CacheError::ChecksumMismatch);
    }
    Ok(RefillOutcome::Loaded { payload })
}
