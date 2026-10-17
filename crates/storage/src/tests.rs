// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{
    content_checksum, synthetic_identity, Attestation, BlockState, CacheError, RequestId,
    ResourceLimits, TenantId, TrustDomain,
};
use gpucache_protocol::{
    encode_export, AuthContext, CommitItem, CommittedExport, GetItem, InvalidateItem, ItemHeader,
    LookupItem, LookupOutcome, ReleaseCancelItem, ReservePutItem, TenantPolicy,
};

use crate::import_committed_block;
use crate::{MemoryStore, ReclaimRequest};

const FP: [u8; 32] = [9u8; 32];

#[test]
fn direct_policy_construction_cannot_enable_a_zero_credential() {
    let identity = synthetic_identity(1, 1);
    let policy = TenantPolicy {
        tenant: identity.tenant,
        trust_domain: identity.trust_domain,
        credential_fingerprint: [0; 32],
    };
    assert!(matches!(
        MemoryStore::new(limits(), vec![policy]),
        Err(CacheError::Unauthorized)
    ));
}

#[test]
fn mutated_auth_context_is_rejected_at_the_store_boundary() {
    let (mut store, mut auth) = open(None);
    auth.credential_fingerprint = [0; 32];
    let reply = store.lookup(
        &auth,
        0,
        vec![LookupItem {
            header: hdr(&store, 1, 0),
            identity: synthetic_identity(1, 1),
        }],
    );
    assert_eq!(reply, Err(CacheError::Unauthorized));
    assert_eq!(store.stats().retained_bytes, 0);
}

fn limits() -> ResourceLimits {
    ResourceLimits::dev_defaults()
}

fn open(extra: Option<TenantPolicy>) -> (MemoryStore, AuthContext) {
    let id = synthetic_identity(1, 1);
    let policy = TenantPolicy::new(id.tenant.clone(), id.trust_domain.clone(), FP).unwrap();
    let auth = AuthContext::new(id.tenant, id.trust_domain, FP).unwrap();
    let mut policies = vec![policy];
    if let Some(extra) = extra {
        policies.push(extra);
    }
    let store = MemoryStore::new(limits(), policies).unwrap();
    (store, auth)
}

fn hdr(store: &MemoryStore, n: u128, now: u64) -> ItemHeader {
    ItemHeader {
        request_id: RequestId::from_counter(n).unwrap(),
        epoch: store.epoch(),
        deadline_ms: now + 1_000,
    }
}

fn put_item(store: &MemoryStore, n: u128, now: u64, bytes: &[u8], family: u16) -> ReservePutItem {
    ReservePutItem {
        header: hdr(store, n, now),
        identity: synthetic_identity(1, family),
        generation: 0,
        length: bytes.len() as u64,
        checksum: content_checksum(bytes),
        payload: Some(bytes.to_vec()),
        lease_ttl_ms: 500,
    }
}

fn commit_bytes(store: &mut MemoryStore, auth: &AuthContext, n: u128, bytes: &[u8]) -> u64 {
    let now = 0;
    let reserved = store
        .reserve_put(auth, now, vec![put_item(store, n, now, bytes, 1)])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let committed = store
        .commit(
            auth,
            now,
            vec![CommitItem {
                header: hdr(store, n + 1, now),
                generation,
                length: bytes.len() as u64,
                checksum: content_checksum(bytes),
            }],
        )
        .unwrap();
    committed.items[0].body.as_ref().unwrap().generation
}

#[test]
fn single_process_export_import_roundtrip() {
    let (mut store, auth) = open(None);
    let payload = b"synthetic-kv-block-not-vllm".to_vec();
    let generation = commit_bytes(&mut store, &auth, 10, &payload);
    let hit = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: hdr(&store, 30, 0),
                generation,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 100,
            }],
        )
        .unwrap();
    let got = hit.items[0].body.as_ref().unwrap();
    assert_eq!(got.payload, payload);
    let frame = CommittedExport {
        identity: synthetic_identity(1, 1),
        source_generation: generation,
        checksum: content_checksum(&payload),
        payload: payload.clone(),
    };
    let encoded = encode_export(&frame).unwrap();
    let decoded = gpucache_protocol::decode_export(&encoded).unwrap();
    let report = import_committed_block(&decoded, FP, 0).unwrap();
    assert_eq!(report.length, payload.len() as u64);
    assert_eq!(report.checksum_hex, hex(&content_checksum(&payload).0));
    assert_eq!(report.source_generation, generation);
    let _ = store.release_cancel(
        &auth,
        0,
        vec![ReleaseCancelItem {
            header: hdr(&store, 31, 0),
            generation,
            region_id: Some(got.region_id),
            pin_owner: Some(RequestId::from_counter(30).unwrap()),
            attestation: Attestation::Drained,
        }],
    );
}

