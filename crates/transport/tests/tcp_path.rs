// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpucache_core_types::{
    content_checksum, synthetic_identity, CacheError, RequestId, ResourceLimits,
};
use gpucache_protocol::{
    AuthContext, CommitItem, ItemHeader, LookupItem, LookupOutcome, MissReason, ReservePutItem,
    TenantPolicy,
};
use gpucache_storage::FileStore;
use gpucache_transport::{run_share_demo, serve, ServerConfig, TcpClient};

const FP: [u8; 32] = [9u8; 32];

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gpucache-tcp-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn open_store(path: &std::path::Path, limits: ResourceLimits) -> (FileStore, AuthContext) {
    let id = synthetic_identity(1, 1);
    let policy = TenantPolicy::new(id.tenant.clone(), id.trust_domain.clone(), FP).unwrap();
    let auth = AuthContext::new(id.tenant, id.trust_domain, FP).unwrap();
    let store = FileStore::open(path, limits, vec![policy]).unwrap();
    (store, auth)
}

fn header(client: &TcpClient, n: u128) -> ItemHeader {
    ItemHeader {
        request_id: RequestId::from_counter(n).unwrap(),
        epoch: client.epoch(),
        deadline_ms: client.deadline(30_000),
    }
}

fn put(
    client: &TcpClient,
    n: u128,
    bytes: &[u8],
    identity: gpucache_core_types::CacheIdentity,
) -> ReservePutItem {
    ReservePutItem {
        header: header(client, n),
        identity,
        generation: 0,
        length: bytes.len() as u64,
        checksum: content_checksum(bytes),
        payload: Some(bytes.to_vec()),
        lease_ttl_ms: 30_000,
    }
}

#[test]
fn two_instances_share_hit_miss_and_whole_block_prefix() {
    let report = run_share_demo().unwrap();
    assert_eq!(report.exact_hit, "PASS");
    assert_eq!(report.unknown_miss, "PASS");
    assert_eq!(report.partial_prefix, "WHOLE_BLOCK_PREFIX");
    assert_eq!(report.matched_prefix_blocks, 1);
    assert_eq!(report.intra_block_slice, "DEGRADED_MISS");
    assert_eq!(report.layout_mismatch, "MISS");
    assert_eq!(report.cross_tenant, "UNAUTHORIZED");
    assert_eq!(report.bytes_match, "PASS");
    let text = report.to_string();
    assert!(text.starts_with("status=REUSE_OK"));
}

#[test]
fn non_loopback_bind_is_refused_without_an_explicit_flag() {
    let dir = TempDir::new();
    let (store, _) = open_store(&dir.path, ResourceLimits::dev_defaults());
    let cfg = ServerConfig {
        bind: SocketAddr::from(([0, 0, 0, 0], 0)),
        allow_remote: false,
        ..ServerConfig::default()
    };
    match serve(store, cfg) {
        Err(CacheError::Malformed {
            reason: "bind must be loopback",
        }) => {}
        Err(err) => panic!("unexpected error: {err}"),
        Ok(_) => panic!("remote bind must be refused"),
    }
}

#[test]
fn wrong_fingerprint_is_unauthorized_and_checksum_mismatch_hides_bytes() {
    let dir = TempDir::new();
    let (store, auth) = open_store(&dir.path, ResourceLimits::dev_defaults());
    let server = serve(store, ServerConfig::default()).unwrap();
    let mut client =
        TcpClient::connect(server.addr(), auth, Duration::from_secs(5), 1024 * 1024).unwrap();
    let payload = b"checksum-over-tcp".to_vec();
    let identity = synthetic_identity(1, 1);
    let reserved = client
        .reserve_put(vec![put(&client, 1, &payload, identity.clone())])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let mut bad = content_checksum(&payload);
    bad.0[0] ^= 0xff;
    let committed = client
        .commit(vec![CommitItem {
            header: header(&client, 2),
            generation,
            length: payload.len() as u64,
            checksum: bad,
        }])
        .unwrap();
    assert!(matches!(
        committed.items[0].body,
        Err(CacheError::ChecksumMismatch)
    ));
    let looked = client
        .lookup(vec![LookupItem {
            header: header(&client, 3),
            identity: identity.clone(),
        }])
        .unwrap();
    assert!(matches!(
        looked.items[0].body,
        Ok(LookupOutcome::Miss {
            reason: MissReason::Absent
        })
    ));
    let bad_auth = AuthContext::new(
        identity.tenant.clone(),
        identity.trust_domain.clone(),
        [3u8; 32],
    )
    .unwrap();
    let mut intruder =
        TcpClient::connect(server.addr(), bad_auth, Duration::from_secs(5), 1024 * 1024).unwrap();
    let denied = intruder.lookup(vec![LookupItem {
        header: header(&intruder, 4),
        identity,
    }]);
    assert!(matches!(denied, Err(CacheError::Unauthorized)));
}

