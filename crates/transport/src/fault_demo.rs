// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! End-to-end hit, miss, and recompute fallback on a clean temp directory.
//!
//! Token comparison is exact. A wrong tenant or wrong model is not a hit.
//! Nothing here talks to a GPU or an inference engine.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use gpucache_core_types::{
    content_checksum, synthetic_identity, AdapterIdentity, Attestation, CacheError, Digest,
    RequestId, ResourceLimits, TenantId,
};
use gpucache_protocol::{
    AuthContext, CommitItem, GetItem, ItemHeader, LookupItem, LookupOutcome, MissReason,
    ReleaseCancelItem, ReservePutItem, TenantPolicy,
};
use gpucache_storage::{
    decode_synthetic_kv, deterministic_recompute, encode_synthetic_kv, FileStore,
};

use crate::client::TcpClient;
use crate::server::{serve, ServerConfig};

const FP_A: [u8; 32] = [9u8; 32];
const FP_B: [u8; 32] = [4u8; 32];
const TOKENS: [u32; 4] = [11, 22, 33, 44];

/// Result of one clean-environment fallback run. Printed only after the checks pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultReport {
    pub hit_tokens: u32,
    pub miss: &'static str,
    pub corrupt_fallback: &'static str,
    pub timeout_fallback: &'static str,
    pub cancel_fallback: &'static str,
    pub restart: &'static str,
    pub restart_down: &'static str,
    pub wrong_tenant: &'static str,
    pub wrong_model: &'static str,
    pub wrong_tokenizer: &'static str,
    pub wrong_adapter: &'static str,
    pub layout_mismatch: &'static str,
    pub quota_fallback: &'static str,
    pub duplicate: &'static str,
    pub reorder: &'static str,
}

impl std::fmt::Display for FaultReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "status=FALLBACK_OK")?;
        writeln!(f, "criteria=EXACT_TOKEN_IDS")?;
        writeln!(f, "tolerance=NONE")?;
        writeln!(f, "hit=PASS tokens={}", self.hit_tokens)?;
        writeln!(f, "miss={}", self.miss)?;
        writeln!(f, "corrupt_fallback={}", self.corrupt_fallback)?;
        writeln!(f, "timeout_fallback={}", self.timeout_fallback)?;
        writeln!(f, "cancel_fallback={}", self.cancel_fallback)?;
        writeln!(f, "restart={}", self.restart)?;
        writeln!(f, "restart_down={}", self.restart_down)?;
        writeln!(f, "wrong_tenant={}", self.wrong_tenant)?;
        writeln!(f, "wrong_model={}", self.wrong_model)?;
        writeln!(f, "wrong_tokenizer={}", self.wrong_tokenizer)?;
        writeln!(f, "wrong_adapter={}", self.wrong_adapter)?;
        writeln!(f, "layout_mismatch={}", self.layout_mismatch)?;
        writeln!(f, "quota_fallback={}", self.quota_fallback)?;
        writeln!(f, "duplicate={}", self.duplicate)?;
        write!(f, "reorder={}", self.reorder)
    }
}

pub fn run_fault_demo() -> Result<FaultReport, CacheError> {
    let dir = temp_dir()?;
    let result = run_in(&dir.path);
    let _ = fs::remove_dir_all(&dir.path);
    result
}

struct TempPath {
    path: PathBuf,
}

fn temp_dir() -> Result<TempPath, CacheError> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "gpucache-fault-demo-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).map_err(|_| CacheError::Storage { reason: "store io" })?;
    let path = fs::canonicalize(&path).map_err(|_| CacheError::Storage { reason: "store io" })?;
    Ok(TempPath { path })
}

