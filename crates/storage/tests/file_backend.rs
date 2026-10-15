// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gpucache_core_types::{
    content_checksum, synthetic_identity, CacheError, QuotaKind, RequestId, ResourceLimits,
    TenantId,
};
use gpucache_protocol::{
    AuthContext, CommitItem, GetItem, ItemHeader, LookupItem, LookupOutcome, ReleaseCancelItem,
    ReservePutItem, TenantPolicy,
};
use gpucache_storage::FileStore;

const FP_A: [u8; 32] = [9u8; 32];
const FP_B: [u8; 32] = [8u8; 32];

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gpucache-file-{}-{}",
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

fn limits(retained: u64, per_tenant: u64) -> ResourceLimits {
    let mut limits = ResourceLimits::dev_defaults();
    limits.max_object_bytes = 32;
    limits.max_retained_bytes = retained;
    limits.max_retained_bytes_per_tenant = per_tenant;
    limits.max_in_flight_bytes_per_tenant = retained;
    limits.max_registrations = 32;
    limits.max_in_flight_requests = 32;
    limits
}

fn policy(nonce: u8, fingerprint: [u8; 32]) -> (TenantPolicy, AuthContext) {
    let id = synthetic_identity(nonce, 1);
    let policy =
        TenantPolicy::new(id.tenant.clone(), id.trust_domain.clone(), fingerprint).unwrap();
    let auth = AuthContext::new(id.tenant, id.trust_domain, fingerprint).unwrap();
    (policy, auth)
}

fn header(epoch: u64, n: u128) -> ItemHeader {
    ItemHeader {
        request_id: RequestId::from_counter(n).unwrap(),
        epoch,
        deadline_ms: 1_000,
    }
}

fn put(
    epoch: u64,
    n: u128,
    bytes: &[u8],
    identity: gpucache_core_types::CacheIdentity,
) -> ReservePutItem {
    ReservePutItem {
        header: header(epoch, n),
        identity,
        generation: 0,
        length: bytes.len() as u64,
        checksum: content_checksum(bytes),
        payload: Some(bytes.to_vec()),
        lease_ttl_ms: 5_000,
    }
}

fn commit_block(
    store: &mut FileStore,
    auth: &AuthContext,
    n: u128,
    bytes: &[u8],
    identity: gpucache_core_types::CacheIdentity,
) -> u64 {
    let epoch = store.epoch();
    let reserved = store
        .reserve_put(auth, 0, vec![put(epoch, n, bytes, identity)])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let epoch = store.epoch();
    store
        .commit(
            auth,
            0,
            vec![CommitItem {
                header: header(epoch, n + 1),
                generation,
                length: bytes.len() as u64,
                checksum: content_checksum(bytes),
            }],
        )
        .unwrap();
    generation
}

#[test]
fn refuses_device_and_parent_paths() {
    let (policy, _) = policy(1, FP_A);
    assert!(matches!(
        must_err(FileStore::open(
            "/dev/nvme0n1",
            ResourceLimits::dev_defaults(),
            vec![policy.clone()],
        )),
        CacheError::Malformed {
            reason: "data path"
        }
    ));
    assert!(matches!(
        must_err(FileStore::open(
            "/tmp/gpucache-not-created/../escape",
            ResourceLimits::dev_defaults(),
            vec![policy],
        )),
        CacheError::Malformed {
            reason: "data path"
        }
    ));
}

fn must_err(result: Result<FileStore, CacheError>) -> CacheError {
    match result {
        Err(err) => err,
        Ok(_) => panic!("expected open to fail"),
    }
}

#[test]
fn empty_policy_is_refused() {
    let dir = TempDir::new();
    assert!(matches!(
        must_err(FileStore::open(&dir.path, limits(64, 64), vec![])),
        CacheError::Unauthorized
    ));
}

#[test]
fn restart_reloads_committed_bytes_and_bumps_epoch() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let payload = b"durable-kv".to_vec();
    let epoch;
    {
        let mut store = FileStore::open(&dir.path, limits(64, 64), vec![policy.clone()]).unwrap();
        epoch = store.epoch();
        let generation = commit_block(&mut store, &auth, 10, &payload, synthetic_identity(1, 1));
        let looked = store
            .lookup(
                &auth,
                0,
                vec![LookupItem {
                    header: header(store.epoch(), 30),
                    identity: synthetic_identity(1, 1),
                }],
            )
            .unwrap();
        assert!(matches!(
            looked.items[0].body,
            Ok(LookupOutcome::Hit { .. })
        ));
        assert!(store.committed_path(generation).is_file());
    }
    let mut store = FileStore::open(&dir.path, limits(64, 64), vec![policy]).unwrap();
    assert_eq!(store.epoch(), epoch + 1);
    let looked = store
        .lookup(
            &auth,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 31),
                identity: synthetic_identity(1, 1),
            }],
        )
        .unwrap();
    let Ok(LookupOutcome::Hit {
        generation,
        checksum,
        ..
    }) = looked.items[0].body
    else {
        panic!("restart must hit");
    };
    assert_eq!(checksum, content_checksum(&payload));
    let got = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 32),
                generation,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    let hit = got.items.into_iter().next().unwrap().body.unwrap();
    assert_eq!(hit.payload, payload);
}