#[test]
fn only_committed_compatible_blocks_hit() {
    let (mut store, auth) = open(None);
    let payload = b"visible-only-after-commit".to_vec();
    let now = 0;
    let missing = store
        .lookup(
            &auth,
            now,
            vec![LookupItem {
                header: hdr(&store, 1, now),
                identity: synthetic_identity(1, 1),
            }],
        )
        .unwrap();
    assert!(matches!(
        missing.items[0].body.as_ref().unwrap(),
        LookupOutcome::Miss { .. }
    ));
    let reserved = store
        .reserve_put(&auth, now, vec![put_item(&store, 2, now, &payload, 1)])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let hidden = store
        .lookup(
            &auth,
            now,
            vec![LookupItem {
                header: hdr(&store, 3, now),
                identity: synthetic_identity(1, 1),
            }],
        )
        .unwrap();
    assert!(matches!(
        hidden.items[0].body.as_ref().unwrap(),
        LookupOutcome::Miss {
            reason: gpucache_protocol::MissReason::NotCommitted
        }
    ));
    let premature = store
        .get(
            &auth,
            now,
            vec![GetItem {
                header: hdr(&store, 4, now),
                generation,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 10,
            }],
        )
        .unwrap();
    assert!(matches!(
        premature.items[0].body,
        Err(CacheError::NotExposed {
            state: BlockState::Writing
        })
    ));
    store
        .commit(
            &auth,
            now,
            vec![CommitItem {
                header: hdr(&store, 5, now),
                generation,
                length: payload.len() as u64,
                checksum: content_checksum(&payload),
            }],
        )
        .unwrap();
    let found = store
        .lookup(
            &auth,
            now,
            vec![LookupItem {
                header: hdr(&store, 6, now),
                identity: synthetic_identity(1, 1),
            }],
        )
        .unwrap();
    assert!(matches!(
        found.items[0].body.as_ref().unwrap(),
        LookupOutcome::Hit { .. }
    ));
    let mut other = synthetic_identity(1, 1);
    other.layout.family = 9;
    let incompatible = store
        .lookup(
            &auth,
            now,
            vec![LookupItem {
                header: hdr(&store, 7, now),
                identity: other.clone(),
            }],
        )
        .unwrap();
    match incompatible.items[0].body.clone().unwrap() {
        LookupOutcome::Miss {
            reason: gpucache_protocol::MissReason::Incompatible,
        } => {}
        other => panic!("layout mismatch must miss, got {other:?}"),
    }
    let wrong_get = store
        .get(
            &auth,
            now,
            vec![GetItem {
                header: hdr(&store, 8, now),
                generation,
                identity: other,
                pin_ttl_ms: 10,
            }],
        )
        .unwrap();
    assert!(matches!(
        wrong_get.items[0].body,
        Err(CacheError::Incompatible)
    ));
}

#[test]
fn checksum_and_generation_failures_do_not_publish_bytes() {
    let (mut store, auth) = open(None);
    let payload = b"checksum-me".to_vec();
    let now = 0;
    let reserved = store
        .reserve_put(&auth, now, vec![put_item(&store, 1, now, &payload, 1)])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    let bad = store
        .commit(
            &auth,
            now,
            vec![CommitItem {
                header: hdr(&store, 2, now),
                generation,
                length: payload.len() as u64,
                checksum: content_checksum(b"other"),
            }],
        )
        .unwrap();
    assert_eq!(bad.items[0].body, Err(CacheError::ChecksumMismatch));
    assert_eq!(
        store.state_of(&auth, generation, now),
        Err(CacheError::UnknownGeneration)
    );
    assert_eq!(store.stats().committed, 0);
    let generation = commit_bytes(&mut store, &auth, 20, &payload);
    let wrong_gen = store
        .get(
            &auth,
            now,
            vec![GetItem {
                header: hdr(&store, 40, now),
                generation: generation + 50,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 10,
            }],
        )
        .unwrap();
    assert_eq!(wrong_gen.items[0].body, Err(CacheError::UnknownGeneration));
}

