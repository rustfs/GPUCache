// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! TCP service for the V1 B shared KV tier.
//!
//! The listener defaults to loopback and rejects a start with no tenant policy.
//! Frames are length-delimited. The GCK1 request bytes are unchanged. Replies
//! use GCKR. There is no RDMA, CUDA, or DPU dependency in this crate.

#![forbid(unsafe_code)]

mod client;
mod clock;
mod demo;
mod frame;
mod pool;
mod server;

pub use client::TcpClient;
pub use demo::{run_share_demo, ShareReport};
pub use server::{serve, ServerConfig, ServerHandle};