#[test]
fn corrupt_file_is_not_a_hit_after_restart() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let payload = b"checksum-on-disk".to_vec();
    let path;
    {
        let mut store = FileStore::open(&dir.path, limits(64, 64), vec![policy.clone()]).unwrap();
        let generation = commit_block(&mut store, &auth, 10, &payload, synthetic_identity(1, 1));
        path = store.committed_path(generation);
    }
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&path, bytes).unwrap();
    let mut store = FileStore::open(&dir.path, limits(64, 64), vec![policy]).unwrap();
    assert!(store.skipped_corrupt() >= 1);
    let looked = store
        .lookup(
            &auth,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 40),
                identity: synthetic_identity(1, 1),
            }],
        )
        .unwrap();
    assert!(matches!(
        looked.items[0].body,
        Ok(LookupOutcome::Miss { .. })
    ));
}

fn block(fill: u8) -> Vec<u8> {
    vec![fill; 16]
}

#[test]
fn lru_evicts_the_oldest_unpinned_block() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let mut store = FileStore::open(&dir.path, limits(32, 32), vec![policy]).unwrap();
    let mut id_a = synthetic_identity(1, 1);
    id_a.prefix_chain[0].0[0] = 1;
    let mut id_b = synthetic_identity(1, 1);
    id_b.prefix_chain[0].0[0] = 2;
    let mut id_c = synthetic_identity(1, 1);
    id_c.prefix_chain[0].0[0] = 3;
    let gen_a = commit_block(&mut store, &auth, 10, &block(1), id_a.clone());
    commit_block(&mut store, &auth, 20, &block(2), id_b.clone());
    let got = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 40),
                generation: gen_a,
                identity: id_a.clone(),
                pin_ttl_ms: 1_000,
            }],
        )
        .unwrap();
    let pin = got.items.into_iter().next().unwrap().body.unwrap();
    store
        .release_cancel(
            &auth,
            0,
            vec![ReleaseCancelItem {
                header: header(store.epoch(), 41),
                generation: gen_a,
                region_id: Some(pin.region_id),
                pin_owner: Some(RequestId::from_counter(40).unwrap()),
                attestation: gpucache_protocol::Attestation::Drained,
            }],
        )
        .unwrap();
    commit_block(&mut store, &auth, 50, &block(3), id_c);
    let miss_b = store
        .lookup(
            &auth,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 60),
                identity: id_b,
            }],
        )
        .unwrap();
    assert!(matches!(
        miss_b.items[0].body,
        Ok(LookupOutcome::Miss { .. })
    ));
    let hit_a = store
        .lookup(
            &auth,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 61),
                identity: id_a,
            }],
        )
        .unwrap();
    assert!(matches!(hit_a.items[0].body, Ok(LookupOutcome::Hit { .. })));
}

#[test]
fn pinned_block_is_not_evicted_to_make_room() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let mut tight = limits(16, 16);
    tight.max_object_bytes = 16;
    let mut store = FileStore::open(&dir.path, tight, vec![policy]).unwrap();
    let mut id_a = synthetic_identity(1, 1);
    id_a.prefix_chain[0].0[0] = 1;
    let gen_a = commit_block(&mut store, &auth, 10, &block(1), id_a.clone());
    let got = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 20),
                generation: gen_a,
                identity: id_a,
                pin_ttl_ms: 1_000,
            }],
        )
        .unwrap();
    assert!(got.items[0].body.is_ok());
    let mut id_b = synthetic_identity(1, 1);
    id_b.prefix_chain[0].0[0] = 2;
    let denied = store
        .reserve_put(&auth, 0, vec![put(store.epoch(), 30, &block(2), id_b)])
        .unwrap();
    assert!(matches!(
        denied.items[0].body,
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::RetainedBytes
        })
    ));
}