#[test]
fn timeout_quarantines_a_registered_region_until_fence() {
    let (mut store, auth) = open(None);
    let payload = b"inflight".to_vec();
    let reserved = store
        .reserve_put(&auth, 0, vec![put_item(&store, 1, 0, &payload, 1)])
        .unwrap();
    let outcome = reserved.items[0].body.as_ref().unwrap();
    let generation = outcome.generation;
    let region = outcome.region_id.unwrap();
    assert_eq!(
        store.state_of(&auth, generation, 0).unwrap(),
        BlockState::Writing
    );
    assert_eq!(
        store.state_of(&auth, generation, 500).unwrap(),
        BlockState::Quarantined
    );
    assert!(store.stats().retained_bytes >= payload.len() as u64);
    assert_eq!(store.stats().registrations, 1);
    let hidden = store
        .get(
            &auth,
            500,
            vec![GetItem {
                header: hdr(&store, 2, 500),
                generation,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 10,
            }],
        )
        .unwrap();
    assert_eq!(hidden.items[0].body, Err(CacheError::TimedOut));
    let early = store.reclaim_fenced(
        &auth,
        500,
        ReclaimRequest {
            request_id: RequestId::from_counter(3).unwrap(),
            epoch: store.epoch(),
            deadline_ms: 1_500,
            generation,
            observed_epoch: store.epoch(),
        },
    );
    assert!(matches!(early, Err(CacheError::FencingRejected { .. })));
    let epoch = store.bump_epoch(500);
    let reclaimed = store
        .reclaim_fenced(
            &auth,
            500,
            ReclaimRequest {
                request_id: RequestId::from_counter(4).unwrap(),
                epoch,
                deadline_ms: 1_500,
                generation,
                observed_epoch: epoch,
            },
        )
        .unwrap();
    assert_eq!(reclaimed.state, BlockState::Absent);
    assert_eq!(store.stats().retained_bytes, 0);
    assert_eq!(store.stats().registrations, 0);
    let again = store
        .reserve_put(&auth, 500, vec![put_item(&store, 5, 500, &payload, 1)])
        .unwrap();
    let new_region = again.items[0].body.as_ref().unwrap().region_id.unwrap();
    assert_ne!(new_region, region);
}

#[test]
fn cancel_unknown_quarantines_and_drained_reclaims() {
    let (mut store, auth) = open(None);
    let payload = b"cancel-me".to_vec();
    let generation = store
        .reserve_put(&auth, 0, vec![put_item(&store, 1, 0, &payload, 1)])
        .unwrap()
        .items[0]
        .body
        .as_ref()
        .unwrap()
        .generation;
    let cancelled = store
        .release_cancel(
            &auth,
            0,
            vec![ReleaseCancelItem {
                header: hdr(&store, 2, 0),
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
    let late = store
        .commit(
            &auth,
            0,
            vec![CommitItem {
                header: hdr(&store, 3, 0),
                generation,
                length: payload.len() as u64,
                checksum: content_checksum(&payload),
            }],
        )
        .unwrap();
    assert_eq!(late.items[0].body, Err(CacheError::Cancelled));
    let (mut store, auth) = open(None);
    let generation = store
        .reserve_put(&auth, 0, vec![put_item(&store, 1, 0, &payload, 1)])
        .unwrap()
        .items[0]
        .body
        .as_ref()
        .unwrap()
        .generation;
    let drained = store
        .release_cancel(
            &auth,
            0,
            vec![ReleaseCancelItem {
                header: hdr(&store, 2, 0),
                generation,
                region_id: None,
                pin_owner: None,
                attestation: Attestation::Drained,
            }],
        )
        .unwrap();
    assert_eq!(
        drained.items[0].body.as_ref().unwrap().state,
        BlockState::Absent
    );
    assert_eq!(store.stats().retained_bytes, 0);
}

#[test]
fn unregistered_reservation_timeout_frees_immediately() {
    let (mut store, auth) = open(None);
    let item = ReservePutItem {
        payload: None,
        ..put_item(&store, 1, 0, b"reserved-only", 1)
    };
    let generation = store.reserve_put(&auth, 0, vec![item]).unwrap().items[0]
        .body
        .as_ref()
        .unwrap()
        .generation;
    assert_eq!(
        store.state_of(&auth, generation, 0).unwrap(),
        BlockState::Reserved
    );
    assert_eq!(store.stats().registrations, 0);
    assert_eq!(
        store.state_of(&auth, generation, 500).unwrap_err(),
        CacheError::UnknownGeneration
    );
    assert_eq!(store.stats().retained_bytes, 0);
}

#[test]
fn invalidate_hides_bytes_and_waits_for_reader_pins() {
    let (mut store, auth) = open(None);
    let payload = b"pinned".to_vec();
    let generation = commit_bytes(&mut store, &auth, 1, &payload);
    let hit = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: hdr(&store, 10, 0),
                generation,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 50,
            }],
        )
        .unwrap();
    let region = hit.items[0].body.as_ref().unwrap().region_id;
    let invalidated = store
        .invalidate(
            &auth,
            0,
            vec![InvalidateItem {
                header: hdr(&store, 11, 0),
                generation,
            }],
        )
        .unwrap();
    assert_eq!(
        invalidated.items[0].body.as_ref().unwrap().state,
        BlockState::Evicting
    );
    let hidden = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: hdr(&store, 12, 0),
                generation,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 10,
            }],
        )
        .unwrap();
    assert!(hidden.items[0].body.is_err());
    assert!(store.stats().retained_bytes >= payload.len() as u64);
    let released = store
        .release_cancel(
            &auth,
            0,
            vec![ReleaseCancelItem {
                header: hdr(&store, 13, 0),
                generation,
                region_id: Some(region),
                pin_owner: Some(RequestId::from_counter(10).unwrap()),
                attestation: Attestation::Drained,
            }],
        )
        .unwrap();
    assert_eq!(
        released.items[0].body.as_ref().unwrap().state,
        BlockState::Absent
    );
    assert_eq!(store.stats().retained_bytes, 0);
}

