// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Actual loopback TCP gates. Synthetic CPU bytes, not an engine or TLS test.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpucache_core_types::{
    content_checksum, synthetic_identity, Attestation, CacheError, CacheIdentity, QuotaKind,
    RequestId, ResourceLimits, TenantId, TrustDomain,
};
use gpucache_protocol::{
    Acl, AuditEvent, AuthContext, CommitItem, GetItem, InvalidateItem, ItemHeader, LookupItem,
    LookupOutcome, ReleaseCancelItem, ReservePutItem, TenantPolicy,
};
use gpucache_storage::FileStore;
use gpucache_transport::{serve, ServerConfig, ServerHandle, TcpClient};

const PAYLOAD: &[u8] = b"synthetic-cpu-kv";

struct Fixture {
    path: PathBuf,
    identity: CacheIdentity,
    auth: AuthContext,
    other_auth: AuthContext,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gpucache-hot-security-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let identity = synthetic_identity(1, 1);
        let auth = AuthContext::new(
            identity.tenant.clone(),
            identity.trust_domain.clone(),
            [9; 32],
        )
        .unwrap();
        let other_auth = AuthContext::new(
            TenantId::new(b"tenant-b".to_vec()).unwrap(),
            identity.trust_domain.clone(),
            [8; 32],
        )
        .unwrap();
        Self {
            path,
            identity,
            auth,
            other_auth,
        }
    }

    fn open(&self) -> FileStore {
        let policies = [&self.auth, &self.other_auth]
            .into_iter()
            .map(|auth| {
                TenantPolicy::new(
                    auth.tenant.clone(),
                    auth.trust_domain.clone(),
                    auth.credential_fingerprint,
                )
                .unwrap()
            })
            .collect();
        FileStore::open(&self.path, ResourceLimits::dev_defaults(), policies).unwrap()
    }

    fn seeded(&self) -> (FileStore, u64) {
        let mut store = self.open();
        let head = |n| ItemHeader {
            request_id: RequestId::from_counter(n).unwrap(),
            epoch: store.epoch(),
            deadline_ms: u64::MAX,
        };
        let put_header = head(1);
        let commit_header = head(2);
        let generation = store
            .reserve_put(
                &self.auth,
                0,
                vec![ReservePutItem {
                    header: put_header,
                    identity: self.identity.clone(),
                    generation: 0,
                    length: PAYLOAD.len() as u64,
                    checksum: content_checksum(PAYLOAD),
                    payload: Some(PAYLOAD.to_vec()),
                    lease_ttl_ms: 30_000,
                }],
            )
            .unwrap()
            .items[0]
            .body
            .as_ref()
            .unwrap()
            .generation;
        store
            .commit(
                &self.auth,
                0,
                vec![CommitItem {
                    header: commit_header,
                    generation,
                    length: PAYLOAD.len() as u64,
                    checksum: content_checksum(PAYLOAD),
                }],
            )
            .unwrap()
            .items[0]
            .body
            .as_ref()
            .unwrap();
        (store, generation)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn connect(server: &ServerHandle, auth: AuthContext) -> TcpClient {
    TcpClient::connect(server.addr(), auth, Duration::from_secs(5), 1024 * 1024).unwrap()
}

fn header(client: &TcpClient, n: u128) -> ItemHeader {
    ItemHeader {
        request_id: RequestId::from_counter(n).unwrap(),
        epoch: client.epoch(),
        deadline_ms: client.deadline(30_000),
    }
}

fn lookup(client: &TcpClient, n: u128, identity: &CacheIdentity) -> LookupItem {
    LookupItem {
        header: header(client, n),
        identity: identity.clone(),
    }
}

fn put(client: &TcpClient, n: u128, identity: &CacheIdentity) -> ReservePutItem {
    ReservePutItem {
        header: header(client, n),
        identity: identity.clone(),
        generation: 0,
        length: PAYLOAD.len() as u64,
        checksum: content_checksum(PAYLOAD),
        payload: Some(PAYLOAD.to_vec()),
        lease_ttl_ms: 30_000,
    }
}

fn commit(client: &TcpClient, n: u128, generation: u64) -> CommitItem {
    CommitItem {
        header: header(client, n),
        generation,
        length: PAYLOAD.len() as u64,
        checksum: content_checksum(PAYLOAD),
    }
}

fn get(client: &TcpClient, n: u128, identity: &CacheIdentity, generation: u64) -> GetItem {
    GetItem {
        header: header(client, n),
        generation,
        identity: identity.clone(),
        pin_ttl_ms: 30_000,
    }
}

fn cancel(client: &TcpClient, n: u128, generation: u64) -> ReleaseCancelItem {
    ReleaseCancelItem {
        header: header(client, n),
        generation,
        region_id: None,
        pin_owner: None,
        attestation: Attestation::Drained,
    }
}

fn invalidate(client: &TcpClient, n: u128, generation: u64) -> InvalidateItem {
    InvalidateItem {
        header: header(client, n),
        generation,
    }
}

fn every_entry_denies(client: &mut TcpClient, identity: &CacheIdentity, generation: u64) {
    assert_eq!(
        client.lookup(vec![lookup(client, 10, identity)]),
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        client.reserve_put(vec![put(client, 11, identity)]),
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        client.commit(vec![commit(client, 12, generation)]),
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        client.get(vec![get(client, 13, identity, generation)]),
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        client.release_cancel(vec![cancel(client, 14, generation)]),
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        client.invalidate(vec![invalidate(client, 15, generation)]),
        Err(CacheError::Unauthorized)
    );
}

#[test]
fn empty_acl_denies_every_tcp_entry_even_with_a_valid_store_credential() {
    let fixture = Fixture::new();
    let (store, generation) = fixture.seeded();
    let server = serve(
        store,
        ServerConfig {
            acl: Some(Acl::new(1).unwrap()),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let mut client = connect(&server, fixture.auth.clone());
    every_entry_denies(&mut client, &fixture.identity, generation);
    assert_eq!(server.denied_requests(), 6);
    assert_eq!(server.security_events(), vec![AuditEvent::Deny; 6]);
}

#[test]
fn readonly_tcp_acl_allows_get_and_pin_release_but_denies_all_writer_paths() {
    let fixture = Fixture::new();
    let (store, generation) = fixture.seeded();
    let mut acl = Acl::new(2).unwrap();
    acl.grant(fixture.auth.clone(), false).unwrap();
    let server = serve(
        store,
        ServerConfig {
            acl: Some(acl),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let mut client = connect(&server, fixture.auth.clone());
    assert!(matches!(
        client
            .lookup(vec![lookup(&client, 10, &fixture.identity)])
            .unwrap()
            .items[0]
            .body,
        Ok(LookupOutcome::Hit { .. })
    ));
    for denied in [
        client
            .reserve_put(vec![put(&client, 11, &fixture.identity)])
            .err(),
        client.commit(vec![commit(&client, 12, generation)]).err(),
        client
            .release_cancel(vec![cancel(&client, 13, generation)])
            .err(),
        client
            .invalidate(vec![invalidate(&client, 14, generation)])
            .err(),
    ] {
        assert_eq!(denied, Some(CacheError::Unauthorized));
    }
    let request = get(&client, 15, &fixture.identity, generation);
    let owner = request.header.request_id;
    let hit = client
        .get(vec![request])
        .unwrap()
        .items
        .remove(0)
        .body
        .unwrap();
    assert_eq!(hit.payload, PAYLOAD);
    let release = ReleaseCancelItem {
        header: header(&client, 16),
        generation,
        region_id: Some(hit.region_id),
        pin_owner: Some(owner),
        attestation: Attestation::Drained,
    };
    assert_eq!(
        client.release_cancel(vec![release.clone(), cancel(&client, 17, generation)]),
        Err(CacheError::Unauthorized)
    );
    // Denying the mixed batch did not consume or release the first pin.
    client.release_cancel(vec![release]).unwrap().items[0]
        .body
        .as_ref()
        .unwrap();
    assert_eq!(server.denied_requests(), 5);
}

#[test]
fn an_acl_grant_cannot_override_store_tenant_trust_or_fingerprint_policies() {
    let fixture = Fixture::new();
    let (store, generation) = fixture.seeded();
    let mut wrong_fingerprint = fixture.auth.clone();
    wrong_fingerprint.credential_fingerprint = [7; 32];
    let mut wrong_trust = fixture.auth.clone();
    wrong_trust.trust_domain = TrustDomain::new(b"other-trust".to_vec()).unwrap();
    let mut unknown_tenant = fixture.auth.clone();
    unknown_tenant.tenant = TenantId::new(b"unknown-tenant".to_vec()).unwrap();
    let mut acl = Acl::new(4).unwrap();
    for auth in [
        &wrong_fingerprint,
        &wrong_trust,
        &unknown_tenant,
        &fixture.auth,
    ] {
        acl.grant(auth.clone(), true).unwrap();
    }
    let server = serve(
        store,
        ServerConfig {
            acl: Some(acl),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    for auth in [wrong_fingerprint, wrong_trust, unknown_tenant] {
        let mut client = connect(&server, auth);
        every_entry_denies(&mut client, &fixture.identity, generation);
    }
    let mut valid = connect(&server, fixture.auth.clone());
    let hit = valid
        .get(vec![get(&valid, 100, &fixture.identity, generation)])
        .unwrap();
    assert_eq!(hit.items[0].body.as_ref().unwrap().payload, PAYLOAD);
    assert_eq!(server.denied_requests(), 18);
}

#[test]
fn valid_other_tenant_cannot_read_or_mutate_a_foreign_generation() {
    let fixture = Fixture::new();
    let (store, generation) = fixture.seeded();
    let server = serve(store, ServerConfig::default()).unwrap();
    let mut other = connect(&server, fixture.other_auth.clone());
    assert_eq!(
        other
            .lookup(vec![lookup(&other, 10, &fixture.identity)])
            .unwrap()
            .items[0]
            .body,
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        other
            .reserve_put(vec![put(&other, 11, &fixture.identity)])
            .unwrap()
            .items[0]
            .body,
        Err(CacheError::Unauthorized)
    );
    let mut own_identity = fixture.identity.clone();
    own_identity.tenant = fixture.other_auth.tenant.clone();
    assert_eq!(
        other
            .get(vec![get(&other, 12, &own_identity, generation)])
            .unwrap()
            .items[0]
            .body,
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        other
            .commit(vec![commit(&other, 13, generation)])
            .unwrap()
            .items[0]
            .body,
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        other
            .release_cancel(vec![cancel(&other, 14, generation)])
            .unwrap()
            .items[0]
            .body,
        Err(CacheError::Unauthorized)
    );
    assert_eq!(
        other
            .invalidate(vec![invalidate(&other, 15, generation)])
            .unwrap()
            .items[0]
            .body,
        Err(CacheError::Unauthorized)
    );
    let mut owner = connect(&server, fixture.auth.clone());
    for mut wrong in [
        synthetic_identity(2, 1),
        synthetic_identity(1, 2),
        fixture.identity.clone(),
    ] {
        if wrong == fixture.identity {
            wrong.trust_domain = TrustDomain::new(b"wrong-trust".to_vec()).unwrap();
        }
        let hit = owner
            .get(vec![get(&owner, 100, &wrong, generation)])
            .unwrap();
        assert!(matches!(
            hit.items[0].body,
            Err(CacheError::Incompatible | CacheError::Unauthorized)
        ));
    }
    assert_eq!(
        owner
            .get(vec![get(&owner, 101, &fixture.identity, generation)])
            .unwrap()
            .items[0]
            .body
            .as_ref()
            .unwrap()
            .payload,
        PAYLOAD
    );
}

#[test]
fn tcp_rate_budget_is_atomic_shared_by_connections_and_separate_by_tenant() {
    let fixture = Fixture::new();
    let server = serve(
        fixture.open(),
        ServerConfig {
            rate_limit_items: 3,
            rate_window_ms: 60_000,
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let mut first = connect(&server, fixture.auth.clone());
    let mut second = connect(&server, fixture.auth.clone());
    first
        .lookup(vec![
            lookup(&first, 10, &fixture.identity),
            lookup(&first, 11, &fixture.identity),
        ])
        .unwrap();
    assert_eq!(
        second.lookup(vec![
            lookup(&second, 12, &fixture.identity),
            lookup(&second, 13, &fixture.identity),
        ]),
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::RateLimit
        })
    );
    second
        .lookup(vec![lookup(&second, 14, &fixture.identity)])
        .unwrap();
    assert_eq!(
        first.lookup(vec![lookup(&first, 15, &fixture.identity)]),
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::RateLimit
        })
    );
    let mut quiet = connect(&server, fixture.other_auth.clone());
    let mut quiet_identity = fixture.identity.clone();
    quiet_identity.tenant = fixture.other_auth.tenant.clone();
    quiet
        .lookup(vec![lookup(&quiet, 16, &quiet_identity)])
        .unwrap();
    assert_eq!(server.rate_limited_requests(), 2);
}

#[test]
fn exhausted_tcp_rate_budget_still_allows_authorized_reader_pin_drain() {
    let fixture = Fixture::new();
    let (store, generation) = fixture.seeded();
    let mut acl = Acl::new(1).unwrap();
    acl.grant(fixture.auth.clone(), false).unwrap();
    let server = serve(
        store,
        ServerConfig {
            acl: Some(acl),
            rate_limit_items: 1,
            rate_window_ms: 60_000,
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let mut client = connect(&server, fixture.auth.clone());
    let request = get(&client, 10, &fixture.identity, generation);
    let pin_owner = request.header.request_id;
    let hit = client
        .get(vec![request])
        .unwrap()
        .items
        .remove(0)
        .body
        .unwrap();
    let release = ReleaseCancelItem {
        header: header(&client, 11),
        generation,
        region_id: Some(hit.region_id),
        pin_owner: Some(pin_owner),
        attestation: Attestation::Drained,
    };
    let mut bad_auth = fixture.auth.clone();
    bad_auth.credential_fingerprint = [7; 32];
    let mut attacker = connect(&server, bad_auth);
    assert_eq!(
        attacker.release_cancel(vec![release.clone()]),
        Err(CacheError::Unauthorized)
    );
    client.release_cancel(vec![release]).unwrap().items[0]
        .body
        .as_ref()
        .unwrap();
    assert_eq!(server.rate_limited_requests(), 0);
    assert_eq!(server.denied_requests(), 1);
    assert_eq!(
        client.get(vec![get(&client, 12, &fixture.identity, generation)]),
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::RateLimit
        })
    );
}

#[test]
fn tcp_deny_storm_has_a_bounded_audit_and_does_not_spend_the_victims_rate_budget() {
    let fixture = Fixture::new();
    let server = serve(
        fixture.open(),
        ServerConfig {
            rate_limit_items: 1,
            rate_window_ms: 60_000,
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let mut wrong = fixture.auth.clone();
    wrong.credential_fingerprint = [7; 32];
    let mut attacker = connect(&server, wrong);
    for n in 1..=300 {
        assert_eq!(
            attacker.lookup(vec![lookup(&attacker, n, &fixture.identity)]),
            Err(CacheError::Unauthorized)
        );
    }
    assert_eq!(server.denied_requests(), 300);
    assert_eq!(server.security_events(), vec![AuditEvent::Deny; 256]);
    let mut victim = connect(&server, fixture.auth.clone());
    victim
        .lookup(vec![lookup(&victim, 500, &fixture.identity)])
        .unwrap();
}

#[test]
fn server_shutdown_closes_idle_connections_before_reopening_the_store() {
    let fixture = Fixture::new();
    let (store, generation) = fixture.seeded();
    let server = serve(store, ServerConfig::default()).unwrap();
    let mut stale = connect(&server, fixture.auth.clone());
    let previous_epoch = stale.epoch();
    drop(server);
    let store = fixture.open();
    assert_eq!(store.epoch(), previous_epoch + 1);
    let server = serve(store, ServerConfig::default()).unwrap();
    assert!(stale
        .invalidate(vec![invalidate(&stale, 10, generation)])
        .is_err());
    let mut fresh = connect(&server, fixture.auth.clone());
    assert!(matches!(
        fresh
            .lookup(vec![lookup(&fresh, 11, &fixture.identity)])
            .unwrap()
            .items[0]
            .body,
        Ok(LookupOutcome::Hit { .. })
    ));
}