#[test]
fn restart_and_quota_eviction_survive_the_tcp_path() {
    let dir = TempDir::new();
    let mut limits = ResourceLimits::dev_defaults();
    limits.max_object_bytes = 16;
    limits.max_retained_bytes = 32;
    limits.max_retained_bytes_per_tenant = 32;
    limits.max_in_flight_bytes_per_tenant = 32;
    let payload_a = vec![1u8; 16];
    let payload_b = vec![2u8; 16];
    let payload_c = vec![3u8; 16];
    let mut id_a = synthetic_identity(1, 1);
    id_a.prefix_chain[0].0[0] = 1;
    let mut id_b = synthetic_identity(1, 1);
    id_b.prefix_chain[0].0[0] = 2;
    let mut id_c = synthetic_identity(1, 1);
    id_c.prefix_chain[0].0[0] = 3;
    let auth = {
        let (store, auth) = open_store(&dir.path, limits);
        let server = serve(store, ServerConfig::default()).unwrap();
        let mut client = TcpClient::connect(
            server.addr(),
            auth.clone(),
            Duration::from_secs(5),
            1024 * 1024,
        )
        .unwrap();
        commit_tcp(&mut client, 10, &payload_a, id_a.clone());
        drop(client);
        drop(server);
        auth
    };
    let store = FileStore::open(
        &dir.path,
        limits,
        vec![TenantPolicy::new(auth.tenant.clone(), auth.trust_domain.clone(), FP).unwrap()],
    )
    .unwrap();
    let server = serve(store, ServerConfig::default()).unwrap();
    let mut client =
        TcpClient::connect(server.addr(), auth, Duration::from_secs(5), 1024 * 1024).unwrap();
    let looked = client
        .lookup(vec![
            LookupItem {
                header: header(&client, 30),
                identity: id_a.clone(),
            },
            LookupItem {
                header: header(&client, 31),
                identity: id_b.clone(),
            },
        ])
        .unwrap();
    assert!(matches!(
        looked.items[0].body,
        Ok(LookupOutcome::Hit { .. })
    ));
    assert!(matches!(
        looked.items[1].body,
        Ok(LookupOutcome::Miss { .. })
    ));
    commit_tcp(&mut client, 40, &payload_b, id_b.clone());
    commit_tcp(&mut client, 50, &payload_c, id_c.clone());
    let after = client
        .lookup(vec![LookupItem {
            header: header(&client, 60),
            identity: id_a,
        }])
        .unwrap();
    assert!(matches!(
        after.items[0].body,
        Ok(LookupOutcome::Miss { .. })
    ));
    let kept = client
        .lookup(vec![LookupItem {
            header: header(&client, 61),
            identity: id_c,
        }])
        .unwrap();
    assert!(matches!(kept.items[0].body, Ok(LookupOutcome::Hit { .. })));
}

fn commit_tcp(
    client: &mut TcpClient,
    n: u128,
    bytes: &[u8],
    identity: gpucache_core_types::CacheIdentity,
) {
    let reserved = client
        .reserve_put(vec![put(client, n, bytes, identity)])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    client
        .commit(vec![CommitItem {
            header: header(client, n + 1),
            generation,
            length: bytes.len() as u64,
            checksum: content_checksum(bytes),
        }])
        .unwrap()
        .items[0]
        .body
        .as_ref()
        .unwrap();
}
