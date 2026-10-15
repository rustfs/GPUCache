// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::codec::{ByteReader, ByteWriter};
use crate::error::CacheError;
use crate::identity::{AdapterIdentity, CacheIdentity, Digest, MultimodalIdentity};
use crate::limits::IdentityLimits;
use crate::{TenantId, TrustDomain};

pub const KEY_VERSION: u16 = 1;
pub const PHYSICAL_MAGIC: &[u8; 4] = b"GPCK";
pub const LOGICAL_MAGIC: &[u8; 4] = b"GPCL";

/// Logical prefix key. It identifies content, not the bytes of a layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalPrefix {
    pub version: u16,
    pub tenant: TenantId,
    pub trust_domain: TrustDomain,
    pub model_revision: Digest,
    pub model_config_hash: Digest,
    pub tokenizer_revision: Digest,
    pub adapter: AdapterIdentity,
    pub prefix_chain: Vec<Digest>,
    pub multimodal: Option<MultimodalIdentity>,
}

impl LogicalPrefix {
    pub fn from_identity(id: &CacheIdentity) -> Self {
        Self {
            version: id.version,
            tenant: id.tenant.clone(),
            trust_domain: id.trust_domain.clone(),
            model_revision: id.model_revision,
            model_config_hash: id.model_config_hash,
            tokenizer_revision: id.tokenizer_revision,
            adapter: id.adapter,
            prefix_chain: id.prefix_chain.clone(),
            multimodal: id.multimodal,
        }
    }
}

/// Physical byte key. Same tokens do not imply the same bytes.
pub fn encode_physical_key(
    id: &CacheIdentity,
    limits: &IdentityLimits,
) -> Result<Vec<u8>, CacheError> {
    id.validate(limits)?;
    encode_key(PHYSICAL_MAGIC, &id.encode_body(false)?)
}

pub fn encode_logical_key(
    id: &CacheIdentity,
    limits: &IdentityLimits,
) -> Result<Vec<u8>, CacheError> {
    id.validate(limits)?;
    encode_key(LOGICAL_MAGIC, &id.encode_body(true)?)
}

pub fn decode_physical_key(bytes: &[u8]) -> Result<CacheIdentity, CacheError> {
    let body = strip_key(bytes, PHYSICAL_MAGIC)?;
    CacheIdentity::decode_body(&body, false)
}

pub fn decode_logical_key(bytes: &[u8]) -> Result<LogicalPrefix, CacheError> {
    let body = strip_key(bytes, LOGICAL_MAGIC)?;
    let id = CacheIdentity::decode_body(&body, true)?;
    Ok(LogicalPrefix::from_identity(&id))
}

fn encode_key(magic: &[u8; 4], body: &[u8]) -> Result<Vec<u8>, CacheError> {
    let mut w = ByteWriter::with_capacity(8 + body.len());
    w.raw(magic);
    w.u16(KEY_VERSION);
    w.bytes(body)?;
    Ok(w.finish())
}

fn strip_key(bytes: &[u8], magic: &[u8; 4]) -> Result<Vec<u8>, CacheError> {
    let mut r = ByteReader::new(bytes);
    let got = r.raw(4)?;
    if got != magic {
        return Err(CacheError::Malformed {
            reason: "key magic",
        });
    }
    let version = r.u16()?;
    if version != KEY_VERSION {
        return Err(CacheError::IdentityInvalid {
            reason: "key version",
        });
    }
    let body = r.bytes(1024 * 1024)?.to_vec();
    r.finish()?;
    Ok(body)
}