#[test]
fn tenant_quota_does_not_evict_another_tenant() {
    let dir = TempDir::new();
    let (policy_a, auth_a) = policy(1, FP_A);
    let id_b = {
        let mut id = synthetic_identity(2, 1);
        id.tenant = TenantId::new(b"tenant-b".to_vec()).unwrap();
        id
    };
    let policy_b = TenantPolicy::new(id_b.tenant.clone(), id_b.trust_domain.clone(), FP_B).unwrap();
    let auth_b = AuthContext::new(id_b.tenant.clone(), id_b.trust_domain.clone(), FP_B).unwrap();
    let mut store = FileStore::open(&dir.path, limits(64, 16), vec![policy_a, policy_b]).unwrap();
    let bytes_b = block(9);
    let bytes_a1 = block(1);
    let bytes_a2 = block(2);
    let mut id_b1 = id_b.clone();
    id_b1.prefix_chain[0].0[0] = 9;
    commit_block(&mut store, &auth_b, 10, &bytes_b, id_b1.clone());
    let mut id_a1 = synthetic_identity(1, 1);
    id_a1.prefix_chain[0].0[0] = 1;
    commit_block(&mut store, &auth_a, 20, &bytes_a1, id_a1.clone());
    let mut id_a2 = synthetic_identity(1, 1);
    id_a2.prefix_chain[0].0[0] = 2;
    commit_block(&mut store, &auth_a, 30, &bytes_a2, id_a2.clone());
    let still_b = store
        .lookup(
            &auth_b,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 40),
                identity: id_b1,
            }],
        )
        .unwrap();
    assert!(matches!(
        still_b.items[0].body,
        Ok(LookupOutcome::Hit { .. })
    ));
    let gone_a = store
        .lookup(
            &auth_a,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 41),
                identity: id_a1,
            }],
        )
        .unwrap();
    assert!(matches!(
        gone_a.items[0].body,
        Ok(LookupOutcome::Miss { .. })
    ));
    let hit_a2 = store
        .lookup(
            &auth_a,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 42),
                identity: id_a2,
            }],
        )
        .unwrap();
    assert!(matches!(
        hit_a2.items[0].body,
        Ok(LookupOutcome::Hit { .. })
    ));
}

#[test]
fn rejected_reservations_do_not_evict_committed_data() {
    for case in 0..7 {
        let dir = TempDir::new();
        let (policy, auth) = policy(1, FP_A);
        let mut tight = limits(32, 16);
        tight.max_object_bytes = 16;
        let mut store = FileStore::open(&dir.path, tight, vec![policy]).unwrap();
        let identity = synthetic_identity(1, 1);
        let generation = commit_block(&mut store, &auth, 10, &block(1), identity.clone());
        let mut other = identity.clone();
        other.prefix_chain[0].0[0] ^= 0xff;
        let mut request = put(store.epoch(), 20, &block(2), other);
        let mut caller = auth.clone();
        match case {
            0 => caller.credential_fingerprint = FP_B,
            1 => request.header.epoch += 1,
            2 => request.header.deadline_ms = 1,
            3 => request.length = 17,
            4 => request.payload = Some(vec![2; 15]),
            5 => request.identity.tenant = TenantId::new(b"other".to_vec()).unwrap(),
            6 => request.identity = identity.clone(),
            _ => unreachable!(),
        }
        let _ = store.reserve_put(&caller, 2, vec![request]);
        assert!(store.committed_path(generation).exists(), "case {case}");
        let reply = store
            .lookup(
                &auth,
                2,
                vec![LookupItem {
                    header: header(store.epoch(), 30),
                    identity,
                }],
            )
            .unwrap();
        assert!(
            matches!(reply.items[0].body, Ok(LookupOutcome::Hit { .. })),
            "case {case}"
        );
    }
}

#[test]
fn replayed_reservation_does_not_evict_its_own_commit() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let mut store = FileStore::open(&dir.path, limits(32, 16), vec![policy]).unwrap();
    let identity = synthetic_identity(1, 1);
    let generation = commit_block(&mut store, &auth, 10, &block(1), identity.clone());
    let reply = store
        .reserve_put(&auth, 0, vec![put(store.epoch(), 10, &block(1), identity)])
        .unwrap();
    assert_eq!(reply.items[0].body.as_ref().unwrap().generation, generation);
    assert!(store.committed_path(generation).exists());
}

#[test]
fn duplicate_reservation_batch_is_rejected_before_mutation() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let mut store = FileStore::open(&dir.path, limits(64, 64), vec![policy]).unwrap();
    let request = put(store.epoch(), 10, &block(1), synthetic_identity(1, 1));
    assert!(matches!(
        store.reserve_put(&auth, 0, vec![request.clone(), request]),
        Err(CacheError::Malformed {
            reason: "duplicate request_id"
        })
    ));
    assert_eq!(store.stats().retained_bytes, 0);
}

#[test]
fn lookup_and_get_keep_recency_in_memory_without_writing_lru() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let mut store = FileStore::open(&dir.path, limits(64, 64), vec![policy]).unwrap();
    let identity = synthetic_identity(1, 1);
    let generation = commit_block(&mut store, &auth, 10, &block(1), identity.clone());
    let sentinel = b"do not rewrite advisory recency on reads";
    fs::write(dir.path.join("LRU"), sentinel).unwrap();
    store
        .lookup(
            &auth,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 20),
                identity: identity.clone(),
            }],
        )
        .unwrap();
    store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 21),
                generation,
                identity,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(fs::read(dir.path.join("LRU")).unwrap(), sentinel);
}