#[test]
fn reader_lease_expiry_does_not_recycle_the_buffer() {
    let (mut store, auth) = open(None);
    let payload = b"lease".to_vec();
    let generation = commit_bytes(&mut store, &auth, 1, &payload);
    let hit = store
        .get(
            &auth,
            0,
            vec![GetItem {
                header: hdr(&store, 8, 0),
                generation,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 10,
            }],
        )
        .unwrap();
    let region = hit.items[0].body.as_ref().unwrap().region_id;
    store
        .invalidate(
            &auth,
            20,
            vec![InvalidateItem {
                header: hdr(&store, 9, 20),
                generation,
            }],
        )
        .unwrap();
    assert_eq!(
        store.state_of(&auth, generation, 20).unwrap(),
        BlockState::Evicting
    );
    assert!(store.stats().registrations >= 1);
    let epoch = store.bump_epoch(20);
    let reclaimed = store
        .reclaim_fenced(
            &auth,
            20,
            ReclaimRequest {
                request_id: RequestId::from_counter(15).unwrap(),
                epoch,
                deadline_ms: 2_000,
                generation,
                observed_epoch: epoch,
            },
        )
        .unwrap();
    assert_eq!(reclaimed.state, BlockState::Absent);
    let _ = region;
}

#[test]
fn quotas_reject_without_growing_past_the_ceiling() {
    let mut tight = limits();
    tight.max_object_bytes = 4;
    tight.max_in_flight_requests = 1;
    tight.max_registrations = 1;
    tight.max_retained_bytes = 8;
    tight.max_retained_bytes_per_tenant = 8;
    tight.max_in_flight_bytes_per_tenant = 8;
    let id = synthetic_identity(1, 1);
    let policy = TenantPolicy::new(id.tenant.clone(), id.trust_domain.clone(), FP).unwrap();
    let auth = AuthContext::new(id.tenant, id.trust_domain, FP).unwrap();
    let mut store = MemoryStore::new(tight, vec![policy]).unwrap();
    let too_big = store.reserve_put(&auth, 0, vec![put_item(&store, 1, 0, b"12345", 1)]);
    assert!(matches!(
        too_big.unwrap().items[0].body,
        Err(CacheError::ObjectTooLarge { .. })
    ));
    let first = store
        .reserve_put(&auth, 0, vec![put_item(&store, 2, 0, b"abcd", 1)])
        .unwrap();
    assert!(first.items[0].body.is_ok());
    let second = store
        .reserve_put(&auth, 0, vec![put_item(&store, 3, 0, b"wxyz", 2)])
        .unwrap();
    assert!(matches!(
        second.items[0].body,
        Err(CacheError::QuotaExceeded { .. })
    ));
    assert!(store.stats().retained_bytes <= 8);
    assert!(store.stats().in_flight_requests <= 1);
    assert!(store.stats().registrations <= 1);
}

