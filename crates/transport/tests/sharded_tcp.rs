// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use gpucache_core_types::{
    content_checksum, synthetic_identity, Attestation, CacheError, CacheIdentity, QuotaKind,
    RequestId, ResourceLimits,
};
use gpucache_protocol::{
    AuthContext, CommitItem, GetItem, ItemHeader, LookupItem, LookupOutcome, ReleaseCancelItem,
    ReservePutItem, TenantPolicy,
};
use gpucache_storage::FileStore;
use gpucache_transport::{
    serve, ClientLimits, ServerConfig, ServerHandle, ShardEndpoint, ShardSession, ShardedClient,
};

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "gpucache-sharded-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn auth() -> AuthContext {
    let id = synthetic_identity(1, 1);
    AuthContext::new(id.tenant, id.trust_domain, [9; 32]).unwrap()
}
fn server(dir: &TempDir) -> ServerHandle {
    let auth = auth();
    let store = FileStore::open(
        &dir.0,
        ResourceLimits::dev_defaults(),
        vec![TenantPolicy::new(auth.tenant, auth.trust_domain, [9; 32]).unwrap()],
    )
    .unwrap();
    serve(store, ServerConfig::default()).unwrap()
}
fn endpoint(shard_id: u64, server: &ServerHandle, epoch: u64) -> ShardEndpoint {
    ShardEndpoint {
        shard_id,
        address: server.addr(),
        epoch,
    }
}
fn header(session: &ShardSession, id: u128) -> ItemHeader {
    ItemHeader {
        request_id: RequestId::from_counter(id).unwrap(),
        epoch: session.epoch(),
        deadline_ms: session.deadline(10_000),
    }
}
fn identity_on(client: &ShardedClient, shard: u64, seed: u64) -> CacheIdentity {
    for suffix in seed..seed + 1000 {
        let mut id = synthetic_identity(1, 1);
        id.prefix_chain[0].0[..8].copy_from_slice(&suffix.to_le_bytes());
        if client.shard_for(&id).unwrap() == shard {
            return id;
        }
    }
    panic!("rendezvous placement must cover two shards")
}
fn put(session: &mut ShardSession, identity: &CacheIdentity, id: u128) -> u64 {
    let payload = vec![1, 2, 3, 4];
    let reserve = ReservePutItem {
        header: header(session, id),
        identity: identity.clone(),
        generation: 0,
        length: 4,
        checksum: content_checksum(&payload),
        payload: Some(payload),
        lease_ttl_ms: 10_000,
    };
    let generation = session.reserve_put(vec![reserve]).unwrap().items[0]
        .body
        .as_ref()
        .unwrap()
        .generation;
    let committed = session
        .commit(vec![CommitItem {
            header: header(session, id + 1),
            generation,
            length: 4,
            checksum: content_checksum(&[1, 2, 3, 4]),
        }])
        .unwrap();
    assert!(committed.items[0].body.is_ok());
    generation
}

#[test]
fn two_shards_keep_generation_scope_tenant_identity_and_pool_reuse() {
    let dirs = [TempDir::new(), TempDir::new()];
    let servers = [server(&dirs[0]), server(&dirs[1])];
    let client = ShardedClient::new(
        auth(),
        vec![endpoint(1, &servers[0], 1), endpoint(2, &servers[1], 1)],
        ClientLimits::default(),
    )
    .unwrap();
    let ids = [identity_on(&client, 1, 0), identity_on(&client, 2, 100)];
    for (index, identity) in ids.iter().enumerate() {
        let mut session = client.route(identity).unwrap();
        assert_eq!(session.shard_id(), index as u64 + 1);
        // Both stores allocate the same local generation. No global remapping.
        assert_eq!(put(&mut session, identity, 1), 1);
        let reply = session
            .get(vec![GetItem {
                header: header(&session, 3),
                generation: 1,
                identity: identity.clone(),
                pin_ttl_ms: 1000,
            }])
            .unwrap();
        assert_eq!(reply.items[0].body.as_ref().unwrap().payload, [1, 2, 3, 4]);
        let mut foreign = synthetic_identity(2, 1);
        foreign.tenant = gpucache_core_types::TenantId::new(b"other-tenant".to_vec()).unwrap();
        assert!(matches!(
            session.lookup(vec![LookupItem {
                header: header(&session, 4),
                identity: foreign
            }]),
            Err(CacheError::Unauthorized)
        ));
        let wrong = identity_on(&client, index as u64 + 1, 200);
        let reply = session
            .get(vec![GetItem {
                header: header(&session, 5),
                generation: 1,
                identity: wrong,
                pin_ttl_ms: 1000,
            }])
            .unwrap();
        assert_eq!(reply.items[0].body, Err(CacheError::Incompatible));
    }
    assert_eq!(client.stats().connections, 2);
    assert_eq!(client.stats().idle_connections, 2);
    let mut session = client.route(&ids[0]).unwrap();
    assert!(matches!(
        session.lookup(vec![LookupItem {
            header: header(&session, 6),
            identity: ids[1].clone()
        }]),
        Err(CacheError::FencingRejected {
            reason: "wrong shard"
        })
    ));
    assert_eq!(client.stats().connection_reuses, 1);
}

