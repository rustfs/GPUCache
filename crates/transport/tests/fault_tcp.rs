// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! TCP fault checks: disk full, short write, and eviction racing a reader.
//! Outcomes are exact bytes or a miss/error. Another block's bytes are a failure.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gpucache_core_types::{
    content_checksum, synthetic_identity, CacheError, RequestId, ResourceLimits,
};
use gpucache_protocol::{
    Attestation, AuthContext, CommitItem, GetItem, ItemHeader, LookupItem, LookupOutcome,
    MissReason, ReleaseCancelItem, ReservePutItem, TenantPolicy,
};
use gpucache_storage::{FileStore, PersistFault};
use gpucache_transport::{run_fault_demo, serve, ServerConfig, TcpClient};

const FP: [u8; 32] = [9u8; 32];

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gpucache-fault-tcp-{}-{}",
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

fn open(path: &std::path::Path, limits: ResourceLimits) -> (FileStore, AuthContext) {
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

fn identity_mark(mark: u8) -> gpucache_core_types::CacheIdentity {
    let mut id = synthetic_identity(1, 1);
    id.prefix_chain[0].0[0] = mark;
    id
}

fn reserve_commit(
    client: &mut TcpClient,
    n: u128,
    bytes: &[u8],
    identity: gpucache_core_types::CacheIdentity,
) -> Result<u64, CacheError> {
    let reserved = client.reserve_put(vec![ReservePutItem {
        header: header(client, n),
        identity,
        generation: 0,
        length: bytes.len() as u64,
        checksum: content_checksum(bytes),
        payload: Some(bytes.to_vec()),
        lease_ttl_ms: 30_000,
    }])?;
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let committed = client.commit(vec![CommitItem {
        header: header(client, n + 1),
        generation,
        length: bytes.len() as u64,
        checksum: content_checksum(bytes),
    }])?;
    committed.items[0]
        .body
        .as_ref()
        .map(|_| generation)
        .map_err(|err| err.clone())
}

#[test]
fn fault_demo_requires_exact_tokens_before_the_ok_banner() {
    let report = run_fault_demo().unwrap();
    assert_eq!(report.hit_tokens, 4);
    assert_eq!(report.miss, "MISS_ABSENT");
    assert_eq!(report.corrupt_fallback, "MISS_ABSENT");
    assert_eq!(report.timeout_fallback, "RECOMPUTE");
    assert_eq!(report.cancel_fallback, "MISS_QUARANTINED");
    assert_eq!(report.restart, "HIT_EXACT");
    assert_eq!(report.restart_down, "TRANSPORT_FALLBACK");
    assert_eq!(report.wrong_tenant, "UNAUTHORIZED");
    assert_eq!(report.wrong_model, "MISS_ABSENT");
    assert_eq!(report.wrong_tokenizer, "MISS_ABSENT");
    assert_eq!(report.wrong_adapter, "MISS_ABSENT");
    assert_eq!(report.layout_mismatch, "MISS_INCOMPATIBLE");
    assert_eq!(report.quota_fallback, "RECOMPUTE");
    assert_eq!(report.duplicate, "REPLAY_ONE_BLOCK");
    assert_eq!(report.reorder, "REJECTED");
    let text = report.to_string();
    assert!(text.starts_with("status=FALLBACK_OK"));
    assert!(text.contains("criteria=EXACT_TOKEN_IDS"));
    assert!(text.contains("tolerance=NONE"));
    assert!(!text.contains("TODO"));
    assert!(!text.contains("placeholder"));
}

#[test]
fn disk_full_and_short_write_over_tcp_are_misses() {
    let dir = TempDir::new();
    let (mut store, auth) = open(&dir.path, ResourceLimits::dev_defaults());
    store.inject_persist_fault(PersistFault::DiskFull, 1);
    let server = serve(store, ServerConfig::default()).unwrap();
    let mut client = TcpClient::connect(
        server.addr(),
        auth.clone(),
        Duration::from_secs(5),
        1024 * 1024,
    )
    .unwrap();
    let payload = b"disk-full-block!!".to_vec();
    let identity = identity_mark(1);
    let reserved = client
        .reserve_put(vec![ReservePutItem {
            header: header(&client, 1),
            identity: identity.clone(),
            generation: 0,
            length: payload.len() as u64,
            checksum: content_checksum(&payload),
            payload: Some(payload.clone()),
            lease_ttl_ms: 30_000,
        }])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let failed = client
        .commit(vec![CommitItem {
            header: header(&client, 2),
            generation,
            length: payload.len() as u64,
            checksum: content_checksum(&payload),
        }])
        .unwrap();
    assert!(matches!(
        failed.items[0].body,
        Err(CacheError::Storage { .. })
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
    drop(client);
    drop(server);

    let (mut store, auth) = open(&dir.path, ResourceLimits::dev_defaults());
    store.inject_persist_fault(PersistFault::ShortWrite, 1);
    let server = serve(store, ServerConfig::default()).unwrap();
    let mut client =
        TcpClient::connect(server.addr(), auth, Duration::from_secs(5), 1024 * 1024).unwrap();
    let reserved = client
        .reserve_put(vec![ReservePutItem {
            header: header(&client, 10),
            identity: identity.clone(),
            generation: 0,
            length: payload.len() as u64,
            checksum: content_checksum(&payload),
            payload: Some(payload.clone()),
            lease_ttl_ms: 30_000,
        }])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let failed = client
        .commit(vec![CommitItem {
            header: header(&client, 11),
            generation,
            length: payload.len() as u64,
            checksum: content_checksum(&payload),
        }])
        .unwrap();
    assert!(matches!(
        failed.items[0].body,
        Err(CacheError::Storage { .. })
    ));
    let looked = client
        .lookup(vec![LookupItem {
            header: header(&client, 12),
            identity,
        }])
        .unwrap();
    assert!(matches!(
        looked.items[0].body,
        Ok(LookupOutcome::Miss { .. })
    ));
    let early = client
        .commit(vec![CommitItem {
            header: header(&client, 13),
            generation: 99,
            length: 4,
            checksum: content_checksum(b"abcd"),
        }])
        .unwrap();
    assert!(matches!(
        early.items[0].body,
        Err(CacheError::UnknownGeneration)
    ));
}

#[test]
fn concurrent_readers_never_observe_another_blocks_bytes() {
    let dir = TempDir::new();
    let mut limits = ResourceLimits::dev_defaults();
    limits.max_object_bytes = 16;
    limits.max_retained_bytes = 32;
    limits.max_retained_bytes_per_tenant = 32;
    limits.max_in_flight_bytes_per_tenant = 32;
    let (store, auth) = open(&dir.path, limits);
    let server = serve(store, ServerConfig::default()).unwrap();
    let addr = server.addr();
    let payload_a = vec![1u8; 16];
    let payload_b = vec![2u8; 16];
    let payload_c = vec![3u8; 16];
    let id_a = identity_mark(1);
    let id_b = identity_mark(2);
    let id_c = identity_mark(3);
    {
        let mut client =
            TcpClient::connect(addr, auth.clone(), Duration::from_secs(5), 1024 * 1024).unwrap();
        reserve_commit(&mut client, 10, &payload_a, id_a.clone()).unwrap();
    }
    let failures = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for idx in 0..2u128 {
        let auth = auth.clone();
        let failures = Arc::clone(&failures);
        let id_a = id_a.clone();
        let payload_a = payload_a.clone();
        handles.push(thread::spawn(move || {
            let mut client =
                TcpClient::connect(addr, auth, Duration::from_secs(5), 1024 * 1024).unwrap();
            for step in 0..8u128 {
                let n = 100 + idx * 50 + step * 4;
                let looked = client.lookup(vec![LookupItem {
                    header: header(&client, n),
                    identity: id_a.clone(),
                }]);
                let Ok(looked) = looked else {
                    continue;
                };
                match &looked.items[0].body {
                    Ok(LookupOutcome::Hit { generation, .. }) => {
                        let got = client.get(vec![GetItem {
                            header: header(&client, n + 1),
                            generation: *generation,
                            identity: id_a.clone(),
                            pin_ttl_ms: 200,
                        }]);
                        let Ok(got) = got else {
                            continue;
                        };
                        if let Ok(hit) = &got.items[0].body {
                            if hit.payload != payload_a {
                                failures.lock().unwrap().push(format!("reader {idx}"));
                            }
                            let _ = client.release_cancel(vec![ReleaseCancelItem {
                                header: header(&client, n + 2),
                                generation: *generation,
                                region_id: Some(hit.region_id),
                                pin_owner: Some(RequestId::from_counter(n + 1).unwrap()),
                                attestation: Attestation::Drained,
                            }]);
                        }
                    }
                    Ok(LookupOutcome::Miss { .. }) => {}
                    Ok(LookupOutcome::PrefixHit { .. }) => {
                        failures.lock().unwrap().push(format!("prefix {idx}"));
                    }
                    Err(_) => {}
                }
            }
        }));
    }
    handles.push(thread::spawn({
        let auth = auth.clone();
        let id_b = id_b.clone();
        let id_c = id_c.clone();
        let payload_b = payload_b.clone();
        let payload_c = payload_c.clone();
        move || {
            let mut client =
                TcpClient::connect(addr, auth, Duration::from_secs(5), 1024 * 1024).unwrap();
            let _ = reserve_commit(&mut client, 400, &payload_b, id_b);
            let _ = reserve_commit(&mut client, 410, &payload_c, id_c);
        }
    }));
    for handle in handles {
        handle.join().unwrap();
    }
    assert!(
        failures.lock().unwrap().is_empty(),
        "cross-block bytes: {:?}",
        failures.lock().unwrap()
    );
    let mut client = TcpClient::connect(addr, auth, Duration::from_secs(5), 1024 * 1024).unwrap();
    let looked = client
        .lookup(vec![LookupItem {
            header: header(&client, 500),
            identity: id_c,
        }])
        .unwrap();
    if let Ok(LookupOutcome::Hit { generation, .. }) = &looked.items[0].body {
        let got = client
            .get(vec![GetItem {
                header: header(&client, 501),
                generation: *generation,
                identity: identity_mark(3),
                pin_ttl_ms: 100,
            }])
            .unwrap();
        assert_eq!(got.items[0].body.as_ref().unwrap().payload, payload_c);
    }
}