fn run_in(path: &Path) -> Result<FaultReport, CacheError> {
    let writer_id = synthetic_identity(1, 1);
    let mut other_id = synthetic_identity(1, 1);
    other_id.tenant = TenantId::new(b"tenant-b".to_vec())?;
    let policy_a = TenantPolicy::new(
        writer_id.tenant.clone(),
        writer_id.trust_domain.clone(),
        FP_A,
    )?;
    let policy_b = TenantPolicy::new(other_id.tenant.clone(), other_id.trust_domain.clone(), FP_B)?;
    let payload = encode_synthetic_kv(&TOKENS)?;
    let store = FileStore::open(
        path,
        ResourceLimits::dev_defaults(),
        vec![policy_a.clone(), policy_b],
    )?;
    let server = serve(store, ServerConfig::default())?;
    let addr = server.addr();
    let auth = AuthContext::new(
        writer_id.tenant.clone(),
        writer_id.trust_domain.clone(),
        FP_A,
    )?;
    let mut writer = connect(addr, auth.clone())?;
    let mut reader = connect(addr, auth.clone())?;
    let generation = put_commit(&mut writer, &payload, writer_id.clone(), 10)?;
    let hit_tokens = expect_exact_hit(&mut reader, &writer_id, generation, &TOKENS, 300)?;
    let miss = expect_miss(
        &mut reader,
        &mut_prefix(writer_id.clone(), 9)?,
        MissReason::Absent,
    )?;
    fallback_matches(&TOKENS)?;
    thread::sleep(Duration::from_millis(20));
    let timed = reader.lookup(vec![LookupItem {
        header: ItemHeader {
            request_id: RequestId::from_counter(40)?,
            epoch: reader.epoch(),
            deadline_ms: 1,
        },
        identity: writer_id.clone(),
    }])?;
    if timed.items[0].body != Err(CacheError::TimedOut) {
        return Err(CacheError::Malformed {
            reason: "fault demo timeout",
        });
    }
    expect_exact_hit(&mut reader, &writer_id, generation, &TOKENS, 310)?;
    let cancel_id = mut_prefix(writer_id.clone(), 3)?;
    let cancel_generation = reserve_only(&mut writer, &payload, cancel_id.clone(), 50)?;
    let cancelled = writer.release_cancel(vec![ReleaseCancelItem {
        header: header(&writer, 52)?,
        generation: cancel_generation,
        region_id: None,
        pin_owner: None,
        attestation: Attestation::Unknown,
    }])?;
    if cancelled.items[0]
        .body
        .as_ref()
        .map(|item| item.quarantined)
        != Ok(true)
    {
        return Err(CacheError::Malformed {
            reason: "fault demo cancel",
        });
    }
    let cancel_fallback = expect_miss(&mut reader, &cancel_id, MissReason::Quarantined)?;
    let blocked = reader.get(vec![GetItem {
        header: header(&reader, 53)?,
        generation: cancel_generation,
        identity: cancel_id,
        pin_ttl_ms: 100,
    }])?;
    if blocked.items[0].body != Err(CacheError::Cancelled) {
        return Err(CacheError::Malformed {
            reason: "fault demo cancel get",
        });
    }
    let duplicate = replay_same_commit(&mut writer, &payload, writer_id.clone())?;
    let reorder = commit_unknown_generation(&mut writer)?;
    let wrong_tenant = tenant_denied(addr, &other_id, &writer_id)?;
    let wrong_model = {
        let mut id = writer_id.clone();
        id.model_revision.0[4] ^= 0x5a;
        expect_miss(&mut reader, &id, MissReason::Absent)?
    };
    let wrong_tokenizer = {
        let mut id = writer_id.clone();
        id.tokenizer_revision.0[4] ^= 0x5a;
        expect_miss(&mut reader, &id, MissReason::Absent)?
    };
    let wrong_adapter = {
        let mut id = writer_id.clone();
        id.adapter = AdapterIdentity::Lora {
            revision: Digest([9u8; 32]),
        };
        expect_miss(&mut reader, &id, MissReason::Absent)?
    };
    let layout_mismatch = {
        let mut id = writer_id.clone();
        id.layout.family = 2;
        expect_miss(&mut reader, &id, MissReason::Incompatible)?
    };
    drop(writer);
    drop(reader);
    drop(server);
    let restart_down =
        match TcpClient::connect(addr, auth.clone(), Duration::from_millis(200), 1024 * 1024) {
            Err(CacheError::Storage { .. } | CacheError::TimedOut) => "TRANSPORT_FALLBACK",
            Ok(_) => {
                return Err(CacheError::Malformed {
                    reason: "fault demo restart",
                })
            }
            Err(_) => {
                return Err(CacheError::Malformed {
                    reason: "fault demo restart",
                })
            }
        };
    let block_path = path.join("blocks").join(format!("{generation:016x}.gblk"));
    {
        let store = FileStore::open(
            path,
            ResourceLimits::dev_defaults(),
            vec![policy_a.clone(), policy_b_from(&other_id)?],
        )?;
        let server = serve(store, ServerConfig::default())?;
        let mut reader = connect(server.addr(), auth.clone())?;
        expect_exact_hit(&mut reader, &writer_id, generation, &TOKENS, 320)?;
        drop(reader);
        drop(server);
    }
    corrupt_last_byte(&block_path)?;
    let store = FileStore::open(
        path,
        ResourceLimits::dev_defaults(),
        vec![policy_a, policy_b_from(&other_id)?],
    )?;
    if store.skipped_corrupt() == 0 {
        return Err(CacheError::Malformed {
            reason: "fault demo corrupt",
        });
    }
    let server = serve(store, ServerConfig::default())?;
    let mut reader = connect(server.addr(), auth)?;
    let corrupt_fallback = expect_miss(&mut reader, &writer_id, MissReason::Absent)?;
    fallback_matches(&TOKENS)?;
    drop(reader);
    drop(server);
    let quota_fallback = quota_fallback_server()?;
    Ok(FaultReport {
        hit_tokens,
        miss,
        corrupt_fallback,
        timeout_fallback: "RECOMPUTE",
        cancel_fallback,
        restart: "HIT_EXACT",
        restart_down,
        wrong_tenant,
        wrong_model,
        wrong_tokenizer,
        wrong_adapter,
        layout_mismatch,
        quota_fallback,
        duplicate,
        reorder,
    })
}