#[test]
fn checked_out_connections_and_cancel_retry_entries_have_hard_bounds() {
    let dir = TempDir::new();
    let server = server(&dir);
    let limits = ClientLimits {
        max_connections: 2,
        max_connections_per_shard: 2,
        max_retry_entries: 8,
        ..ClientLimits::default()
    };
    let client = ShardedClient::new(auth(), vec![endpoint(0, &server, 1)], limits).unwrap();
    let mut first = client.route(&synthetic_identity(1, 1)).unwrap();
    let second = client.shard(0).unwrap();
    assert!(matches!(
        client.shard(0),
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::InFlightRequests
        })
    ));
    for n in 1..=200u128 {
        let result = first.release_cancel(vec![ReleaseCancelItem {
            header: header(&first, n),
            generation: n as u64,
            region_id: None,
            pin_owner: None,
            attestation: Attestation::Drained,
        }]);
        if n <= 8 {
            assert!(result.is_ok());
        } else {
            assert_eq!(
                result,
                Err(CacheError::QuotaExceeded {
                    kind: QuotaKind::IdempotencyTable
                })
            );
        }
        assert!(client.stats().retry_entries <= 8);
        assert_eq!(client.stats().connections, 2);
    }
    drop(second);
    for _ in 0..200 {
        drop(client.shard(0).unwrap());
    }
    assert_eq!(client.stats().connections, 2);
    assert_eq!(client.stats().idle_connections, 1);
}

#[test]
fn retry_admission_is_atomic_and_get_is_never_replayed() {
    let dir = TempDir::new();
    let server = server(&dir);
    let limits = ClientLimits {
        max_retry_entries: 4,
        ..ClientLimits::default()
    };
    let client = ShardedClient::new(auth(), vec![endpoint(0, &server, 1)], limits).unwrap();
    let identity = synthetic_identity(1, 1);
    let mut session = client.route(&identity).unwrap();
    put(&mut session, &identity, 1);
    let item = GetItem {
        header: header(&session, 3),
        generation: 1,
        identity: identity.clone(),
        pin_ttl_ms: 1000,
    };
    assert!(session.get(vec![item.clone()]).unwrap().items[0]
        .body
        .is_ok());
    assert_eq!(
        session.get(vec![item]),
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::IdempotencyTable
        })
    );
    let too_large = vec![
        LookupItem {
            header: header(&session, 4),
            identity: identity.clone(),
        },
        LookupItem {
            header: header(&session, 5),
            identity: identity.clone(),
        },
    ];
    assert_eq!(
        session.lookup(too_large),
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::IdempotencyTable
        })
    );
    assert_eq!(client.stats().retry_entries, 3);
    let item = LookupItem {
        header: header(&session, 4),
        identity,
    };
    assert!(session.lookup(vec![item.clone()]).is_ok());
    let mut different = item;
    different.header.deadline_ms += 1;
    assert_eq!(
        session.lookup(vec![different]),
        Err(CacheError::IdempotencyConflict)
    );
    assert_eq!(client.stats().retry_entries, 4);
}

