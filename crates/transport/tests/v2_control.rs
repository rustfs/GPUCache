// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration coverage for the V2 control plane. No GPU, NIC, or RustFS.

use gpucache_core_types::{CacheError, QuotaKind};
use gpucache_transport::{
    rdma_probe, stage_through_tcp, Admit, FillRequest, NixlTransport, RegisteredPool,
    ShardDirectory, ShardParams, TcpTransport, TransportKind,
};

#[test]
fn directory_pool_and_tcp_compose_without_a_device() {
    let mut dir = ShardDirectory::single_node(ShardParams::new(64, 64, 4, 4, 2).unwrap());
    let epoch = dir.epoch();
    let leader = dir
        .begin_or_join(&FillRequest {
            physical_key: b"block-physical".to_vec(),
            tenant: b"tenant-a".to_vec(),
            bytes: 16,
            request: 7u128.to_le_bytes(),
            epoch,
        })
        .unwrap();
    assert!(matches!(leader, Admit::Leader { .. }));
    let mut pool = RegisteredPool::new(2, 8).unwrap();
    let mut tcp = TcpTransport::new(32).unwrap();
    let receipt = stage_through_tcp(&mut pool, &mut tcp, &[b"aa", b"bb"]).unwrap();
    assert_eq!(receipt.kind, TransportKind::Tcp);
    assert!(!receipt.dma_posted);
    assert_eq!(pool.stats().checked_out, 0);
    assert_eq!(rdma_probe(), "NOT_RUN_HARDWARE");
    assert!(matches!(
        NixlTransport::open(),
        Err(CacheError::Storage {
            reason: "rdma not linked"
        })
    ));
}

#[test]
fn feature_slot_is_independent_of_a_linked_library() {
    let selected = gpucache_transport::rdma_feature_selected();
    if cfg!(feature = "rdma-nixl") {
        assert!(selected);
    } else {
        assert!(!selected);
    }
    let err = NixlTransport::open().unwrap_err();
    assert_ne!(
        err,
        CacheError::QuotaExceeded {
            kind: QuotaKind::Registrations,
        }
    );
}