fn policy_b_from(
    other_id: &gpucache_core_types::CacheIdentity,
) -> Result<TenantPolicy, CacheError> {
    TenantPolicy::new(other_id.tenant.clone(), other_id.trust_domain.clone(), FP_B)
}

fn connect(addr: SocketAddr, auth: AuthContext) -> Result<TcpClient, CacheError> {
    TcpClient::connect(addr, auth, Duration::from_secs(5), 1024 * 1024)
}

fn header(client: &TcpClient, n: u128) -> Result<ItemHeader, CacheError> {
    Ok(ItemHeader {
        request_id: RequestId::from_counter(n)?,
        epoch: client.epoch(),
        deadline_ms: client.deadline(30_000),
    })
}

fn put_commit(
    client: &mut TcpClient,
    payload: &[u8],
    identity: gpucache_core_types::CacheIdentity,
    n: u128,
) -> Result<u64, CacheError> {
    let generation = reserve_only(client, payload, identity, n)?;
    let committed = client.commit(vec![CommitItem {
        header: header(client, n + 1)?,
        generation,
        length: payload.len() as u64,
        checksum: content_checksum(payload),
    }])?;
    if committed.items[0].body.is_err() {
        return Err(CacheError::Malformed {
            reason: "fault demo commit",
        });
    }
    Ok(generation)
}

fn reserve_only(
    client: &mut TcpClient,
    payload: &[u8],
    identity: gpucache_core_types::CacheIdentity,
    n: u128,
) -> Result<u64, CacheError> {
    let reserved = client.reserve_put(vec![ReservePutItem {
        header: header(client, n)?,
        identity,
        generation: 0,
        length: payload.len() as u64,
        checksum: content_checksum(payload),
        payload: Some(payload.to_vec()),
        lease_ttl_ms: 30_000,
    }])?;
    reserved.items[0]
        .body
        .as_ref()
        .map(|item| item.generation)
        .map_err(|_| CacheError::Malformed {
            reason: "fault demo reserve",
        })
}

