// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! One process, independent localhost TCP services, synthetic whole blocks.
//! This exercises orchestration; it is not discovery, migration, or a benchmark.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpucache_core_types::{
    content_checksum, encode_physical_key, synthetic_identity, Attestation, BlockState, CacheError,
    CacheIdentity, IdentityLimits, RequestId, ResourceLimits,
};
use gpucache_protocol::{
    AuthContext, BatchReply, CommitItem, GetItem, ItemHeader, LookupItem, LookupOutcome,
    ReleaseCancelItem, ReservePutItem, TenantPolicy,
};
use gpucache_storage::FileStore;
use gpucache_transport::{
    serve, ClientLimits, ServerConfig, ServerHandle, ShardEndpoint, ShardSession, ShardedClient,
    TcpClient,
};

const COUNTS: [usize; 3] = [1, 3, 10];
const MAX_PREPARE_OPENS: usize = 3;
const FRAME_BYTES: usize = 1024 * 1024;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "gpucache-scaling-smoke-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::other("no unused scaling smoke directory"))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Report {
    roundtrips: usize,
    transitions: usize,
    stages: Vec<Stage>,
}

#[derive(Debug, PartialEq, Eq)]
struct Stage {
    count: usize,
    epoch: u64,
    revision: u64,
    verified_shards: Vec<u64>,
}

struct Requests(u128);
impl Requests {
    fn header(&mut self, session: &ShardSession) -> Result<ItemHeader, CacheError> {
        self.at(session.epoch(), session.deadline(10_000))
    }

    fn at(&mut self, epoch: u64, deadline_ms: u64) -> Result<ItemHeader, CacheError> {
        let request_id = RequestId::from_counter(self.0)?;
        self.0 = self
            .0
            .checked_add(1)
            .ok_or(CacheError::IdempotencyConflict)?;
        Ok(ItemHeader {
            request_id,
            epoch,
            deadline_ms,
        })
    }
}

fn auth() -> Result<AuthContext, CacheError> {
    let identity = synthetic_identity(1, 1);
    // Public, synthetic test credential. Never use this fingerprint remotely.
    AuthContext::new(identity.tenant, identity.trust_domain, [9; 32])
}

fn policy() -> Result<TenantPolicy, CacheError> {
    let auth = auth()?;
    TenantPolicy::new(auth.tenant, auth.trust_domain, auth.credential_fingerprint)
}

fn check(ok: bool, reason: &'static str) -> Result<(), CacheError> {
    if ok {
        Ok(())
    } else {
        Err(CacheError::Storage { reason })
    }
}

fn one<T>(batch: BatchReply<T>) -> Result<T, CacheError> {
    check(batch.items.len() == 1, "scaling reply length")?;
    batch
        .items
        .into_iter()
        .next()
        .ok_or(CacheError::UnknownGeneration)?
        .body
}

/// Only called while this demo owns the directory exclusively and every old
/// service has stopped and joined. It never edits a MANIFEST or resets an epoch.
/// Fresh roots need two opens for epoch 2 and three opens for epoch 3. The demo
/// has a fixed three-open bound, not an unbounded production epoch reconciler.
fn prepare_store(root: &Path, target: u64) -> Result<FileStore, CacheError> {
    check((1..=3).contains(&target), "scaling target epoch")?;
    for _ in 0..MAX_PREPARE_OPENS {
        let store = FileStore::open(root, ResourceLimits::dev_defaults(), vec![policy()?])?;
        if store.epoch() == target {
            return Ok(store);
        }
        if store.epoch() > target {
            return Err(CacheError::EpochMismatch {
                store: store.epoch(),
                request: target,
            });
        }
        drop(store);
    }
    Err(CacheError::FencingRejected {
        reason: "scaling epoch preparation",
    })
}

fn start_servers(
    root: &Path,
    count: usize,
    epoch: u64,
) -> Result<(Vec<ServerHandle>, Vec<ShardEndpoint>), CacheError> {
    let mut servers = Vec::with_capacity(count);
    let mut routes = Vec::with_capacity(count);
    for shard in 0..count {
        let store = prepare_store(&root.join(format!("shard-{shard}")), epoch)?;
        check(store.epoch() == epoch, "scaling store epoch")?;
        let server = serve(store, ServerConfig::default())?;
        routes.push(ShardEndpoint {
            shard_id: shard as u64,
            address: server.addr(),
            epoch,
        });
        servers.push(server);
    }
    Ok((servers, routes))
}

