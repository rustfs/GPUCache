// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpucache_core_types::{
    content_checksum, synthetic_identity, CacheError, Digest, RequestId, ResourceLimits,
};
use gpucache_protocol::{
    identity_for_prefix_hit, AuthContext, CommitItem, GetItem, ItemHeader, LookupItem,
    LookupOutcome, MissReason, ReservePutItem, TenantPolicy,
};
use gpucache_storage::FileStore;

use crate::client::TcpClient;
use crate::server::{serve, ServerConfig};

/// Two TCP clients sharing one file-backed service.
///
/// Partial reuse is a whole-block prefix hit. A shorter `valid_length` on the
/// same chain is an incompatible miss: this demo does not slice inside a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareReport {
    pub exact_hit: &'static str,
    pub unknown_miss: &'static str,
    pub partial_prefix: &'static str,
    pub matched_prefix_blocks: u32,
    pub intra_block_slice: &'static str,
    pub layout_mismatch: &'static str,
    pub cross_tenant: &'static str,
    pub bytes_match: &'static str,
}

impl std::fmt::Display for ShareReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "status=REUSE_OK")?;
        writeln!(f, "exact_hit={}", self.exact_hit)?;
        writeln!(f, "unknown_miss={}", self.unknown_miss)?;
        writeln!(
            f,
            "partial_prefix={} matched_blocks={}",
            self.partial_prefix, self.matched_prefix_blocks
        )?;
        writeln!(f, "intra_block_slice={}", self.intra_block_slice)?;
        writeln!(f, "layout_mismatch={}", self.layout_mismatch)?;
        writeln!(f, "cross_tenant={}", self.cross_tenant)?;
        write!(f, "bytes_match={}", self.bytes_match)
    }
}

pub fn run_share_demo() -> Result<ShareReport, CacheError> {
    let dir = temp_dir();
    let result = run_in(&dir.path);
    let _ = fs::remove_dir_all(&dir.path);
    result
}

struct TempPath {
    path: PathBuf,
}

fn temp_dir() -> TempPath {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gpucache-share-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    TempPath { path }
}

fn run_in(path: &std::path::Path) -> Result<ShareReport, CacheError> {
    let writer_id = synthetic_identity(1, 1);
    let fingerprint = [9u8; 32];
    let other_fingerprint = [4u8; 32];
    let policy = TenantPolicy::new(
        writer_id.tenant.clone(),
        writer_id.trust_domain.clone(),
        fingerprint,
    )?;
    let mut other_id = synthetic_identity(3, 1);
    other_id.tenant = gpucache_core_types::TenantId::new(b"tenant-b".to_vec())?;
    let other_policy = TenantPolicy::new(
        other_id.tenant.clone(),
        other_id.trust_domain.clone(),
        other_fingerprint,
    )?;
    let store = FileStore::open(
        path,
        ResourceLimits::dev_defaults(),
        vec![policy, other_policy],
    )?;
    let server = serve(store, ServerConfig::default())?;
    let auth = AuthContext::new(
        writer_id.tenant.clone(),
        writer_id.trust_domain.clone(),
        fingerprint,
    )?;
    let mut writer = TcpClient::connect(
        server.addr(),
        auth.clone(),
        Duration::from_secs(5),
        1024 * 1024,
    )?;
    let mut reader = TcpClient::connect(server.addr(), auth, Duration::from_secs(5), 1024 * 1024)?;
    let payload = b"shared-kv-block".to_vec();
    let generation = put_commit(&mut writer, &payload, writer_id.clone())?;
    let hit = lookup_one(&mut reader, writer_id.clone())?;
    let LookupOutcome::Hit {
        generation: hit_generation,
        checksum,
        ..
    } = hit
    else {
        return Err(CacheError::Malformed {
            reason: "demo exact hit",
        });
    };
    if hit_generation != generation || checksum != content_checksum(&payload) {
        return Err(CacheError::ChecksumMismatch);
    }
    let got = get_one(&mut reader, generation, writer_id.clone())?;
    if got != payload {
        return Err(CacheError::ChecksumMismatch);
    }
    let mut unknown = writer_id.clone();
    unknown.prefix_chain[0] = Digest([0x91; 32]);
    match lookup_one(&mut reader, unknown)? {
        LookupOutcome::Miss {
            reason: MissReason::Absent,
        } => {}
        _ => {
            return Err(CacheError::Malformed {
                reason: "demo miss",
            })
        }
    }
    let mut longer = writer_id.clone();
    longer.prefix_chain.push(Digest([0xab; 32]));
    let prefix = lookup_one(&mut reader, longer.clone())?;
    let LookupOutcome::PrefixHit {
        generation: prefix_generation,
        valid_length,
        matched_prefix_blocks,
        checksum: prefix_checksum,
        ..
    } = prefix
    else {
        return Err(CacheError::Malformed {
            reason: "demo prefix",
        });
    };
    if matched_prefix_blocks != 1 || prefix_checksum != content_checksum(&payload) {
        return Err(CacheError::Incompatible);
    }
    let matched = identity_for_prefix_hit(&longer, valid_length, matched_prefix_blocks)?;
    if get_one(&mut reader, prefix_generation, longer.clone()).is_ok() {
        return Err(CacheError::Incompatible);
    }
    let prefix_bytes = get_one(&mut reader, prefix_generation, matched)?;
    if prefix_bytes != payload {
        return Err(CacheError::ChecksumMismatch);
    }
    let mut sliced = writer_id.clone();
    sliced.valid_length = 4;
    match lookup_one(&mut reader, sliced)? {
        LookupOutcome::Miss {
            reason: MissReason::Incompatible,
        } => {}
        _ => {
            return Err(CacheError::Malformed {
                reason: "demo slice",
            })
        }
    }
    let mut layout = writer_id.clone();
    layout.layout.family = 9;
    match lookup_one(&mut reader, layout)? {
        LookupOutcome::Miss {
            reason: MissReason::Incompatible,
        } => {}
        _ => {
            return Err(CacheError::Malformed {
                reason: "demo layout",
            })
        }
    }
    let foreign = lookup_raw(&mut reader, other_id);
    if !matches!(foreign, Err(CacheError::Unauthorized)) {
        return Err(CacheError::Malformed {
            reason: "demo tenant",
        });
    }
    Ok(ShareReport {
        exact_hit: "PASS",
        unknown_miss: "PASS",
        partial_prefix: "WHOLE_BLOCK_PREFIX",
        matched_prefix_blocks,
        intra_block_slice: "DEGRADED_MISS",
        layout_mismatch: "MISS",
        cross_tenant: "UNAUTHORIZED",
        bytes_match: "PASS",
    })
}