fn expect_exact_hit(
    client: &mut TcpClient,
    identity: &gpucache_core_types::CacheIdentity,
    generation: u64,
    tokens: &[u32],
    n: u128,
) -> Result<u32, CacheError> {
    let looked = client.lookup(vec![LookupItem {
        header: header(client, n)?,
        identity: identity.clone(),
    }])?;
    let LookupOutcome::Hit {
        generation: hit_generation,
        ..
    } = looked.items[0]
        .body
        .as_ref()
        .map_err(|_| CacheError::Malformed {
            reason: "fault demo hit",
        })?
    else {
        return Err(CacheError::Malformed {
            reason: "fault demo hit",
        });
    };
    if *hit_generation != generation {
        return Err(CacheError::Malformed {
            reason: "fault demo hit",
        });
    }
    let got = client.get(vec![GetItem {
        header: header(client, n + 1)?,
        generation,
        identity: identity.clone(),
        pin_ttl_ms: 1_000,
    }])?;
    let hit = got.items[0]
        .body
        .as_ref()
        .map_err(|_| CacheError::Malformed {
            reason: "fault demo hit",
        })?;
    let decoded = decode_synthetic_kv(&hit.payload)?;
    if decoded != tokens {
        return Err(CacheError::Malformed {
            reason: "fault demo token mismatch",
        });
    }
    let region_id = hit.region_id;
    client.release_cancel(vec![ReleaseCancelItem {
        header: header(client, n + 2)?,
        generation,
        region_id: Some(region_id),
        pin_owner: Some(RequestId::from_counter(n + 1)?),
        attestation: Attestation::Drained,
    }])?;
    u32::try_from(decoded.len()).map_err(|_| CacheError::Malformed {
        reason: "fault demo hit",
    })
}

fn expect_miss(
    client: &mut TcpClient,
    identity: &gpucache_core_types::CacheIdentity,
    reason: MissReason,
) -> Result<&'static str, CacheError> {
    let looked = client.lookup(vec![LookupItem {
        header: header(client, 210 + u128::from(reason_tag(reason)))?,
        identity: identity.clone(),
    }])?;
    match &looked.items[0].body {
        Ok(LookupOutcome::Miss { reason: got }) if *got == reason => Ok(miss_label(reason)),
        _ => Err(CacheError::Malformed {
            reason: "fault demo miss",
        }),
    }
}

fn reason_tag(reason: MissReason) -> u8 {
    match reason {
        MissReason::Absent => 1,
        MissReason::NotCommitted => 2,
        MissReason::Incompatible => 3,
        MissReason::Evicting => 4,
        MissReason::Quarantined => 5,
        MissReason::Expired => 6,
    }
}

fn miss_label(reason: MissReason) -> &'static str {
    match reason {
        MissReason::Absent => "MISS_ABSENT",
        MissReason::NotCommitted => "MISS_NOT_COMMITTED",
        MissReason::Incompatible => "MISS_INCOMPATIBLE",
        MissReason::Evicting => "MISS_EVICTING",
        MissReason::Quarantined => "MISS_QUARANTINED",
        MissReason::Expired => "MISS_EXPIRED",
    }
}

fn fallback_matches(tokens: &[u32]) -> Result<(), CacheError> {
    let recomputed = deterministic_recompute(tokens)?;
    if decode_synthetic_kv(&recomputed)? != tokens {
        return Err(CacheError::Malformed {
            reason: "fault demo recompute",
        });
    }
    Ok(())
}

fn mut_prefix(
    mut identity: gpucache_core_types::CacheIdentity,
    mark: u8,
) -> Result<gpucache_core_types::CacheIdentity, CacheError> {
    identity.prefix_chain[0].0[2] = mark;
    Ok(identity)
}

fn replay_same_commit(
    client: &mut TcpClient,
    payload: &[u8],
    identity: gpucache_core_types::CacheIdentity,
) -> Result<&'static str, CacheError> {
    let mut identity = identity;
    identity.prefix_chain[0].0[2] = 4;
    let generation = reserve_only(client, payload, identity.clone(), 60)?;
    let item = CommitItem {
        header: header(client, 61)?,
        generation,
        length: payload.len() as u64,
        checksum: content_checksum(payload),
    };
    let first = client.commit(vec![item.clone()])?;
    let second = client.commit(vec![item])?;
    if first.items[0].body != second.items[0].body || first.items[0].body.is_err() {
        return Err(CacheError::Malformed {
            reason: "fault demo duplicate",
        });
    }
    let again = client.reserve_put(vec![ReservePutItem {
        header: header(client, 62)?,
        identity,
        generation: 0,
        length: payload.len() as u64,
        checksum: content_checksum(payload),
        payload: Some(payload.to_vec()),
        lease_ttl_ms: 30_000,
    }])?;
    if !matches!(
        again.items[0].body,
        Err(CacheError::InvalidTransition { .. })
    ) {
        return Err(CacheError::Malformed {
            reason: "fault demo duplicate",
        });
    }
    Ok("REPLAY_ONE_BLOCK")
}

