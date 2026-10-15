// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Versioned cache identity, key codec, block lifecycle, and errors.
//!
//! This crate is the CPU contract for the G3.5 shared KV tier. It does not
//! link CUDA, DOCA, RDMA, or a DPU SDK, and it does not treat a KV block as
//! model-agnostic long-term memory.

#![forbid(unsafe_code)]

mod checksum;
mod codec;
mod error;
mod identity;
mod key;
mod lifecycle;
mod limits;

#[cfg(test)]
mod props;

pub use checksum::{content_checksum, verify_checksum, Checksum};
pub use codec::{ByteReader, ByteWriter, HARD_BYTES_CAP};
pub use error::{CacheError, QuotaKind};
pub use identity::{
    logical_fields_diverge, AdapterIdentity, CacheIdentity, Digest, KvDtype, LayoutAbi,
    MultimodalIdentity, PositionEncoding, QuantFormat, RopeConfig, RopeScaling, ShardPlacement,
    TrustDomain, IDENTITY_VERSION,
};
pub use key::{
    decode_logical_key, decode_physical_key, encode_logical_key, encode_physical_key,
    LogicalPrefix, KEY_VERSION, LOGICAL_MAGIC, PHYSICAL_MAGIC,
};
pub use lifecycle::{Attestation, BlockState, Completion, Effect, Event, Lifecycle};
pub use limits::{IdentityLimits, ResourceLimits};

/// Opaque tenant identifier. Callers must pass an id, never a prompt.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TenantId(Vec<u8>);

impl TenantId {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, CacheError> {
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > limits::DEFAULT_MAX_FIELD_BYTES as usize {
            return Err(CacheError::IdentityInvalid {
                reason: "tenant id length",
            });
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for TenantId {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Client-generated idempotency key. All-zero is rejected.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RequestId(pub [u8; 16]);

impl RequestId {
    pub fn new(bytes: [u8; 16]) -> Result<Self, CacheError> {
        if bytes == [0u8; 16] {
            return Err(CacheError::Malformed {
                reason: "request_id must not be all zeros",
            });
        }
        Ok(Self(bytes))
    }

    pub fn from_counter(n: u128) -> Result<Self, CacheError> {
        Self::new(n.to_le_bytes())
    }
}

/// Fixed synthetic identity used by unit tests and the CPU transfer tool.
///
/// These bytes are not a vLLM KV block. A real engine export remains
/// `NOT_RUN_HARDWARE` until it is executed on a GPU.
pub fn synthetic_identity(nonce: u8, layout_family: u16) -> CacheIdentity {
    let mut model = [0x11u8; 32];
    model[0] = nonce;
    let mut tokenizer = [0x22u8; 32];
    tokenizer[0] = nonce;
    let mut config = [0x33u8; 32];
    config[0] = nonce;
    let mut link = [0x44u8; 32];
    link[0] = nonce;
    link[1] = layout_family as u8;
    CacheIdentity {
        version: IDENTITY_VERSION,
        tenant: TenantId::new(b"tenant-a".to_vec()).expect("fixture tenant"),
        trust_domain: TrustDomain::new(b"td-local".to_vec()).expect("fixture trust"),
        model_revision: Digest(model),
        model_config_hash: Digest(config),
        tokenizer_revision: Digest(tokenizer),
        adapter: AdapterIdentity::None,
        kv_dtype: KvDtype::Bf16,
        quant_format: QuantFormat::None,
        layout: LayoutAbi {
            family: layout_family,
            major: 1,
            minor: 0,
        },
        shard: ShardPlacement {
            tp_size: 1,
            tp_rank: 0,
            pp_size: 1,
            pp_rank: 0,
        },
        position: PositionEncoding::Rope,
        rope: RopeConfig {
            theta_bits: 1_000_000.0f64.to_bits(),
            scaling: RopeScaling::None,
            scaling_factor_bits: 0,
            max_position_embeddings: 4096,
        },
        prefix_chain: vec![Digest(link)],
        block_token_count: 16,
        valid_length: 16,
        multimodal: None,
    }
}