#[test]
fn idempotent_replay_and_conflict() {
    let (mut store, auth) = open(None);
    let payload = b"once".to_vec();
    let item = put_item(&store, 7, 0, &payload, 1);
    let first = store.reserve_put(&auth, 0, vec![item.clone()]).unwrap();
    let second = store.reserve_put(&auth, 0, vec![item.clone()]).unwrap();
    assert_eq!(first.items[0].body, second.items[0].body);
    assert_eq!(store.stats().in_flight_requests, 1);
    let mut changed = item.clone();
    changed.length = 3;
    changed.payload = Some(b"no!".to_vec());
    changed.checksum = content_checksum(b"no!");
    let conflict = store.reserve_put(&auth, 0, vec![changed]).unwrap();
    assert_eq!(conflict.items[0].body, Err(CacheError::IdempotencyConflict));
}

#[test]
fn auth_fails_closed_and_blocks_cross_tenant_reads() {
    let (store, _) = {
        let store = MemoryStore::new(limits(), vec![]).unwrap();
        let id = synthetic_identity(1, 1);
        let auth = AuthContext::new(id.tenant, id.trust_domain, FP).unwrap();
        (store, auth)
    };
    let id = synthetic_identity(1, 1);
    let auth = AuthContext::new(id.tenant.clone(), id.trust_domain.clone(), FP).unwrap();
    let mut store = store;
    let denied = store.lookup(
        &auth,
        0,
        vec![LookupItem {
            header: hdr(&store, 1, 0),
            identity: id.clone(),
        }],
    );
    assert_eq!(denied, Err(CacheError::Unauthorized));

    let other_fp = [4u8; 32];
    let other_policy = TenantPolicy::new(
        TenantId::new(b"tenant-b".to_vec()).unwrap(),
        TrustDomain::new(b"td-local".to_vec()).unwrap(),
        other_fp,
    )
    .unwrap();
    let (mut store, auth_a) = open(Some(other_policy));
    let generation = commit_bytes(&mut store, &auth_a, 1, b"secret-bytes");
    let auth_b = AuthContext::new(
        TenantId::new(b"tenant-b".to_vec()).unwrap(),
        TrustDomain::new(b"td-local".to_vec()).unwrap(),
        other_fp,
    )
    .unwrap();
    let stolen = store
        .get(
            &auth_b,
            0,
            vec![GetItem {
                header: hdr(&store, 9, 0),
                generation,
                identity: synthetic_identity(1, 1),
                pin_ttl_ms: 10,
            }],
        )
        .unwrap();
    assert_eq!(stolen.items[0].body, Err(CacheError::Unauthorized));
    assert_eq!(store.stats().committed, 1);
}

#[test]
fn batch_is_item_scoped_and_duplicate_ids_fail_closed() {
    let (mut store, auth) = open(None);
    let ok = put_item(&store, 1, 0, b"ok-block", 1);
    let mut huge = put_item(&store, 2, 0, b"x", 2);
    huge.length = limits().max_object_bytes + 1;
    huge.payload = None;
    let mixed = store.reserve_put(&auth, 0, vec![ok, huge]).unwrap();
    assert!(mixed.items[0].body.is_ok());
    assert!(matches!(
        mixed.items[1].body,
        Err(CacheError::ObjectTooLarge { .. })
    ));
    let dup = put_item(&store, 3, 0, b"dup", 3);
    let denied = store.reserve_put(&auth, 0, vec![dup.clone(), dup]);
    assert_eq!(
        denied,
        Err(CacheError::Malformed {
            reason: "duplicate request_id"
        })
    );
}

#[test]
fn protocol_frame_drives_the_store() {
    let (mut store, auth) = open(None);
    let payload = b"from-wire".to_vec();
    let item = put_item(&store, 1, 0, &payload, 1);
    let encoded = gpucache_protocol::encode_request(
        &auth,
        &gpucache_protocol::RequestBody::ReservePut(vec![item]),
    )
    .unwrap();
    let (wire_auth, body) = gpucache_protocol::decode_request(&encoded).unwrap();
    let gpucache_protocol::RequestBody::ReservePut(items) = body else {
        panic!("op");
    };
    let reserved = store.reserve_put(&wire_auth, 0, items).unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    store
        .commit(
            &auth,
            0,
            vec![CommitItem {
                header: hdr(&store, 2, 0),
                generation,
                length: payload.len() as u64,
                checksum: content_checksum(&payload),
            }],
        )
        .unwrap();
    let looked = store
        .lookup(
            &auth,
            0,
            vec![LookupItem {
                header: hdr(&store, 3, 0),
                identity: synthetic_identity(1, 1),
            }],
        )
        .unwrap();
    match looked.items[0].body.clone().unwrap() {
        LookupOutcome::Hit {
            length, checksum, ..
        } => {
            assert_eq!(length, payload.len() as u64);
            assert_eq!(checksum, content_checksum(&payload));
        }
        other => panic!("expected hit, got {other:?}"),
    }
}