fn commit_unknown_generation(client: &mut TcpClient) -> Result<&'static str, CacheError> {
    let early = client.commit(vec![CommitItem {
        header: header(client, 70)?,
        generation: 99,
        length: 4,
        checksum: content_checksum(&[1, 2, 3, 4]),
    }])?;
    if early.items[0].body != Err(CacheError::UnknownGeneration) {
        return Err(CacheError::Malformed {
            reason: "fault demo reorder",
        });
    }
    Ok("REJECTED")
}

fn tenant_denied(
    addr: SocketAddr,
    other_id: &gpucache_core_types::CacheIdentity,
    writer_id: &gpucache_core_types::CacheIdentity,
) -> Result<&'static str, CacheError> {
    let auth = AuthContext::new(other_id.tenant.clone(), other_id.trust_domain.clone(), FP_B)?;
    let mut intruder = connect(addr, auth)?;
    let denied = intruder.lookup(vec![LookupItem {
        header: header(&intruder, 80)?,
        identity: writer_id.clone(),
    }])?;
    if denied.items[0].body != Err(CacheError::Unauthorized) {
        return Err(CacheError::Malformed {
            reason: "fault demo tenant",
        });
    }
    Ok("UNAUTHORIZED")
}

fn corrupt_last_byte(path: &Path) -> Result<(), CacheError> {
    let mut bytes = fs::read(path).map_err(|_| CacheError::Storage { reason: "store io" })?;
    if bytes.is_empty() {
        return Err(CacheError::Malformed {
            reason: "fault demo corrupt",
        });
    }
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(path, bytes).map_err(|_| CacheError::Storage { reason: "store io" })?;
    Ok(())
}

fn quota_fallback_server() -> Result<&'static str, CacheError> {
    let dir = temp_dir()?;
    let result = quota_in(&dir.path);
    let _ = fs::remove_dir_all(&dir.path);
    result
}

fn quota_in(path: &Path) -> Result<&'static str, CacheError> {
    let payload = encode_synthetic_kv(&TOKENS)?;
    let mut limits = ResourceLimits::dev_defaults();
    let len = payload.len() as u64;
    limits.max_object_bytes = len;
    limits.max_retained_bytes = len;
    limits.max_retained_bytes_per_tenant = len;
    limits.max_in_flight_bytes_per_tenant = len;
    let identity = synthetic_identity(1, 1);
    let policy = TenantPolicy::new(identity.tenant.clone(), identity.trust_domain.clone(), FP_A)?;
    let auth = AuthContext::new(identity.tenant.clone(), identity.trust_domain.clone(), FP_A)?;
    let store = FileStore::open(path, limits, vec![policy])?;
    let server = serve(store, ServerConfig::default())?;
    let mut client = connect(server.addr(), auth)?;
    let generation = put_commit(&mut client, &payload, identity.clone(), 90)?;
    let got = client.get(vec![GetItem {
        header: header(&client, 93)?,
        generation,
        identity: identity.clone(),
        pin_ttl_ms: 30_000,
    }])?;
    let pin = got.items[0]
        .body
        .as_ref()
        .map_err(|_| CacheError::Malformed {
            reason: "fault demo quota",
        })?;
    if decode_synthetic_kv(&pin.payload)? != TOKENS {
        return Err(CacheError::Malformed {
            reason: "fault demo quota",
        });
    }
    let mut second = identity.clone();
    second.prefix_chain[0].0[2] = 8;
    let denied = client.reserve_put(vec![ReservePutItem {
        header: header(&client, 94)?,
        identity: second.clone(),
        generation: 0,
        length: len,
        checksum: content_checksum(&payload),
        payload: Some(payload.clone()),
        lease_ttl_ms: 30_000,
    }])?;
    if !matches!(denied.items[0].body, Err(CacheError::QuotaExceeded { .. })) {
        return Err(CacheError::Malformed {
            reason: "fault demo quota",
        });
    }
    let looked = client.lookup(vec![LookupItem {
        header: header(&client, 95)?,
        identity: second,
    }])?;
    if !matches!(
        looked.items[0].body,
        Ok(LookupOutcome::Miss {
            reason: MissReason::Absent
        })
    ) {
        return Err(CacheError::Malformed {
            reason: "fault demo quota",
        });
    }
    fallback_matches(&[90, 91, 92, 93])?;
    Ok("RECOMPUTE")
}
