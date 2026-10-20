// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Object-store failures that indicate temporary unavailability. Corruption,
//! invalid requests, authorization failures and unknown labels fail closed.

use gpucache_core_types::CacheError;

pub(crate) fn transient(error: &CacheError) -> bool {
    matches!(
        error,
        CacheError::TimedOut
            | CacheError::Storage {
                reason: "object store" | "object unavailable"
            }
            | CacheError::QuotaExceeded { .. }
    )
}

pub(crate) fn refill_miss(error: &CacheError) -> bool {
    transient(error)
        || matches!(
            error,
            CacheError::Storage {
                reason: "object missing"
            }
        )
}
