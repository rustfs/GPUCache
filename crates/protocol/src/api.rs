// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{
    Attestation, BlockState, CacheError, CacheIdentity, Checksum, RequestId,
};

/// Every mutating and read item carries the fencing epoch and a deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemHeader {
    pub request_id: RequestId,
    pub epoch: u64,
    pub deadline_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupItem {
    pub header: ItemHeader,
    pub identity: CacheIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservePutItem {
    pub header: ItemHeader,
    pub identity: CacheIdentity,
    /// Zero allocates a generation. Any other value continues that reservation.
    pub generation: u64,
    pub length: u64,
    pub checksum: Checksum,
    pub payload: Option<Vec<u8>>,
    pub lease_ttl_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitItem {
    pub header: ItemHeader,
    pub generation: u64,
    pub length: u64,
    pub checksum: Checksum,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetItem {
    pub header: ItemHeader,
    pub generation: u64,
    pub identity: CacheIdentity,
    pub pin_ttl_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseCancelItem {
    pub header: ItemHeader,
    pub generation: u64,
    pub region_id: Option<u64>,
    /// Set when releasing a reader pin created by `Get`.
    pub pin_owner: Option<RequestId>,
    pub attestation: Attestation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidateItem {
    pub header: ItemHeader,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestBody {
    Lookup(Vec<LookupItem>),
    ReservePut(Vec<ReservePutItem>),
    Commit(Vec<CommitItem>),
    Get(Vec<GetItem>),
    ReleaseCancel(Vec<ReleaseCancelItem>),
    Invalidate(Vec<InvalidateItem>),
}

impl RequestBody {
    pub fn len(&self) -> usize {
        match self {
            Self::Lookup(v) => v.len(),
            Self::ReservePut(v) => v.len(),
            Self::Commit(v) => v.len(),
            Self::Get(v) => v.len(),
            Self::ReleaseCancel(v) => v.len(),
            Self::Invalidate(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissReason {
    Absent,
    NotCommitted,
    Incompatible,
    Evicting,
    Quarantined,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupOutcome {
    /// Exact physical-key match. The bytes cover the whole requested block.
    Hit {
        generation: u64,
        length: u64,
        checksum: Checksum,
        valid_length: u32,
    },
    /// Whole stored block whose prefix chain is a strict prefix of the query.
    ///
    /// This is not a sliced block. `matched_prefix_blocks` is the stored chain
    /// length. `Get` must use [`identity_for_prefix_hit`], not the longer query.
    PrefixHit {
        generation: u64,
        length: u64,
        checksum: Checksum,
        valid_length: u32,
        matched_prefix_blocks: u32,
    },
    Miss {
        reason: MissReason,
    },
}

/// Rebuild the stored identity for a [`LookupOutcome::PrefixHit`].
///
/// The result keeps the query layout and truncates the hash chain. It does not
/// invent bytes for the suffix, and it rejects a chain that is not strictly shorter.
pub fn identity_for_prefix_hit(
    query: &CacheIdentity,
    valid_length: u32,
    matched_prefix_blocks: u32,
) -> Result<CacheIdentity, CacheError> {
    let n = matched_prefix_blocks as usize;
    if matched_prefix_blocks == 0 || n >= query.prefix_chain.len() {
        return Err(CacheError::Malformed {
            reason: "prefix hit alignment",
        });
    }
    if valid_length == 0 || valid_length > query.block_token_count {
        return Err(CacheError::IdentityInvalid {
            reason: "valid length",
        });
    }
    let mut matched = query.clone();
    matched.prefix_chain.truncate(n);
    matched.valid_length = valid_length;
    Ok(matched)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReserveOutcome {
    pub generation: u64,
    pub state: BlockState,
    pub region_id: Option<u64>,
    pub lease_deadline_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitOutcome {
    pub generation: u64,
    pub length: u64,
    pub checksum: Checksum,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetHit {
    pub generation: u64,
    pub length: u64,
    pub checksum: Checksum,
    pub region_id: u64,
    pub lease_deadline_ms: u64,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseOutcome {
    pub generation: u64,
    pub state: BlockState,
    pub quarantined: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidateOutcome {
    pub generation: u64,
    pub state: BlockState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemResult<T> {
    pub request_id: RequestId,
    pub body: Result<T, CacheError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReply<T> {
    pub store_epoch: u64,
    pub items: Vec<ItemResult<T>>,
}
