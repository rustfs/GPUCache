// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Framed Get admission over real loopback TCP with synthetic CPU payloads.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpucache_core_types::{
    content_checksum, synthetic_identity, Attestation, CacheError, QuotaKind, RequestId,
    ResourceLimits,
};
use gpucache_protocol::{
    AuthContext, CommitItem, GetItem, ItemHeader, LookupItem, LookupOutcome, ReleaseCancelItem,
    ReservePutItem, TenantPolicy,
};
use gpucache_storage::FileStore;
use gpucache_transport::{serve, ServerConfig, ServerHandle, TcpClient};

const FRAME_BYTES: usize = 2048;

struct Fixture {
    path: PathBuf,
    auth: AuthContext,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let identity = synthetic_identity(1, 1);
        Self {
            path: std::env::temp_dir().join(format!(
                "gpucache-reply-budget-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
            auth: AuthContext::new(identity.tenant, identity.trust_domain, [9; 32]).unwrap(),
        }
    }

    fn open(&self, max_pins: u32) -> FileStore {
        let mut limits = ResourceLimits::dev_defaults();
        limits.max_in_flight_requests = max_pins;
        FileStore::open(
            &self.path,
            limits,
            vec![TenantPolicy::new(
                self.auth.tenant.clone(),
                self.auth.trust_domain.clone(),
                self.auth.credential_fingerprint,
            )
            .unwrap()],
        )
        .unwrap()
    }

    fn seed(&self, store: &mut FileStore, family: u16, payload: &[u8]) -> u64 {
        let header = |n| ItemHeader {
            request_id: RequestId::from_counter(n).unwrap(),
            epoch: store.epoch(),
            deadline_ms: u64::MAX,
        };
        let reserve = header(u128::from(family) * 2);
        let commit = header(u128::from(family) * 2 + 1);
        let generation = store
            .reserve_put(
                &self.auth,
                0,
                vec![ReservePutItem {
                    header: reserve,
                    identity: synthetic_identity(1, family),
                    generation: 0,
                    length: payload.len() as u64,
                    checksum: content_checksum(payload),
                    payload: Some(payload.to_vec()),
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
                    header: commit,
                    generation,
                    length: payload.len() as u64,
                    checksum: content_checksum(payload),
                }],
            )
            .unwrap()
            .items[0]
            .body
            .as_ref()
            .unwrap();
        generation
    }

    fn serve(&self, store: FileStore) -> (ServerHandle, TcpClient) {
        let server = serve(
            store,
            ServerConfig {
                max_frame_bytes: FRAME_BYTES,
                ..ServerConfig::default()
            },
        )
        .unwrap();
        let client = TcpClient::connect(
            server.addr(),
            self.auth.clone(),
            Duration::from_secs(5),
            FRAME_BYTES,
        )
        .unwrap();
        (server, client)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn header(client: &TcpClient, n: u128) -> ItemHeader {
    ItemHeader {
        request_id: RequestId::from_counter(n).unwrap(),
        epoch: client.epoch(),
        deadline_ms: client.deadline(30_000),
    }
}

fn get(client: &TcpClient, n: u128, family: u16, generation: u64) -> GetItem {
    GetItem {
        header: header(client, n),
        identity: synthetic_identity(1, family),
        generation,
        pin_ttl_ms: 30_000,
    }
}

fn release(client: &mut TcpClient, n: u128, owner: u128, generation: u64, region_id: u64) {
    client
        .release_cancel(vec![ReleaseCancelItem {
            header: header(client, n),
            generation,
            region_id: Some(region_id),
            pin_owner: Some(RequestId::from_counter(owner).unwrap()),
            attestation: Attestation::Drained,
        }])
        .unwrap()
        .items[0]
        .body
        .as_ref()
        .unwrap();
}

#[test]
fn oversized_get_creates_no_pin_and_keeps_the_tcp_connection_synchronized() {
    let fixture = Fixture::new();
    let mut store = fixture.open(1);
    // A successful single-item GCKR reply adds exactly 105 metadata bytes.
    let too_large = fixture.seed(&mut store, 1, &vec![1; FRAME_BYTES - 104]);
    let exact = fixture.seed(&mut store, 2, &vec![2; FRAME_BYTES - 105]);
    assert_eq!(store.stats().reader_pins, 0);
    let (_server, mut client) = fixture.serve(store);

    for n in 100..103 {
        let denied = client.get(vec![get(&client, n, 1, too_large)]).unwrap();
        assert_eq!(
            denied.items[0].body,
            Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            })
        );
    }
    assert!(matches!(
        client
            .lookup(vec![LookupItem {
                header: header(&client, 103),
                identity: synthetic_identity(1, 2),
            }])
            .unwrap()
            .items[0]
            .body,
        Ok(LookupOutcome::Hit { generation, .. }) if generation == exact
    ));
    // With a global ceiling of one, this success proves that no rejected Get
    // retained a hidden pin. Its reply fills the frame exactly.
    let hit = client.get(vec![get(&client, 104, 2, exact)]).unwrap().items[0]
        .body
        .clone()
        .unwrap();
    assert_eq!(hit.payload, vec![2; FRAME_BYTES - 105]);
    release(&mut client, 105, 104, exact, hit.region_id);
    assert!(
        client.get(vec![get(&client, 106, 2, exact)]).unwrap().items[0]
            .body
            .is_ok()
    );
}

#[test]
fn multi_item_get_accounts_for_every_header_before_creating_pins() {
    let fixture = Fixture::new();
    let mut store = fixture.open(2);
    // Two successful Get items add 20 + 2 * 85 = 190 metadata bytes.
    let exact = fixture.seed(&mut store, 1, &vec![1; (FRAME_BYTES - 190) / 2]);
    let over = fixture.seed(&mut store, 2, &vec![2; (FRAME_BYTES - 190) / 2 + 1]);
    let (_server, mut client) = fixture.serve(store);

    let reply = client
        .get(vec![get(&client, 100, 2, over), get(&client, 101, 2, over)])
        .unwrap();
    let hit = reply.items[0].body.as_ref().unwrap();
    assert_eq!(
        reply.items[1].body,
        Err(CacheError::QuotaExceeded {
            kind: QuotaKind::InFlightBytes,
        })
    );
    release(&mut client, 102, 100, over, hit.region_id);

    // Both credits remain usable: the rejected second item did not pin.
    // The complete two-item reply is exactly FRAME_BYTES, not just its payload.
    let reply = client
        .get(vec![
            get(&client, 103, 1, exact),
            get(&client, 104, 1, exact),
        ])
        .unwrap();
    for (index, owner) in [103, 104].into_iter().enumerate() {
        let hit = reply.items[index].body.as_ref().unwrap();
        assert_eq!(hit.payload, vec![1; (FRAME_BYTES - 190) / 2]);
        release(
            &mut client,
            105 + index as u128,
            owner,
            exact,
            hit.region_id,
        );
    }
}
