// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Synthetic CPU microbenchmarks, not inference or NIC performance.
use std::hint::black_box;
use std::time::Instant;

use gpucache_core_types::{content_checksum, synthetic_identity, RequestId, ResourceLimits};
use gpucache_protocol::{
    AuthContext, CommitItem, ItemHeader, LookupItem, ReservePutItem, TenantPolicy,
};
use gpucache_storage::FileStore;
use gpucache_transport::{Admit, FillRequest, ShardDirectory, ShardParams};

fn header(id: u128) -> ItemHeader {
    ItemHeader {
        request_id: RequestId::from_counter(id).unwrap(),
        epoch: 1,
        deadline_ms: 1_000_000,
    }
}

fn main() {
    println!("scope=CPU_SYNTHETIC hardware=NOT_RUN_HARDWARE tco=TCO_UNKNOWN");
    let root = std::env::temp_dir().join(format!("gpucache-audit-bench-{}", std::process::id()));
    std::fs::create_dir(&root).expect("fresh benchmark directory");
    let identity = synthetic_identity(1, 1);
    let auth = AuthContext::new(
        identity.tenant.clone(),
        identity.trust_domain.clone(),
        [9; 32],
    )
    .unwrap();
    let policy = TenantPolicy::new(
        identity.tenant.clone(),
        identity.trust_domain.clone(),
        [9; 32],
    )
    .unwrap();
    let mut store = FileStore::open(&root, ResourceLimits::dev_defaults(), vec![policy]).unwrap();
    let checksum = content_checksum(b"synthetic");
    let mut identities = Vec::new();
    for n in 0..128u128 {
        let mut id = identity.clone();
        id.prefix_chain[0].0[..16].copy_from_slice(&n.to_le_bytes());
        let reserved = store
            .reserve_put(
                &auth,
                0,
                vec![ReservePutItem {
                    header: header(2 * n + 1),
                    identity: id.clone(),
                    generation: 0,
                    length: 9,
                    checksum,
                    payload: Some(b"synthetic".to_vec()),
                    lease_ttl_ms: 1000,
                }],
            )
            .unwrap();
        let generation = reserved.items[0].body.as_ref().unwrap().generation;
        store
            .commit(
                &auth,
                0,
                vec![CommitItem {
                    header: header(2 * n + 2),
                    generation,
                    length: 9,
                    checksum,
                }],
            )
            .unwrap()
            .items[0]
            .body
            .as_ref()
            .unwrap();
        identities.push(id);
    }
    let start = Instant::now();
    for n in 0..512 {
        black_box(
            store
                .lookup(
                    &auth,
                    0,
                    vec![LookupItem {
                        header: header(10_000 + n as u128),
                        identity: identities[n % 128].clone(),
                    }],
                )
                .unwrap(),
        );
    }
    println!(
        "workload=file_lookup blocks=128 operations=512 elapsed_ns={}",
        start.elapsed().as_nanos()
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();

    let mut dir =
        ShardDirectory::single_node(ShardParams::new(1_000_000, 1_000_000, 8192, 4, 4).unwrap());
    let requests: Vec<_> = (1..=8192u128)
        .map(|n| FillRequest {
            physical_key: n.to_le_bytes().to_vec(),
            tenant: b"tenant-a".to_vec(),
            bytes: 1,
            request: n.to_le_bytes(),
            epoch: 1,
        })
        .collect();
    let mut ids = Vec::new();
    for req in &requests {
        let Admit::Leader { flight_id, .. } = dir.begin_or_join(req).unwrap() else {
            panic!("leader")
        };
        ids.push(flight_id);
    }
    let start = Instant::now();
    for n in 0..8192 {
        black_box(dir.begin_or_join(&requests[n * 7919 % 8192]).unwrap());
    }
    println!(
        "workload=directory_replay flights=8192 operations=8192 elapsed_ns={}",
        start.elapsed().as_nanos()
    );
    let start = Instant::now();
    for n in 0..8192 {
        dir.complete_flight(ids[n], 1, requests[n].request).unwrap();
    }
    println!(
        "workload=directory_complete flights=8192 operations=8192 elapsed_ns={}",
        start.elapsed().as_nanos()
    );
    assert_eq!(dir.inflight(), 0);
    assert_eq!(dir.credits_used(), 0);
}
