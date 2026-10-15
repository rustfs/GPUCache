// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! V1 C fault matrix for the file and memory stores.
//!
//! These tests use synthetic token blocks. A hit must decode to the same token
//! ids that were stored. A wrong tenant, model, tokenizer, adapter, or layout
//! must not return those ids.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gpucache_core_types::{
    content_checksum, synthetic_identity, AdapterIdentity, Attestation, BlockState, CacheError,
    Digest, QuotaKind, RequestId, ResourceLimits, TenantId,
};
use gpucache_protocol::{
    AuthContext, CommitItem, GetItem, ItemHeader, LookupItem, LookupOutcome, MissReason,
    ReleaseCancelItem, ReservePutItem, TenantPolicy,
};
use gpucache_storage::{
    decode_synthetic_kv, encode_synthetic_kv, FileStore, MemoryStore, PersistFault,
};

const FP_A: [u8; 32] = [9u8; 32];
const FP_B: [u8; 32] = [8u8; 32];
const TOKENS_A: [u32; 4] = [11, 22, 33, 44];
const TOKENS_B: [u32; 4] = [90, 91, 92, 93];

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gpucache-fault-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        let path = fs::canonicalize(&path).unwrap();
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
    limits.max_object_bytes = 256;
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
    let committed = store
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
    assert!(committed.items[0].body.is_ok());
    generation
}

fn lookup(
    store: &mut FileStore,
    auth: &AuthContext,
    n: u128,
    identity: gpucache_core_types::CacheIdentity,
) -> LookupOutcome {
    let looked = store
        .lookup(
            auth,
            0,
            vec![LookupItem {
                header: header(store.epoch(), n),
                identity,
            }],
        )
        .unwrap();
    looked.items.into_iter().next().unwrap().body.unwrap()
}

fn assert_miss(outcome: LookupOutcome) {
    assert!(
        matches!(outcome, LookupOutcome::Miss { .. }),
        "expected miss, got {outcome:?}"
    );
}

fn payload_of(tokens: &[u32]) -> Vec<u8> {
    encode_synthetic_kv(tokens).unwrap()
}

#[test]
fn short_write_is_not_durable_and_replay_is_not_success() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let bytes = payload_of(&TOKENS_A);
    let mut store = FileStore::open(&dir.path, limits(4096, 4096), vec![policy]).unwrap();
    store.inject_persist_fault(PersistFault::ShortWrite, 1);
    let identity = synthetic_identity(1, 1);
    let epoch = store.epoch();
    let reserved = store
        .reserve_put(&auth, 0, vec![put(epoch, 10, &bytes, identity.clone())])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let commit_item = CommitItem {
        header: header(epoch, 11),
        generation,
        length: bytes.len() as u64,
        checksum: content_checksum(&bytes),
    };
    let failed = store.commit(&auth, 0, vec![commit_item.clone()]).unwrap();
    assert_eq!(
        failed.items[0].body,
        Err(CacheError::Storage {
            reason: "short write",
        })
    );
    assert!(!store.committed_path(generation).is_file());
    assert_miss(lookup(&mut store, &auth, 12, identity.clone()));
    let replay = store.commit(&auth, 0, vec![commit_item]).unwrap();
    assert!(
        replay.items[0].body.is_err(),
        "replay of a failed short write must not report success"
    );
    assert_eq!(store.stats().committed, 0);
    let generation = commit_block(&mut store, &auth, 20, &bytes, identity.clone());
    let got = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 30),
                generation,
                identity,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    let hit = got.items[0].body.as_ref().unwrap();
    assert_eq!(decode_synthetic_kv(&hit.payload).unwrap(), TOKENS_A);
}

#[test]
fn disk_full_abandon_leaves_a_miss() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let bytes = payload_of(&TOKENS_A);
    let mut store = FileStore::open(&dir.path, limits(4096, 4096), vec![policy]).unwrap();
    store.inject_persist_fault(PersistFault::DiskFull, 1);
    let identity = synthetic_identity(1, 1);
    let epoch = store.epoch();
    let reserved = store
        .reserve_put(&auth, 0, vec![put(epoch, 10, &bytes, identity.clone())])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let failed = store
        .commit(
            &auth,
            0,
            vec![CommitItem {
                header: header(epoch, 11),
                generation,
                length: bytes.len() as u64,
                checksum: content_checksum(&bytes),
            }],
        )
        .unwrap();
    assert_eq!(
        failed.items[0].body,
        Err(CacheError::Storage {
            reason: "disk full",
        })
    );
    assert_miss(lookup(&mut store, &auth, 12, identity));
    assert_eq!(store.stats().committed, 0);
    assert_eq!(store.stats().retained_bytes, 0);
}

