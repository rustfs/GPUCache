// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{
    content_checksum, encode_logical_key, encode_physical_key, verify_checksum, CacheError,
    IdentityLimits, RequestId, ResourceLimits,
};
use gpucache_protocol::{
    Attestation, AuthContext, CommitItem, CommittedExport, GetItem, ItemHeader, LookupItem,
    LookupOutcome, ReleaseCancelItem, ReservePutItem, TenantPolicy,
};

use crate::memory::MemoryStore;

/// Result of importing one synthetic or engine-produced block into a fresh store.
///
/// V1 A tests use synthetic bytes. A real vLLM export is `NOT_RUN_HARDWARE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub physical_key_hex: String,
    pub logical_key_hex: String,
    pub checksum_hex: String,
    pub length: u64,
    pub source_generation: u64,
    pub local_generation: u64,
}

pub fn import_committed_block(
    frame: &CommittedExport,
    credential_fingerprint: [u8; 32],
    now_ms: u64,
) -> Result<ImportReport, CacheError> {
    verify_checksum(&frame.payload, frame.checksum)?;
    if content_checksum(&frame.payload) != frame.checksum {
        return Err(CacheError::ChecksumMismatch);
    }
    let limits = ResourceLimits::dev_defaults();
    let identity_limits = IdentityLimits::from_resources(&limits);
    frame.identity.validate(&identity_limits)?;
    let policy = TenantPolicy::new(
        frame.identity.tenant.clone(),
        frame.identity.trust_domain.clone(),
        credential_fingerprint,
    )?;
    let auth = AuthContext::new(
        frame.identity.tenant.clone(),
        frame.identity.trust_domain.clone(),
        credential_fingerprint,
    )?;
    let mut store = MemoryStore::new(limits, vec![policy])?;
    let epoch = store.epoch();
    let deadline = now_ms.saturating_add(60_000);
    let reserved = store.reserve_put(
        &auth,
        now_ms,
        vec![ReservePutItem {
            header: ItemHeader {
                request_id: RequestId::from_counter(1)?,
                epoch,
                deadline_ms: deadline,
            },
            identity: frame.identity.clone(),
            generation: 0,
            length: frame.payload.len() as u64,
            checksum: frame.checksum,
            payload: Some(frame.payload.clone()),
            lease_ttl_ms: 30_000,
        }],
    )?;
    let reserved = one(reserved.items)?;
    let committed = store.commit(
        &auth,
        now_ms,
        vec![CommitItem {
            header: ItemHeader {
                request_id: RequestId::from_counter(2)?,
                epoch: store.epoch(),
                deadline_ms: deadline,
            },
            generation: reserved.generation,
            length: frame.payload.len() as u64,
            checksum: frame.checksum,
        }],
    )?;
    let committed = one(committed.items)?;
    let looked = store.lookup(
        &auth,
        now_ms,
        vec![LookupItem {
            header: ItemHeader {
                request_id: RequestId::from_counter(3)?,
                epoch: store.epoch(),
                deadline_ms: deadline,
            },
            identity: frame.identity.clone(),
        }],
    )?;
    match one(looked.items)? {
        LookupOutcome::Hit { checksum, .. } if checksum == frame.checksum => {}
        LookupOutcome::Hit { .. } => return Err(CacheError::ChecksumMismatch),
        LookupOutcome::PrefixHit { .. } => return Err(CacheError::Incompatible),
        LookupOutcome::Miss { .. } => {
            return Err(CacheError::NotExposed {
                state: gpucache_core_types::BlockState::Absent,
            })
        }
    }
    let got = store.get(
        &auth,
        now_ms,
        vec![GetItem {
            header: ItemHeader {
                request_id: RequestId::from_counter(4)?,
                epoch: store.epoch(),
                deadline_ms: deadline,
            },
            generation: committed.generation,
            identity: frame.identity.clone(),
            pin_ttl_ms: 1_000,
        }],
    )?;
    let hit = one(got.items)?;
    if hit.payload != frame.payload {
        return Err(CacheError::ChecksumMismatch);
    }
    let _ = store.release_cancel(
        &auth,
        now_ms,
        vec![ReleaseCancelItem {
            header: ItemHeader {
                request_id: RequestId::from_counter(5)?,
                epoch: store.epoch(),
                deadline_ms: deadline,
            },
            generation: committed.generation,
            region_id: Some(hit.region_id),
            pin_owner: Some(RequestId::from_counter(4)?),
            attestation: Attestation::Drained,
        }],
    )?;
    let physical = encode_physical_key(&frame.identity, &identity_limits)?;
    let logical = encode_logical_key(&frame.identity, &identity_limits)?;
    Ok(ImportReport {
        physical_key_hex: hex(&physical),
        logical_key_hex: hex(&logical),
        checksum_hex: hex(&frame.checksum.0),
        length: frame.payload.len() as u64,
        source_generation: frame.source_generation,
        local_generation: committed.generation,
    })
}

fn one<T>(mut items: Vec<gpucache_protocol::ItemResult<T>>) -> Result<T, CacheError> {
    if items.len() != 1 {
        return Err(CacheError::Malformed {
            reason: "import batch",
        });
    }
    match items.pop() {
        Some(item) => item.body,
        None => Err(CacheError::Malformed {
            reason: "import batch",
        }),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}