fn put_commit(
    client: &mut TcpClient,
    payload: &[u8],
    identity: gpucache_core_types::CacheIdentity,
) -> Result<u64, CacheError> {
    let epoch = client.epoch();
    let reserved = client.reserve_put(vec![ReservePutItem {
        header: header(client, 1, epoch),
        identity: identity.clone(),
        generation: 0,
        length: payload.len() as u64,
        checksum: content_checksum(payload),
        payload: Some(payload.to_vec()),
        lease_ttl_ms: 30_000,
    }])?;
    let generation = match reserved.items.into_iter().next() {
        Some(item) => item.body?.generation,
        None => {
            return Err(CacheError::Malformed {
                reason: "demo reserve",
            })
        }
    };
    let committed = client.commit(vec![CommitItem {
        header: header(client, 2, epoch),
        generation,
        length: payload.len() as u64,
        checksum: content_checksum(payload),
    }])?;
    match committed.items.into_iter().next() {
        Some(item) => {
            item.body?;
            Ok(generation)
        }
        None => Err(CacheError::Malformed {
            reason: "demo commit",
        }),
    }
}

fn lookup_one(
    client: &mut TcpClient,
    identity: gpucache_core_types::CacheIdentity,
) -> Result<LookupOutcome, CacheError> {
    let epoch = client.epoch();
    let reply = client.lookup(vec![LookupItem {
        header: header(client, 3, epoch),
        identity,
    }])?;
    match reply.items.into_iter().next() {
        Some(item) => item.body,
        None => Err(CacheError::Malformed {
            reason: "demo lookup",
        }),
    }
}

fn lookup_raw(
    client: &mut TcpClient,
    identity: gpucache_core_types::CacheIdentity,
) -> Result<LookupOutcome, CacheError> {
    lookup_one(client, identity)
}

fn get_one(
    client: &mut TcpClient,
    generation: u64,
    identity: gpucache_core_types::CacheIdentity,
) -> Result<Vec<u8>, CacheError> {
    let epoch = client.epoch();
    let reply = client.get(vec![GetItem {
        header: header(client, 4, epoch),
        generation,
        identity,
        pin_ttl_ms: 1_000,
    }])?;
    match reply.items.into_iter().next() {
        Some(item) => Ok(item.body?.payload),
        None => Err(CacheError::Malformed { reason: "demo get" }),
    }
}

fn header(client: &TcpClient, n: u128, epoch: u64) -> ItemHeader {
    ItemHeader {
        request_id: RequestId::from_counter(n).unwrap_or(RequestId([1; 16])),
        epoch,
        deadline_ms: client.deadline(30_000),
    }
}