#[test]
fn truncated_block_file_is_a_miss_after_restart() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let bytes = payload_of(&TOKENS_A);
    let path;
    {
        let mut store =
            FileStore::open(&dir.path, limits(4096, 4096), vec![policy.clone()]).unwrap();
        let generation = commit_block(&mut store, &auth, 10, &bytes, synthetic_identity(1, 1));
        path = store.committed_path(generation);
    }
    let original = fs::read(&path).unwrap();
    fs::write(&path, &original[..8]).unwrap();
    let mut store = FileStore::open(&dir.path, limits(4096, 4096), vec![policy]).unwrap();
    assert!(store.skipped_corrupt() >= 1);
    assert_miss(lookup(&mut store, &auth, 20, synthetic_identity(1, 1)));
}

#[cfg(unix)]
#[test]
fn unwritable_blocks_directory_does_not_leave_a_hit() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let bytes = payload_of(&TOKENS_A);
    let mut store = FileStore::open(&dir.path, limits(4096, 4096), vec![policy]).unwrap();
    let blocks = dir.path.join("blocks");
    let restore = blocks.clone();
    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    let _guard = Guard(restore);
    fs::set_permissions(&blocks, fs::Permissions::from_mode(0o555)).unwrap();
    let identity = synthetic_identity(1, 1);
    let epoch = store.epoch();
    let reserved = store
        .reserve_put(&auth, 0, vec![put(epoch, 10, &bytes, identity.clone())])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let failed = store
        .commit(
            &auth,
            0,
            vec![CommitItem {
                header: header(epoch, 11),
                generation,
                length: bytes.len() as u64,
                checksum: content_checksum(&bytes),
            }],
        )
        .unwrap();
    assert!(matches!(
        failed.items[0].body,
        Err(CacheError::Storage { .. })
    ));
    fs::set_permissions(&blocks, fs::Permissions::from_mode(0o755)).unwrap();
    assert_miss(lookup(&mut store, &auth, 12, identity));
    assert_eq!(store.stats().committed, 0);
}

#[test]
fn duplicate_commit_replays_one_block_and_conflict_hides_bytes() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let bytes = payload_of(&TOKENS_A);
    let mut store = FileStore::open(&dir.path, limits(4096, 4096), vec![policy]).unwrap();
    let identity = synthetic_identity(1, 1);
    let epoch = store.epoch();
    let reserved = store
        .reserve_put(&auth, 0, vec![put(epoch, 10, &bytes, identity.clone())])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let item = CommitItem {
        header: header(epoch, 11),
        generation,
        length: bytes.len() as u64,
        checksum: content_checksum(&bytes),
    };
    let first = store.commit(&auth, 0, vec![item.clone()]).unwrap();
    let second = store.commit(&auth, 0, vec![item]).unwrap();
    assert_eq!(first.items[0].body, second.items[0].body);
    assert_eq!(store.stats().committed, 1);
    let mut other = bytes.clone();
    other[10] ^= 0xff;
    let conflict = store
        .reserve_put(
            &auth,
            0,
            vec![ReservePutItem {
                header: header(epoch, 10),
                identity: identity.clone(),
                generation: 0,
                length: other.len() as u64,
                checksum: content_checksum(&other),
                payload: Some(other),
                lease_ttl_ms: 5_000,
            }],
        )
        .unwrap();
    assert!(matches!(
        conflict.items[0].body,
        Err(CacheError::IdempotencyConflict)
    ));
    let got = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(epoch, 30),
                generation,
                identity,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(
        decode_synthetic_kv(&got.items[0].body.as_ref().unwrap().payload).unwrap(),
        TOKENS_A
    );
}

