// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::error::{CacheError, QuotaKind};

pub const DEFAULT_MAX_FIELD_BYTES: u32 = 128;

/// Bounds checked while validating a [`crate::CacheIdentity`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityLimits {
    pub max_field_bytes: u32,
    pub max_prefix_links: u32,
    pub max_block_tokens: u32,
}

impl IdentityLimits {
    pub const fn from_resources(limits: &ResourceLimits) -> Self {
        Self {
            max_field_bytes: limits.max_field_bytes,
            max_prefix_links: limits.max_prefix_links,
            max_block_tokens: limits.max_block_tokens,
        }
    }
}

/// Hard caps enforced by MemoryStore. Every counter is a ceiling, not a hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    pub max_object_bytes: u64,
    pub max_in_flight_requests: u32,
    pub max_registrations: u32,
    pub max_in_flight_bytes_per_tenant: u64,
    pub max_retained_bytes: u64,
    pub max_retained_bytes_per_tenant: u64,
    pub max_prefix_links: u32,
    pub max_batch_items: u32,
    pub max_idempotency_entries: u32,
    pub idempotency_ttl_ms: u64,
    pub max_field_bytes: u32,
    pub max_block_tokens: u32,
}

impl ResourceLimits {
    /// Local development ceilings. Not a capacity plan and not a benchmark.
    pub const fn dev_defaults() -> Self {
        Self {
            max_object_bytes: 16 * 1024 * 1024,
            max_in_flight_requests: 128,
            max_registrations: 1024,
            max_in_flight_bytes_per_tenant: 64 * 1024 * 1024,
            max_retained_bytes: 256 * 1024 * 1024,
            max_retained_bytes_per_tenant: 128 * 1024 * 1024,
            max_prefix_links: 1024,
            max_batch_items: 64,
            max_idempotency_entries: 4096,
            idempotency_ttl_ms: 60_000,
            max_field_bytes: DEFAULT_MAX_FIELD_BYTES,
            max_block_tokens: 1_048_576,
        }
    }

    pub fn validate(&self) -> Result<(), CacheError> {
        if self.max_object_bytes == 0
            || self.max_in_flight_requests == 0
            || self.max_registrations == 0
            || self.max_in_flight_bytes_per_tenant == 0
            || self.max_retained_bytes == 0
            || self.max_retained_bytes_per_tenant == 0
            || self.max_prefix_links == 0
            || self.max_batch_items == 0
            || self.max_idempotency_entries == 0
            || self.idempotency_ttl_ms == 0
            || self.max_field_bytes == 0
            || self.max_block_tokens == 0
        {
            return Err(CacheError::Malformed {
                reason: "resource limit must be non-zero",
            });
        }
        if self.max_object_bytes > self.max_retained_bytes
            || self.max_in_flight_bytes_per_tenant > self.max_retained_bytes
            || self.max_retained_bytes_per_tenant > self.max_retained_bytes
        {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RetainedBytes,
            });
        }
        Ok(())
    }
}
