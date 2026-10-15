// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Batch messages, deadlines, and wire-version negotiation for GPUCache.
//!
//! V1 speaks this contract over an in-process API. A TCP listener is a later
//! milestone. Nothing here depends on a GPU, RDMA stack, or DPU.

#![forbid(unsafe_code)]

mod api;
mod auth;
mod codec;
mod negotiate;

pub use api::{
    BatchReply, CommitItem, CommitOutcome, GetHit, GetItem, InvalidateItem, InvalidateOutcome,
    ItemHeader, ItemResult, LookupItem, LookupOutcome, MissReason, ReleaseCancelItem,
    ReleaseOutcome, RequestBody, ReserveOutcome, ReservePutItem,
};
pub use auth::{fingerprints_equal, AuthContext, TenantPolicy};
pub use codec::{
    decode_export, decode_hello, decode_hello_ack, decode_request, encode_export, encode_hello,
    encode_hello_ack, encode_request, ensure_deadline, CommittedExport, EXPORT_VERSION, WIRE_MAGIC,
};
pub use negotiate::{negotiate, HELLO_OP, WIRE_VERSION};

pub use gpucache_core_types::Attestation;
