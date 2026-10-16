// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! TCP service for the shared KV tier, plus the V2 control-plane types.
//!
//! The listener defaults to loopback and rejects a start with no tenant policy.
//! Frames are length-delimited. The GCK1 request bytes are unchanged. Replies
//! use GCKR. Feature `rdma-nixl` does not link libnixl. Live RDMA is
//! `NOT_RUN_HARDWARE`.

#![forbid(unsafe_code)]

mod client;
mod clock;
mod demo;
mod directory;
mod fault_demo;
mod frame;
mod iface;
mod plane;
mod pool;
mod rate;
mod registered;
mod server;
mod shard_demo;
mod stage;

pub use client::TcpClient;
pub use demo::{run_share_demo, ShareReport};
pub use directory::{Admit, FillNotice, FillRequest, MemberView, ShardDirectory, ShardParams};
pub use fault_demo::{run_fault_demo, FaultReport};
pub use iface::{
    rdma_feature_selected, rdma_probe, submit_preferring_rdma, FallbackReason, HardwarePath,
    KvTransport, NixlTransport, TcpTransport, TransferDesc, TransferReceipt, TransportKind,
};
pub use plane::{ControlPlane, RebalancePlan, UpgradeStamp};
pub use rate::RateGate;
pub use registered::{CheckedBatch, PoolStats, RegisteredPool, SimCompletion, SlotId};
pub use server::{serve, ServerConfig, ServerHandle};
pub use shard_demo::{run_shard_demo, ShardReport};
pub use stage::stage_through_tcp;
