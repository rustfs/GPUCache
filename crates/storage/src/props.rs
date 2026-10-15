// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{
    content_checksum, synthetic_identity, CacheError, RequestId, ResourceLimits,
};
use gpucache_protocol::{
    AuthContext, CommitItem, GetItem, InvalidateItem, ItemHeader, LookupItem, LookupOutcome,
    ReservePutItem, TenantPolicy,
};

use crate::MemoryStore;

fn lcg(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    *state
}

#[test]
fn random_operations_never_return_uncommitted_or_foreign_bytes() {
    let mut limits = ResourceLimits::dev_defaults();
    limits.max_object_bytes = 32;
    limits.max_retained_bytes = 256;
    limits.max_retained_bytes_per_tenant = 256;
    limits.max_in_flight_bytes_per_tenant = 128;
    limits.max_in_flight_requests = 4;
    limits.max_registrations = 4;
    limits.max_idempotency_entries = 128;
    let identity = synthetic_identity(1, 1);
    let fp = [9u8; 32];
    let policy =
        TenantPolicy::new(identity.tenant.clone(), identity.trust_domain.clone(), fp).unwrap();
    let auth = AuthContext::new(identity.tenant, identity.trust_domain, fp).unwrap();
    let mut store = MemoryStore::new(limits, vec![policy]).unwrap();
    let mut shadow: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut seed = 0xdec0_u64;
    let mut next_id = 100u128;
    for _ in 0..80 {
        let roll = lcg(&mut seed) % 10;
        let epoch = store.epoch();
        let header = |n: u128| ItemHeader {
            request_id: RequestId::from_counter(n).unwrap(),
            epoch,
            deadline_ms: 10_000,
        };
        if roll < 4 {
            let len = 1 + (lcg(&mut seed) % 8) as usize;
            let mut payload = vec![0u8; len];
            for byte in &mut payload {
                *byte = lcg(&mut seed) as u8;
            }
            let bad = lcg(&mut seed) % 5 == 0;
            let checksum = if bad {
                content_checksum(b"not-the-payload")
            } else {
                content_checksum(&payload)
            };
            let id_reserve = next_id;
            next_id += 1;
            let reserved = store.reserve_put(
                &auth,
                0,
                vec![ReservePutItem {
                    header: header(id_reserve),
                    identity: synthetic_identity(1, 1),
                    generation: 0,
                    length: payload.len() as u64,
                    checksum,
                    payload: Some(payload.clone()),
                    lease_ttl_ms: 5_000,
                }],
            );
            if let Ok(reply) = reserved {
                if let Ok(outcome) = &reply.items[0].body {
                    let id_commit = next_id;
                    next_id += 1;
                    let committed = store.commit(
                        &auth,
                        0,
                        vec![CommitItem {
                            header: header(id_commit),
                            generation: outcome.generation,
                            length: payload.len() as u64,
                            checksum,
                        }],
                    );
                    if let Ok(done) = committed {
                        if done.items[0].body.is_ok() && !bad {
                            shadow.push((outcome.generation, payload));
                        } else {
                            assert!(done.items[0].body.is_err() || bad);
                            if bad {
                                assert!(done.items[0].body.is_err());
                            }
                        }
                    }
                }
            }
        } else if roll == 4 {
            if let Some((generation, _)) = shadow.pop() {
                let _ = store.invalidate(
                    &auth,
                    0,
                    vec![InvalidateItem {
                        header: header(next_id),
                        generation,
                    }],
                );
                next_id += 1;
            }
        } else if roll == 5 {
            let mut other = synthetic_identity(1, 1);
            other.layout.family = 7;
            other.shard.tp_size = 2;
            let looked = store
                .lookup(
                    &auth,
                    0,
                    vec![LookupItem {
                        header: header(next_id),
                        identity: other,
                    }],
                )
                .unwrap();
            next_id += 1;
            assert!(!matches!(
                looked.items[0].body.as_ref().unwrap(),
                LookupOutcome::Hit { .. }
            ));
        }
        for (generation, payload) in &shadow {
            let looked = store
                .lookup(
                    &auth,
                    0,
                    vec![LookupItem {
                        header: header(next_id),
                        identity: synthetic_identity(1, 1),
                    }],
                )
                .unwrap();
            next_id += 1;
            match &looked.items[0].body {
                Ok(LookupOutcome::Hit {
                    checksum, length, ..
                }) => {
                    assert_eq!(*length, payload.len() as u64);
                    assert_eq!(*checksum, content_checksum(payload));
                }
                other => panic!("committed block disappeared: {other:?}"),
            }
            let got = store
                .get(
                    &auth,
                    0,
                    vec![GetItem {
                        header: header(next_id),
                        generation: *generation,
                        identity: synthetic_identity(1, 1),
                        pin_ttl_ms: 100,
                    }],
                )
                .unwrap();
            next_id += 1;
            match &got.items[0].body {
                Ok(hit) => assert_eq!(hit.payload, *payload),
                Err(err) => panic!("get of committed block failed: {err}"),
            }
        }
        let stats = store.stats();
        assert!(stats.retained_bytes <= limits.max_retained_bytes);
        assert!(stats.registrations <= limits.max_registrations);
        assert!(stats.in_flight_requests <= limits.max_in_flight_requests);
    }
    let _ = CacheError::TimedOut;
}