#[test]
fn stale_route_update_and_early_replay_retention_contract_are_rejected() {
    let dir = TempDir::new();
    let server = server(&dir);
    let routes = vec![endpoint(0, &server, 1)];
    let invalid = ClientLimits {
        mutation_retry_retention_ms: Some(30_000),
        ..ClientLimits::default()
    };
    assert!(matches!(
        ShardedClient::new(auth(), routes.clone(), invalid),
        Err(CacheError::Malformed {
            reason: "shard client config"
        })
    ));
    let client = ShardedClient::new(auth(), routes.clone(), ClientLimits::default()).unwrap();
    let mut old = client.shard(0).unwrap();
    assert_eq!(
        client.replace_routes(1, routes),
        Err(CacheError::FencingRejected {
            reason: "route epoch"
        })
    );
    assert_eq!(client.stats().revision, 1);
    assert!(old
        .lookup(vec![LookupItem {
            header: header(&old, 1),
            identity: synthetic_identity(1, 1)
        }])
        .is_ok());
    client
        .replace_routes(1, vec![endpoint(0, &server, 2)])
        .unwrap();
    assert_eq!(
        old.lookup(vec![LookupItem {
            header: header(&old, 2),
            identity: synthetic_identity(1, 1)
        }]),
        Err(CacheError::FencingRejected {
            reason: "route revision"
        })
    );
    drop(old);
    assert_eq!(client.stats().connections, 0);
    // Merely publishing a route does not make an unfenced server current.
    assert!(matches!(
        client.shard(0),
        Err(CacheError::EpochMismatch {
            store: 1,
            request: 2
        })
    ));
    assert_eq!(client.stats().connections, 0);
}

#[test]
fn two_shard_restart_fences_concurrent_lookup_get_and_put_and_inflight_reservation() {
    let dirs = [TempDir::new(), TempDir::new()];
    let servers = [server(&dirs[0]), server(&dirs[1])];
    let client = ShardedClient::new(
        auth(),
        vec![endpoint(1, &servers[0], 1), endpoint(2, &servers[1], 1)],
        ClientLimits::default(),
    )
    .unwrap();
    let identities = [identity_on(&client, 1, 0), identity_on(&client, 2, 100)];
    for identity in &identities {
        put(&mut client.route(identity).unwrap(), identity, 1);
    }
    let ready = Arc::new(Barrier::new(7));
    let resumed = Arc::new(Barrier::new(7));
    let mut workers = Vec::new();
    for (shard, identity) in identities.iter().enumerate() {
        for operation in 0..3 {
            let mut session = client.route(identity).unwrap();
            let identity = identity.clone();
            let ready = Arc::clone(&ready);
            let resumed = Arc::clone(&resumed);
            workers.push(std::thread::spawn(move || {
                ready.wait();
                let id = 1000 + shard as u128 * 100 + operation;
                let result = match operation {
                    0 => session
                        .lookup(vec![LookupItem {
                            header: header(&session, id),
                            identity: identity.clone(),
                        }])
                        .map(|_| ()),
                    1 => session
                        .get(vec![GetItem {
                            header: header(&session, id),
                            generation: 1,
                            identity: identity.clone(),
                            pin_ttl_ms: 1000,
                        }])
                        .map(|_| ()),
                    _ => session
                        .reserve_put(vec![ReservePutItem {
                            header: header(&session, id),
                            identity: identity.clone(),
                            generation: 0,
                            length: 4,
                            checksum: content_checksum(&[1, 2, 3, 4]),
                            payload: Some(vec![1, 2, 3, 4]),
                            lease_ttl_ms: 1000,
                        }])
                        .map(|_| ()),
                };
                // An operation racing shutdown may finish or fail to recompute.
                if let Err(error) = result {
                    assert!(matches!(
                        error,
                        CacheError::Storage { .. }
                            | CacheError::TimedOut
                            | CacheError::EpochMismatch { .. }
                            | CacheError::FencingRejected { .. }
                    ));
                }
                resumed.wait();
                let result = session.lookup(vec![LookupItem {
                    header: header(&session, id + 10),
                    identity,
                }]);
                assert_eq!(
                    result,
                    Err(CacheError::FencingRejected {
                        reason: "route revision"
                    })
                );
            }));
        }
    }
    // A writing generation must not survive the epoch change as a hit.
    let absent_id = identity_on(&client, 1, 400);
    let mut pending = client.route(&absent_id).unwrap();
    let reserved = pending
        .reserve_put(vec![ReservePutItem {
            header: header(&pending, 5000),
            identity: absent_id.clone(),
            generation: 0,
            length: 4,
            checksum: content_checksum(&[1, 2, 3, 4]),
            payload: Some(vec![1, 2, 3, 4]),
            lease_ttl_ms: 10_000,
        }])
        .unwrap();
    let generation = reserved.items[0].body.as_ref().unwrap().generation;
    ready.wait();
    drop(servers);
    let restarted = [server(&dirs[0]), server(&dirs[1])];
    client
        .replace_routes(
            1,
            vec![endpoint(1, &restarted[0], 2), endpoint(2, &restarted[1], 2)],
        )
        .unwrap();
    resumed.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        pending.commit(vec![CommitItem {
            header: header(&pending, 5001),
            generation,
            length: 4,
            checksum: content_checksum(&[1, 2, 3, 4])
        }]),
        Err(CacheError::FencingRejected {
            reason: "route revision"
        })
    );
    drop(pending);
    let mut fresh = client.route(&absent_id).unwrap();
    assert_eq!(fresh.epoch(), 2);
    assert!(matches!(
        fresh
            .lookup(vec![LookupItem {
                header: header(&fresh, 6000),
                identity: absent_id
            }])
            .unwrap()
            .items[0]
            .body,
        Ok(LookupOutcome::Miss { .. })
    ));
}

