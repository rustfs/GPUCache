// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use core::fmt;

use crate::lifecycle::BlockState;

/// Which bounded resource rejected an admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaKind {
    InFlightRequests,
    Registrations,
    InFlightBytes,
    RetainedBytes,
    TenantRetainedBytes,
    BatchItems,
    IdempotencyTable,
    PrefixLinks,
    /// Per-tenant admission rate. Not a durability quota.
    RateLimit,
}

/// Recoverable contract failure. Payloads and prompts are never embedded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheError {
    InvalidTransition {
        from: BlockState,
        event: &'static str,
    },
    NotReclaimable {
        from: BlockState,
    },
    InUse {
        refs: u32,
    },
    FencingRejected {
        reason: &'static str,
    },
    ChecksumMismatch,
    LengthMismatch {
        expected: u64,
        actual: u64,
    },
    GenerationMismatch {
        expected: u64,
        actual: u64,
    },
    UnknownGeneration,
    IdempotencyConflict,
    QuotaExceeded {
        kind: QuotaKind,
    },
    ObjectTooLarge {
        limit: u64,
        actual: u64,
    },
    Unauthorized,
    TimedOut,
    Cancelled,
    Malformed {
        reason: &'static str,
    },
    WireVersionUnsupported {
        local: u16,
        remote: u16,
    },
    NotExposed {
        state: BlockState,
    },
    Incompatible,
    EpochMismatch {
        store: u64,
        request: u64,
    },
    IdentityInvalid {
        reason: &'static str,
    },
    /// Local storage or transport failure. The reason is a static label, never a path or payload.
    Storage {
        reason: &'static str,
    },
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, event } => {
                write!(f, "invalid transition from {from:?} on {event}")
            }
            Self::NotReclaimable { from } => write!(f, "buffer not reclaimable from {from:?}"),
            Self::InUse { refs } => write!(f, "buffer still referenced ({refs})"),
            Self::FencingRejected { reason } => write!(f, "fencing rejected: {reason}"),
            Self::ChecksumMismatch => write!(f, "checksum mismatch"),
            Self::LengthMismatch { expected, actual } => {
                write!(f, "length mismatch expected {expected} actual {actual}")
            }
            Self::GenerationMismatch { expected, actual } => {
                write!(f, "generation mismatch expected {expected} actual {actual}")
            }
            Self::UnknownGeneration => write!(f, "unknown generation"),
            Self::IdempotencyConflict => write!(f, "idempotency conflict"),
            Self::QuotaExceeded { kind } => write!(f, "quota exceeded: {kind:?}"),
            Self::ObjectTooLarge { limit, actual } => {
                write!(f, "object too large limit {limit} actual {actual}")
            }
            Self::Unauthorized => write!(f, "unauthorized"),
            Self::TimedOut => write!(f, "timed out"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Malformed { reason } => write!(f, "malformed: {reason}"),
            Self::WireVersionUnsupported { local, remote } => {
                write!(f, "wire version unsupported local {local} remote {remote}")
            }
            Self::NotExposed { state } => write!(f, "block not exposed in state {state:?}"),
            Self::Incompatible => write!(f, "incompatible cache identity"),
            Self::EpochMismatch { store, request } => {
                write!(f, "epoch mismatch store {store} request {request}")
            }
            Self::IdentityInvalid { reason } => write!(f, "identity invalid: {reason}"),
            Self::Storage { reason } => write!(f, "storage: {reason}"),
        }
    }
}

impl std::error::Error for CacheError {}