#[test]
fn commit_before_reserve_and_duplicate_batch_ids_fail_closed() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let mut store = FileStore::open(&dir.path, limits(4096, 4096), vec![policy]).unwrap();
    let epoch = store.epoch();
    let early = store
        .commit(
            &auth,
            0,
            vec![CommitItem {
                header: header(epoch, 1),
                generation: 99,
                length: 4,
                checksum: content_checksum(&[1, 2, 3, 4]),
            }],
        )
        .unwrap();
    assert!(matches!(
        early.items[0].body,
        Err(CacheError::UnknownGeneration)
    ));
    let identity = synthetic_identity(1, 1);
    let duplicate = store.lookup(
        &auth,
        0,
        vec![
            LookupItem {
                header: header(epoch, 2),
                identity: identity.clone(),
            },
            LookupItem {
                header: header(epoch, 2),
                identity,
            },
        ],
    );
    assert!(matches!(
        duplicate,
        Err(CacheError::Malformed {
            reason: "duplicate request_id"
        })
    ));
    assert_eq!(store.stats().committed, 0);
}

#[test]
fn timeout_and_cancel_do_not_return_payload() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let bytes = payload_of(&TOKENS_A);
    let mut store = FileStore::open(&dir.path, limits(4096, 4096), vec![policy]).unwrap();
    let identity = synthetic_identity(1, 1);
    let epoch = store.epoch();
    let reserved = store
        .reserve_put(&auth, 0, vec![put(epoch, 10, &bytes, identity.clone())])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let timed = store
        .commit(
            &auth,
            50,
            vec![CommitItem {
                header: ItemHeader {
                    request_id: RequestId::from_counter(11).unwrap(),
                    epoch,
                    deadline_ms: 10,
                },
                generation,
                length: bytes.len() as u64,
                checksum: content_checksum(&bytes),
            }],
        )
        .unwrap();
    assert_eq!(timed.items[0].body, Err(CacheError::TimedOut));
    let cancelled = store
        .release_cancel(
            &auth,
            0,
            vec![ReleaseCancelItem {
                header: header(epoch, 12),
                generation,
                region_id: None,
                pin_owner: None,
                attestation: Attestation::Unknown,
            }],
        )
        .unwrap();
    assert_eq!(
        cancelled.items[0].body.as_ref().unwrap().state,
        BlockState::Quarantined
    );
    assert_eq!(
        lookup(&mut store, &auth, 13, identity.clone()),
        LookupOutcome::Miss {
            reason: MissReason::Quarantined
        }
    );
    let got = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(epoch, 14),
                generation,
                identity,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(got.items[0].body, Err(CacheError::Cancelled));
}

#[test]
fn pinned_read_keeps_exact_tokens_while_eviction_is_refused() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let bytes_a = payload_of(&TOKENS_A);
    let bytes_b = payload_of(&TOKENS_B);
    let mut tight = limits(bytes_a.len() as u64, bytes_a.len() as u64);
    tight.max_object_bytes = bytes_a.len() as u64;
    let mut store = FileStore::open(&dir.path, tight, vec![policy]).unwrap();
    let mut id_a = synthetic_identity(1, 1);
    id_a.prefix_chain[0].0[0] = 1;
    let mut id_b = synthetic_identity(1, 1);
    id_b.prefix_chain[0].0[0] = 2;
    let generation = commit_block(&mut store, &auth, 10, &bytes_a, id_a.clone());
    let got = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 20),
                generation,
                identity: id_a.clone(),
                pin_ttl_ms: 1_000,
            }],
        )
        .unwrap();
    let pin = got.items[0].body.as_ref().unwrap();
    assert_eq!(decode_synthetic_kv(&pin.payload).unwrap(), TOKENS_A);
    let denied = store
        .reserve_put(&auth, 0, vec![put(store.epoch(), 30, &bytes_b, id_b)])
        .unwrap();
    assert!(matches!(
        denied.items[0].body,
        Err(CacheError::QuotaExceeded { .. })
    ));
    let again = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 21),
                generation,
                identity: id_a.clone(),
                pin_ttl_ms: 1_000,
            }],
        )
        .unwrap();
    assert_eq!(
        decode_synthetic_kv(&again.items[0].body.as_ref().unwrap().payload).unwrap(),
        TOKENS_A
    );
    store
        .release_cancel(
            &auth,
            0,
            vec![
                ReleaseCancelItem {
                    header: header(store.epoch(), 22),
                    generation,
                    region_id: Some(pin.region_id),
                    pin_owner: Some(RequestId::from_counter(20).unwrap()),
                    attestation: Attestation::Drained,
                },
                ReleaseCancelItem {
                    header: header(store.epoch(), 23),
                    generation,
                    region_id: Some(again.items[0].body.as_ref().unwrap().region_id),
                    pin_owner: Some(RequestId::from_counter(21).unwrap()),
                    attestation: Attestation::Drained,
                },
            ],
        )
        .unwrap();
}