#[test]
fn request_deadline_is_enforced() {
    let (mut store, auth) = open(None);
    let mut item = put_item(&store, 1, 0, b"late", 1);
    item.header.deadline_ms = 5;
    let timed = store.reserve_put(&auth, 5, vec![item]).unwrap();
    assert_eq!(timed.items[0].body, Err(CacheError::TimedOut));
    assert_eq!(store.stats().retained_bytes, 0);
}

#[test]
fn partial_prefix_returns_the_whole_shorter_block_and_does_not_slice() {
    let (mut store, auth) = open(None);
    let payload = b"prefix-block".to_vec();
    let generation = commit_bytes(&mut store, &auth, 10, &payload);
    let mut longer = synthetic_identity(1, 1);
    longer
        .prefix_chain
        .push(gpucache_core_types::Digest([0xab; 32]));
    let looked = store
        .lookup(
            &auth,
            0,
            vec![LookupItem {
                header: hdr(&store, 30, 0),
                identity: longer.clone(),
            }],
        )
        .unwrap();
    match looked.items[0].body.clone().unwrap() {
        LookupOutcome::PrefixHit {
            generation: got_generation,
            matched_prefix_blocks,
            valid_length,
            length,
            ..
        } => {
            assert_eq!(got_generation, generation);
            assert_eq!(matched_prefix_blocks, 1);
            assert_eq!(length, payload.len() as u64);
            let matched = gpucache_protocol::identity_for_prefix_hit(
                &longer,
                valid_length,
                matched_prefix_blocks,
            )
            .unwrap();
            assert_eq!(matched, synthetic_identity(1, 1));
            let fetched = store
                .get(
                    &auth,
                    0,
                    vec![GetItem {
                        header: hdr(&store, 31, 0),
                        generation,
                        identity: matched,
                        pin_ttl_ms: 100,
                    }],
                )
                .unwrap();
            assert_eq!(fetched.items[0].body.as_ref().unwrap().payload, payload);
            let wrong = store
                .get(
                    &auth,
                    0,
                    vec![GetItem {
                        header: hdr(&store, 32, 0),
                        generation,
                        identity: longer,
                        pin_ttl_ms: 100,
                    }],
                )
                .unwrap();
            assert!(matches!(wrong.items[0].body, Err(CacheError::Incompatible)));
        }
        other => panic!("expected a whole-block prefix hit, got {other:?}"),
    }
    let mut sliced = synthetic_identity(1, 1);
    sliced.valid_length = 4;
    let slice_lookup = store
        .lookup(
            &auth,
            0,
            vec![LookupItem {
                header: hdr(&store, 33, 0),
                identity: sliced,
            }],
        )
        .unwrap();
    assert!(matches!(
        slice_lookup.items[0].body.as_ref().unwrap(),
        LookupOutcome::Miss {
            reason: gpucache_protocol::MissReason::Incompatible
        }
    ));
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

#[test]
fn replay_state_is_scoped_to_the_authenticated_tenant() {
    let mut other = synthetic_identity(1, 1);
    other.tenant = TenantId::new(b"tenant-b".to_vec()).unwrap();
    let policy = TenantPolicy::new(other.tenant.clone(), other.trust_domain.clone(), FP).unwrap();
    let other_auth =
        AuthContext::new(other.tenant.clone(), other.trust_domain.clone(), FP).unwrap();
    let (mut store, auth) = open(Some(policy));
    let generation = commit_bytes(&mut store, &auth, 10, b"payload");
    let denied = store
        .commit(
            &other_auth,
            0,
            vec![CommitItem {
                header: hdr(&store, 11, 0),
                generation,
                length: 7,
                checksum: content_checksum(b"payload"),
            }],
        )
        .unwrap();
    assert_eq!(denied.items[0].body, Err(CacheError::Unauthorized));
    let mut request = put_item(&store, 10, 0, b"other", 1);
    request.identity = other;
    let independent = store.reserve_put(&other_auth, 0, vec![request]).unwrap();
    assert!(independent.items[0].body.is_ok());
}