fn identity(nonce: u64) -> CacheIdentity {
    let mut id = synthetic_identity(1, 1);
    id.prefix_chain[0].0[..8].copy_from_slice(&nonce.to_le_bytes());
    id
}

fn identity_on(
    client: &ShardedClient,
    shard: u64,
    first: u64,
) -> Result<CacheIdentity, CacheError> {
    for nonce in first..first + 10_000 {
        let id = identity(nonce);
        if client.shard_for(&id)? == shard {
            return Ok(id);
        }
    }
    Err(CacheError::Storage {
        reason: "scaling identity search exhausted",
    })
}

fn payload(id: &CacheIdentity) -> Result<Vec<u8>, CacheError> {
    let key = encode_physical_key(
        id,
        &IdentityLimits::from_resources(&ResourceLimits::dev_defaults()),
    )?;
    // Distinct synthetic bytes per identity; never an engine KV tensor.
    Ok(content_checksum(&key).0.to_vec())
}

fn lookup(
    session: &mut ShardSession,
    id: &CacheIdentity,
    requests: &mut Requests,
) -> Result<LookupOutcome, CacheError> {
    one(session.lookup(vec![LookupItem {
        header: requests.header(session)?,
        identity: id.clone(),
    }])?)
}

fn reserve(
    session: &mut ShardSession,
    id: &CacheIdentity,
    requests: &mut Requests,
) -> Result<u64, CacheError> {
    let bytes = payload(id)?;
    Ok(one(session.reserve_put(vec![ReservePutItem {
        header: requests.header(session)?,
        identity: id.clone(),
        generation: 0,
        length: bytes.len() as u64,
        checksum: content_checksum(&bytes),
        payload: Some(bytes),
        lease_ttl_ms: 10_000,
    }])?)?
    .generation)
}

fn write_and_read(
    session: &mut ShardSession,
    id: &CacheIdentity,
    requests: &mut Requests,
) -> Result<u64, CacheError> {
    let bytes = payload(id)?;
    let generation = reserve(session, id, requests)?;
    one(session.commit(vec![CommitItem {
        header: requests.header(session)?,
        generation,
        length: bytes.len() as u64,
        checksum: content_checksum(&bytes),
    }])?)?;
    match lookup(session, id, requests)? {
        LookupOutcome::Hit {
            generation: got,
            length,
            checksum,
            ..
        } if got == generation
            && length == bytes.len() as u64
            && checksum == content_checksum(&bytes) => {}
        _ => return Err(CacheError::Incompatible),
    }
    let header = requests.header(session)?;
    let pin_owner = header.request_id;
    let hit = one(session.get(vec![GetItem {
        header,
        generation,
        identity: id.clone(),
        pin_ttl_ms: 10_000,
    }])?)?;
    check(hit.payload == bytes, "scaling payload mismatch")?;
    let released = one(session.release_cancel(vec![ReleaseCancelItem {
        header: requests.header(session)?,
        generation,
        region_id: Some(hit.region_id),
        pin_owner: Some(pin_owner),
        attestation: Attestation::Drained,
    }])?)?;
    check(
        released.state == BlockState::Committed && !released.quarantined,
        "scaling pin release",
    )?;
    Ok(generation)
}

fn predictor(count: usize, epoch: u64) -> Result<ShardedClient, CacheError> {
    // Pure placement calculation: new() and shard_for() open no sockets. Real
    // listening ports are selected later and do not affect rendezvous placement.
    let routes = (0..count)
        .map(|shard| ShardEndpoint {
            shard_id: shard as u64,
            address: ([127, 0, 0, 1], 9735 + shard as u16).into(),
            epoch,
        })
        .collect();
    ShardedClient::new(auth()?, routes, ClientLimits::default())
}

