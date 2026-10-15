// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Batch messages, deadlines, and wire-version negotiation for GPUCache.
//!
//! V1 A defined the request frames. V1 B adds reply frames and a TCP listener
//! in `gpucache-transport`. Nothing here depends on a GPU, RDMA stack, or DPU.

#![forbid(unsafe_code)]

mod api;
mod auth;
mod codec;
mod negotiate;
mod reply;

#[cfg(test)]
mod fuzz;

pub use api::{
    identity_for_prefix_hit, BatchReply, CommitItem, CommitOutcome, GetHit, GetItem,
    InvalidateItem, InvalidateOutcome, ItemHeader, ItemResult, LookupItem, LookupOutcome,
    MissReason, ReleaseCancelItem, ReleaseOutcome, RequestBody, ReserveOutcome, ReservePutItem,
};
pub use auth::{fingerprints_equal, AuthContext, TenantPolicy};
pub use codec::{
    decode_export, decode_hello, decode_hello_ack, decode_request, encode_export, encode_hello,
    encode_hello_ack, encode_request, ensure_deadline, request_op, CommittedExport, EXPORT_VERSION,
    OP_COMMIT, OP_GET, OP_HELLO, OP_HELLO_ACK, OP_INVALIDATE, OP_LOOKUP, OP_RELEASE, OP_RESERVE,
    WIRE_MAGIC,
};
pub use negotiate::{negotiate, HELLO_OP, WIRE_VERSION};
pub use reply::{decode_reply, encode_reply, ReplyBody, REPLY_MAGIC};

pub use gpucache_core_types::Attestation;