#[test]
fn tenant_key_collision_never_returns_the_other_payload() {
    let dir = TempDir::new();
    let (policy_a, auth_a) = policy(1, FP_A);
    let mut id_b = synthetic_identity(1, 1);
    id_b.tenant = TenantId::new(b"tenant-b".to_vec()).unwrap();
    let policy_b = TenantPolicy::new(id_b.tenant.clone(), id_b.trust_domain.clone(), FP_B).unwrap();
    let auth_b = AuthContext::new(id_b.tenant.clone(), id_b.trust_domain.clone(), FP_B).unwrap();
    let mut store =
        FileStore::open(&dir.path, limits(4096, 4096), vec![policy_a, policy_b]).unwrap();
    let mut id_a = synthetic_identity(1, 1);
    id_a.prefix_chain[0].0[1] = 7;
    id_b.prefix_chain[0].0[1] = 7;
    let bytes_a = payload_of(&TOKENS_A);
    let bytes_b = payload_of(&TOKENS_B);
    let gen_a = commit_block(&mut store, &auth_a, 10, &bytes_a, id_a.clone());
    let gen_b = commit_block(&mut store, &auth_b, 20, &bytes_b, id_b.clone());
    assert_ne!(gen_a, gen_b);
    let got_a = store
        .get(
            &auth_a,
            0,
            vec![GetItem {
                header: header(store.epoch(), 30),
                generation: gen_a,
                identity: id_a.clone(),
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(
        decode_synthetic_kv(&got_a.items[0].body.as_ref().unwrap().payload).unwrap(),
        TOKENS_A
    );
    let foreign = store
        .lookup(
            &auth_a,
            0,
            vec![LookupItem {
                header: header(store.epoch(), 31),
                identity: id_b.clone(),
            }],
        )
        .unwrap();
    assert_eq!(foreign.items[0].body, Err(CacheError::Unauthorized));
    let foreign_get = store
        .get(
            &auth_a,
            0,
            vec![GetItem {
                header: header(store.epoch(), 33),
                generation: gen_b,
                identity: id_b.clone(),
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(foreign_get.items[0].body, Err(CacheError::Unauthorized));
    let got_b = store
        .get(
            &auth_b,
            0,
            vec![GetItem {
                header: header(store.epoch(), 32),
                generation: gen_b,
                identity: id_b,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(
        decode_synthetic_kv(&got_b.items[0].body.as_ref().unwrap().payload).unwrap(),
        TOKENS_B
    );
}

#[test]
fn model_tokenizer_adapter_and_layout_changes_are_not_hits() {
    let dir = TempDir::new();
    let (policy, auth) = policy(1, FP_A);
    let bytes = payload_of(&TOKENS_A);
    let mut store = FileStore::open(&dir.path, limits(4096, 4096), vec![policy]).unwrap();
    let identity = synthetic_identity(1, 1);
    let generation = commit_block(&mut store, &auth, 10, &bytes, identity.clone());
    let mut model = identity.clone();
    model.model_revision.0[4] ^= 0x5a;
    let mut config = identity.clone();
    config.model_config_hash.0[4] ^= 0x5a;
    let mut tokenizer = identity.clone();
    tokenizer.tokenizer_revision.0[4] ^= 0x5a;
    let mut adapter = identity.clone();
    adapter.adapter = AdapterIdentity::Lora {
        revision: Digest([9u8; 32]),
    };
    let mut layout = identity.clone();
    layout.layout.family = 2;
    assert_eq!(
        lookup(&mut store, &auth, 20, model.clone()),
        LookupOutcome::Miss {
            reason: MissReason::Absent
        }
    );
    assert_eq!(
        lookup(&mut store, &auth, 21, config),
        LookupOutcome::Miss {
            reason: MissReason::Absent
        }
    );
    assert_eq!(
        lookup(&mut store, &auth, 22, tokenizer),
        LookupOutcome::Miss {
            reason: MissReason::Absent
        }
    );
    assert_eq!(
        lookup(&mut store, &auth, 23, adapter),
        LookupOutcome::Miss {
            reason: MissReason::Absent
        }
    );
    assert_eq!(
        lookup(&mut store, &auth, 24, layout.clone()),
        LookupOutcome::Miss {
            reason: MissReason::Incompatible
        }
    );
    let wrong_get = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 25),
                generation,
                identity: model,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(wrong_get.items[0].body, Err(CacheError::Incompatible));
    let layout_get = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 26),
                generation,
                identity: layout,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(layout_get.items[0].body, Err(CacheError::Incompatible));
    let exact = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(store.epoch(), 27),
                generation,
                identity,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(
        decode_synthetic_kv(&exact.items[0].body.as_ref().unwrap().payload).unwrap(),
        TOKENS_A
    );
}

#[test]
fn memory_quota_exhaustion_does_not_evict_or_return_foreign_bytes() {
    let bytes = payload_of(&TOKENS_A);
    let mut limits = ResourceLimits::dev_defaults();
    limits.max_object_bytes = bytes.len() as u64;
    limits.max_retained_bytes = bytes.len() as u64;
    limits.max_retained_bytes_per_tenant = bytes.len() as u64;
    limits.max_in_flight_bytes_per_tenant = bytes.len() as u64;
    limits.max_registrations = 1;
    let identity = synthetic_identity(1, 1);
    let policy =
        TenantPolicy::new(identity.tenant.clone(), identity.trust_domain.clone(), FP_A).unwrap();
    let auth =
        AuthContext::new(identity.tenant.clone(), identity.trust_domain.clone(), FP_A).unwrap();
    let mut store = MemoryStore::new(limits, vec![policy]).unwrap();
    let epoch = store.epoch();
    let reserved = store
        .reserve_put(&auth, 0, vec![put(epoch, 10, &bytes, identity.clone())])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    store
        .commit(
            &auth,
            0,
            vec![CommitItem {
                header: header(epoch, 11),
                generation,
                length: bytes.len() as u64,
                checksum: content_checksum(&bytes),
            }],
        )
        .unwrap()
        .items[0]
        .body
        .as_ref()
        .unwrap();
    let mut other = identity.clone();
    other.prefix_chain[0].0[2] = 9;
    let denied = store
        .reserve_put(&auth, 0, vec![put(epoch, 20, &bytes, other)])
        .unwrap();
    assert!(matches!(
        denied.items[0].body,
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::RetainedBytes | QuotaKind::Registrations
        })
    ));
    assert_eq!(store.stats().committed, 1);
    assert_eq!(store.stats().retained_bytes, bytes.len() as u64);
    let got = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: header(epoch, 30),
                generation,
                identity,
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    assert_eq!(
        decode_synthetic_kv(&got.items[0].body.as_ref().unwrap().payload).unwrap(),
        TOKENS_A
    );
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(12))]
    #[test]
    fn flipped_model_byte_is_never_a_hit(index in 0usize..32) {
        let bytes = payload_of(&TOKENS_A);
        let mut limits = ResourceLimits::dev_defaults();
        limits.max_object_bytes = 256;
        limits.max_retained_bytes = 4096;
        limits.max_retained_bytes_per_tenant = 4096;
        limits.max_in_flight_bytes_per_tenant = 4096;
        let identity = synthetic_identity(1, 1);
        let policy = TenantPolicy::new(identity.tenant.clone(), identity.trust_domain.clone(), FP_A).unwrap();
        let auth = AuthContext::new(identity.tenant.clone(), identity.trust_domain.clone(), FP_A).unwrap();
        let mut store = MemoryStore::new(limits, vec![policy]).unwrap();
        let epoch = store.epoch();
        let reserved = store.reserve_put(&auth, 0, vec![put(epoch, 10, &bytes, identity.clone())]).unwrap();
        let generation = reserved.items[0].body.as_ref().unwrap().generation;
        store.commit(&auth, 0, vec![CommitItem {
            header: header(epoch, 11),
            generation,
            length: bytes.len() as u64,
            checksum: content_checksum(&bytes),
        }]).unwrap().items[0].body.as_ref().unwrap();
        let mut wrong = identity.clone();
        wrong.model_revision.0[index] ^= 0x5a;
        let looked = store.lookup(&auth, 0, vec![LookupItem {
            header: header(epoch, 12),
            identity: wrong,
        }]).unwrap();
        assert!(matches!(looked.items[0].body, Ok(LookupOutcome::Miss { .. })));
    }
}