fn run_scaling_smoke() -> Result<Report, Box<dyn std::error::Error>> {
    let scratch = Scratch::new()?;
    let (mut servers, routes) = start_servers(&scratch.0, COUNTS[0], 1)?;
    let client = ShardedClient::new(auth()?, routes, ClientLimits::default())?;
    let mut requests = Requests(1);
    let mut report = Report {
        roundtrips: 0,
        transitions: 0,
        stages: Vec::new(),
    };

    for (phase, &count) in COUNTS.iter().enumerate() {
        if phase > 0 {
            let target_epoch = phase as u64 + 1;
            let next = predictor(count, target_epoch)?;
            let mut moved = None;
            for nonce in 500_000 + phase as u64 * 10_000..500_000 + phase as u64 * 10_000 + 10_000 {
                let id = identity(nonce);
                let old_shard = client.shard_for(&id)?;
                if next.shard_for(&id)? != old_shard {
                    moved = Some((id, old_shard));
                    break;
                }
            }
            let (moved_id, old_shard) = moved.ok_or(CacheError::Storage {
                reason: "scaling remap search exhausted",
            })?;
            let mut stale = client.route(&moved_id)?;
            write_and_read(&mut stale, &moved_id, &mut requests)?;
            let pending_id = identity_on(&client, old_shard, 900_000 + phase as u64 * 10_000)?;
            let pending_generation = reserve(&mut stale, &pending_id, &mut requests)?;

            // All calls are synchronous and every Get above was drained. There
            // is no other client: admission is paused here, including reads.
            // Dropping every handle stops and joins its connection workers.
            // This is explicit orchestration, not a remotely triggered fence.
            drop(servers);
            let (started, replacement) = start_servers(&scratch.0, count, target_epoch)?;
            servers = started;
            let old_address = replacement
                .iter()
                .find(|route| route.shard_id == old_shard)
                .ok_or(CacheError::UnknownGeneration)?
                .address;
            let old_revision = client.stats().revision;
            check(
                client.replace_routes(old_revision, replacement)? == old_revision + 1,
                "scaling route revision",
            )?;
            check(
                matches!(
                    lookup(&mut stale, &moved_id, &mut requests),
                    Err(CacheError::FencingRejected {
                        reason: "route revision"
                    })
                ),
                "scaling stale lookup",
            )?;
            let pending_bytes = payload(&pending_id)?;
            let commit = stale.commit(vec![CommitItem {
                header: requests.header(&stale)?,
                generation: pending_generation,
                length: pending_bytes.len() as u64,
                checksum: content_checksum(&pending_bytes),
            }]);
            check(
                matches!(
                    commit,
                    Err(CacheError::FencingRejected {
                        reason: "route revision"
                    })
                ),
                "scaling stale commit",
            )?;
            drop(stale);

            // The old committed copy remains on its original store. Inspect it
            // directly; shard sessions intentionally refuse wrong-route reads.
            let mut original =
                TcpClient::connect(old_address, auth()?, Duration::from_secs(5), FRAME_BYTES)?;
            let old_copy = one(original.lookup(vec![LookupItem {
                header: requests.at(original.epoch(), original.deadline(10_000))?,
                identity: moved_id.clone(),
            }])?)?;
            check(
                matches!(old_copy, LookupOutcome::Hit { .. }),
                "scaling original committed copy",
            )?;
            let pending_copy = one(original.lookup(vec![LookupItem {
                header: requests.at(original.epoch(), original.deadline(10_000))?,
                identity: pending_id.clone(),
            }])?)?;
            check(
                matches!(pending_copy, LookupOutcome::Miss { .. }),
                "scaling original uncommitted copy",
            )?;
            let old_epoch = one(original.lookup(vec![LookupItem {
                header: requests.at(target_epoch - 1, original.deadline(10_000))?,
                identity: moved_id.clone(),
            }])?);
            check(
                matches!(old_epoch, Err(CacheError::EpochMismatch { store, request })
                    if store == target_epoch && request == target_epoch - 1),
                "scaling remote old epoch",
            )?;
            drop(original);
            let mut new_owner = client.route(&moved_id)?;
            check(new_owner.shard_id() != old_shard, "scaling expected remap")?;
            check(
                matches!(
                    lookup(&mut new_owner, &moved_id, &mut requests)?,
                    LookupOutcome::Miss { .. }
                ),
                "scaling must not migrate",
            )?;
            // Simulated recomputation repopulates the new owner explicitly.
            write_and_read(&mut new_owner, &moved_id, &mut requests)?;
            drop(new_owner);
            let mut pending_owner = client.route(&pending_id)?;
            check(
                matches!(
                    lookup(&mut pending_owner, &pending_id, &mut requests)?,
                    LookupOutcome::Miss { .. }
                ),
                "scaling uncommitted block survived",
            )?;
            report.transitions += 1;
        }
        let mut stage = Stage {
            count,
            epoch: phase as u64 + 1,
            revision: client.stats().revision,
            verified_shards: Vec::new(),
        };
        for shard in 0..count {
            let id = identity_on(
                &client,
                shard as u64,
                phase as u64 * 100_000 + shard as u64 * 10_000,
            )?;
            let mut session = client.route(&id)?;
            check(
                session.shard_id() == shard as u64 && session.epoch() == phase as u64 + 1,
                "scaling session placement",
            )?;
            check(
                matches!(
                    lookup(&mut session, &id, &mut requests)?,
                    LookupOutcome::Miss { .. }
                ),
                "scaling initial miss",
            )?;
            write_and_read(&mut session, &id, &mut requests)?;
            report.roundtrips += 1;
            stage.verified_shards.push(shard as u64);
        }
        check(client.stats().shards == count, "scaling shard count")?;
        check(
            client.stats().connections <= count,
            "scaling connection count",
        )?;
        report.stages.push(stage);
    }
    drop(client);
    drop(servers);
    Ok(report)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let report = run_scaling_smoke()?;
    println!("status=SCALING_SMOKE_OK");
    println!("topology=SINGLE_PROCESS_LOCALHOST");
    for stage in &report.stages {
        println!(
            "stage_shards={} store_epoch={} local_revision={}",
            stage.count, stage.epoch, stage.revision
        );
        if stage.epoch > 1 {
            println!("transition_to_shards={} stale_session=REJECTED stale_commit=REJECTED old_epoch=REJECTED uncommitted=MISS remapped=MISS_THEN_RECOMPUTE original_copy=RETAINED", stage.count);
        }
        for shard in &stage.verified_shards {
            println!(
                "shard={} epoch={} lookup_put_commit_get_drained=OK",
                shard, stage.epoch
            );
        }
    }
    println!("shard_counts=1,3,10");
    println!("verified_store_epochs=1,2,3");
    println!("per_shard_roundtrips={}", report.roundtrips);
    println!("transition_fences={}", report.transitions);
    println!("stale_sessions=REJECTED");
    println!("inflight_puts=FENCED_MISS");
    println!("remapped_blocks=MISS_THEN_RECOMPUTE");
    println!("automatic_payload_migration=NONE");
    println!("durability_replication=NONE");
    println!("engine_blocks=SYNTHETIC");
    println!("hardware=NOT_RUN_HARDWARE");
    println!("performance=NOT_A_PERFORMANCE_RESULT");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_three_ten_localhost_services_fence_and_recompute_without_migration() {
        let report = run_scaling_smoke().unwrap();
        assert_eq!(report.roundtrips, 14);
        assert_eq!(report.transitions, 2);
        assert_eq!(report.stages.len(), COUNTS.len());
        for (phase, stage) in report.stages.iter().enumerate() {
            assert_eq!(stage.count, COUNTS[phase]);
            assert_eq!(stage.epoch, phase as u64 + 1);
            assert_eq!(stage.revision, phase as u64 + 1);
            assert_eq!(
                stage.verified_shards,
                (0..COUNTS[phase] as u64).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn epoch_preparation_refuses_to_reset_or_run_without_a_bound() {
        let scratch = Scratch::new().unwrap();
        let root = scratch.0.join("epoch");
        assert!(prepare_store(&root, 0).is_err());
        assert!(prepare_store(&root, 4).is_err());
        let store = prepare_store(&root, 3).unwrap();
        assert_eq!(store.epoch(), 3);
        drop(store);
        assert!(matches!(
            prepare_store(&root, 2),
            Err(CacheError::EpochMismatch {
                store: 4,
                request: 2
            })
        ));
    }
}