#[test]
fn idle_connection_on_one_shard_does_not_starve_another_shard() {
    let dirs = [TempDir::new(), TempDir::new()];
    let servers = [server(&dirs[0]), server(&dirs[1])];
    let limits = ClientLimits {
        max_connections: 1,
        max_connections_per_shard: 1,
        ..ClientLimits::default()
    };
    let client = ShardedClient::new(
        auth(),
        vec![endpoint(1, &servers[0], 1), endpoint(2, &servers[1], 1)],
        limits,
    )
    .unwrap();
    for shard in [1, 2, 1, 2, 1, 2] {
        drop(client.shard(shard).unwrap());
        assert_eq!(client.stats().connections, 1);
        assert_eq!(client.stats().idle_connections, 1);
    }
}

#[test]
fn prefix_on_another_shard_is_a_clean_miss_without_global_fanout() {
    let dirs = [TempDir::new(), TempDir::new()];
    let servers = [server(&dirs[0]), server(&dirs[1])];
    let client = ShardedClient::new(
        auth(),
        vec![endpoint(1, &servers[0], 1), endpoint(2, &servers[1], 1)],
        ClientLimits::default(),
    )
    .unwrap();
    let short = identity_on(&client, 1, 0);
    put(&mut client.route(&short).unwrap(), &short, 1);
    let longer = (0..100u8)
        .find_map(|nonce| {
            let mut query = short.clone();
            query
                .prefix_chain
                .push(gpucache_core_types::Digest([nonce; 32]));
            (client.shard_for(&query).unwrap() == 2).then_some(query)
        })
        .expect("longer prefix can land on the other shard");
    let mut session = client.route(&longer).unwrap();
    let result = session
        .lookup(vec![LookupItem {
            header: header(&session, 10),
            identity: longer,
        }])
        .unwrap();
    assert!(matches!(
        result.items[0].body,
        Ok(LookupOutcome::Miss { .. })
    ));
    // A wrong local generation cannot fetch bytes from the shorter prefix's
    // owner. Neither lookup nor Get silently fans out or slices a block.
    let result = session.get(vec![GetItem {
        header: header(&session, 11),
        generation: 1,
        identity: short,
        pin_ttl_ms: 1000,
    }]);
    assert_eq!(
        result,
        Err(CacheError::FencingRejected {
            reason: "wrong shard"
        })
    );
}
