// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::codec::{ByteReader, ByteWriter};
use crate::error::CacheError;
use crate::limits::IdentityLimits;
use crate::TenantId;

pub const IDENTITY_VERSION: u16 = 1;

const TAG_TENANT: u16 = 1;
const TAG_TRUST: u16 = 2;
const TAG_MODEL_REV: u16 = 3;
const TAG_MODEL_CFG: u16 = 4;
const TAG_TOKENIZER: u16 = 5;
const TAG_ADAPTER: u16 = 6;
const TAG_DTYPE: u16 = 7;
const TAG_QUANT: u16 = 8;
const TAG_LAYOUT: u16 = 9;
const TAG_SHARD: u16 = 10;
const TAG_POSITION: u16 = 11;
const TAG_ROPE: u16 = 12;
const TAG_PREFIX: u16 = 13;
const TAG_BLOCK_TOKENS: u16 = 14;
const TAG_VALID_LEN: u16 = 15;
const TAG_MULTIMODAL: u16 = 16;

/// 32-byte digest. Callers hash weights, configs, and token blocks outside this crate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Digest(pub [u8; 32]);

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TrustDomain(Vec<u8>);

impl TrustDomain {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, CacheError> {
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > crate::limits::DEFAULT_MAX_FIELD_BYTES as usize {
            return Err(CacheError::IdentityInvalid {
                reason: "trust domain length",
            });
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum KvDtype {
    F16 = 1,
    Bf16 = 2,
    F32 = 3,
    F8E4M3 = 4,
    F8E5M2 = 5,
    I8 = 6,
}

impl KvDtype {
    fn from_u16(v: u16) -> Result<Self, CacheError> {
        match v {
            1 => Ok(Self::F16),
            2 => Ok(Self::Bf16),
            3 => Ok(Self::F32),
            4 => Ok(Self::F8E4M3),
            5 => Ok(Self::F8E5M2),
            6 => Ok(Self::I8),
            _ => Err(CacheError::IdentityInvalid {
                reason: "unknown kv dtype",
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum QuantFormat {
    None = 1,
    Fp8 = 2,
    Int8 = 3,
}

impl QuantFormat {
    fn from_u16(v: u16) -> Result<Self, CacheError> {
        match v {
            1 => Ok(Self::None),
            2 => Ok(Self::Fp8),
            3 => Ok(Self::Int8),
            _ => Err(CacheError::IdentityInvalid {
                reason: "unknown quant format",
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum PositionEncoding {
    Absolute = 1,
    Rope = 2,
    Alibi = 3,
}

impl PositionEncoding {
    fn from_u16(v: u16) -> Result<Self, CacheError> {
        match v {
            1 => Ok(Self::Absolute),
            2 => Ok(Self::Rope),
            3 => Ok(Self::Alibi),
            _ => Err(CacheError::IdentityInvalid {
                reason: "unknown position encoding",
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum RopeScaling {
    None = 1,
    Linear = 2,
    Dynamic = 3,
}

impl RopeScaling {
    fn from_u16(v: u16) -> Result<Self, CacheError> {
        match v {
            1 => Ok(Self::None),
            2 => Ok(Self::Linear),
            3 => Ok(Self::Dynamic),
            _ => Err(CacheError::IdentityInvalid {
                reason: "unknown rope scaling",
            }),
        }
    }
}

/// RoPE parameters stored as raw IEEE-754 bits so the key does not depend on
/// float formatting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RopeConfig {
    pub theta_bits: u64,
    pub scaling: RopeScaling,
    pub scaling_factor_bits: u64,
    pub max_position_embeddings: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayoutAbi {
    pub family: u16,
    pub major: u16,
    pub minor: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShardPlacement {
    pub tp_size: u16,
    pub tp_rank: u16,
    pub pp_size: u16,
    pub pp_rank: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdapterIdentity {
    None,
    Lora { revision: Digest },
}

/// Hash of multimodal inputs plus the processor revision. Raw media is not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MultimodalIdentity {
    pub processor_revision: Digest,
    pub input_digest: Digest,
}

/// Full identity of one KV block. Fields are hashes and layout parameters.
/// Plaintext prompts and token ids do not belong here.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheIdentity {
    pub version: u16,
    pub tenant: TenantId,
    pub trust_domain: TrustDomain,
    pub model_revision: Digest,
    pub model_config_hash: Digest,
    pub tokenizer_revision: Digest,
    pub adapter: AdapterIdentity,
    pub kv_dtype: KvDtype,
    pub quant_format: QuantFormat,
    pub layout: LayoutAbi,
    pub shard: ShardPlacement,
    pub position: PositionEncoding,
    pub rope: RopeConfig,
    pub prefix_chain: Vec<Digest>,
    pub block_token_count: u32,
    pub valid_length: u32,
    pub multimodal: Option<MultimodalIdentity>,
}

impl CacheIdentity {
    /// Canonical identity body. The store still has to call [`Self::validate`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, CacheError> {
        self.encode_body(false)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CacheError> {
        Self::decode_body(bytes, false)
    }

    /// Validate protocol hard limits independently of tighter store quotas.
    pub fn validate_wire(&self) -> Result<(), CacheError> {
        self.validate(&IdentityLimits {
            max_field_bytes: crate::limits::DEFAULT_MAX_FIELD_BYTES,
            max_prefix_links: 4096,
            max_block_tokens: u32::MAX,
        })
    }

    pub fn validate(&self, limits: &IdentityLimits) -> Result<(), CacheError> {
        if self.version != IDENTITY_VERSION {
            return Err(CacheError::IdentityInvalid {
                reason: "identity version",
            });
        }
        check_field(self.tenant.as_bytes(), limits.max_field_bytes, "tenant")?;
        check_field(
            self.trust_domain.as_bytes(),
            limits.max_field_bytes,
            "trust domain",
        )?;
        if self.layout.family == 0 || self.layout.major == 0 {
            return Err(CacheError::IdentityInvalid {
                reason: "layout abi",
            });
        }
        if self.shard.tp_size == 0
            || self.shard.pp_size == 0
            || self.shard.tp_rank >= self.shard.tp_size
            || self.shard.pp_rank >= self.shard.pp_size
        {
            return Err(CacheError::IdentityInvalid { reason: "shard" });
        }
        if self.block_token_count == 0 || self.block_token_count > limits.max_block_tokens {
            return Err(CacheError::IdentityInvalid {
                reason: "block token count",
            });
        }
        if self.valid_length == 0 || self.valid_length > self.block_token_count {
            return Err(CacheError::IdentityInvalid {
                reason: "valid length",
            });
        }
        if self.prefix_chain.is_empty()
            || self.prefix_chain.len() > limits.max_prefix_links as usize
        {
            return Err(CacheError::QuotaExceeded {
                kind: crate::QuotaKind::PrefixLinks,
            });
        }
        if self.rope.scaling == RopeScaling::None && self.rope.scaling_factor_bits != 0 {
            return Err(CacheError::IdentityInvalid {
                reason: "rope scaling factor",
            });
        }
        Ok(())
    }

    pub(crate) fn encode_body(&self, logical_only: bool) -> Result<Vec<u8>, CacheError> {
        let mut w = ByteWriter::with_capacity(256);
        w.u16(self.version);
        write_tlv(&mut w, TAG_TENANT, self.tenant.as_bytes())?;
        write_tlv(&mut w, TAG_TRUST, self.trust_domain.as_bytes())?;
        write_tlv(&mut w, TAG_MODEL_REV, &self.model_revision.0)?;
        write_tlv(&mut w, TAG_MODEL_CFG, &self.model_config_hash.0)?;
        write_tlv(&mut w, TAG_TOKENIZER, &self.tokenizer_revision.0)?;
        write_tlv(&mut w, TAG_ADAPTER, &encode_adapter(&self.adapter))?;
        if !logical_only {
            write_tlv(&mut w, TAG_DTYPE, &u16::to_le_bytes(self.kv_dtype as u16))?;
            write_tlv(
                &mut w,
                TAG_QUANT,
                &u16::to_le_bytes(self.quant_format as u16),
            )?;
            write_tlv(&mut w, TAG_LAYOUT, &encode_layout(&self.layout))?;
            write_tlv(&mut w, TAG_SHARD, &encode_shard(&self.shard))?;
            write_tlv(
                &mut w,
                TAG_POSITION,
                &u16::to_le_bytes(self.position as u16),
            )?;
            write_tlv(&mut w, TAG_ROPE, &encode_rope(&self.rope))?;
        }
        write_tlv(&mut w, TAG_PREFIX, &encode_prefix(&self.prefix_chain)?)?;
        if !logical_only {
            write_tlv(
                &mut w,
                TAG_BLOCK_TOKENS,
                &self.block_token_count.to_le_bytes(),
            )?;
            write_tlv(&mut w, TAG_VALID_LEN, &self.valid_length.to_le_bytes())?;
        }
        if let Some(mm) = &self.multimodal {
            let mut mm_bytes = Vec::with_capacity(64);
            mm_bytes.extend_from_slice(&mm.processor_revision.0);
            mm_bytes.extend_from_slice(&mm.input_digest.0);
            write_tlv(&mut w, TAG_MULTIMODAL, &mm_bytes)?;
        }
        Ok(w.finish())
    }

    pub(crate) fn decode_body(bytes: &[u8], logical_only: bool) -> Result<Self, CacheError> {
        let mut r = ByteReader::new(bytes);
        let version = r.u16()?;
        if version != IDENTITY_VERSION {
            return Err(CacheError::IdentityInvalid {
                reason: "identity version",
            });
        }
        let mut tenant = None;
        let mut trust = None;
        let mut model_revision = None;
        let mut model_config_hash = None;
        let mut tokenizer_revision = None;
        let mut adapter = None;
        let mut kv_dtype = None;
        let mut quant_format = None;
        let mut layout = None;
        let mut shard = None;
        let mut position = None;
        let mut rope = None;
        let mut prefix_chain = None;
        let mut block_token_count = None;
        let mut valid_length = None;
        let mut multimodal = None;
        let mut last_tag = 0u16;
        while r.remaining() > 0 {
            let tag = r.u16()?;
            if tag <= last_tag {
                return Err(CacheError::Malformed {
                    reason: "tlv tags must be strictly increasing",
                });
            }
            last_tag = tag;
            let value = r.bytes(1024 * 1024)?;
            match tag {
                TAG_TENANT => tenant = Some(TenantId::new(value.to_vec())?),
                TAG_TRUST => trust = Some(TrustDomain::new(value.to_vec())?),
                TAG_MODEL_REV => model_revision = Some(Digest(expect_len(value, 32)?)),
                TAG_MODEL_CFG => model_config_hash = Some(Digest(expect_len(value, 32)?)),
                TAG_TOKENIZER => tokenizer_revision = Some(Digest(expect_len(value, 32)?)),
                TAG_ADAPTER => adapter = Some(decode_adapter(value)?),
                TAG_DTYPE => kv_dtype = Some(KvDtype::from_u16(expect_u16(value)?)?),
                TAG_QUANT => quant_format = Some(QuantFormat::from_u16(expect_u16(value)?)?),
                TAG_LAYOUT => layout = Some(decode_layout(value)?),
                TAG_SHARD => shard = Some(decode_shard(value)?),
                TAG_POSITION => position = Some(PositionEncoding::from_u16(expect_u16(value)?)?),
                TAG_ROPE => rope = Some(decode_rope(value)?),
                TAG_PREFIX => prefix_chain = Some(decode_prefix(value)?),
                TAG_BLOCK_TOKENS => block_token_count = Some(expect_u32(value)?),
                TAG_VALID_LEN => valid_length = Some(expect_u32(value)?),
                TAG_MULTIMODAL => {
                    if value.len() != 64 {
                        return Err(CacheError::Malformed {
                            reason: "multimodal identity",
                        });
                    }
                    let mut processor = [0u8; 32];
                    let mut input = [0u8; 32];
                    processor.copy_from_slice(&value[..32]);
                    input.copy_from_slice(&value[32..]);
                    multimodal = Some(MultimodalIdentity {
                        processor_revision: Digest(processor),
                        input_digest: Digest(input),
                    });
                }
                _ => {
                    return Err(CacheError::Malformed {
                        reason: "unknown identity tag",
                    })
                }
            }
        }
        r.finish()?;
        let id = Self {
            version,
            tenant: tenant.ok_or(CacheError::Malformed {
                reason: "missing tenant",
            })?,
            trust_domain: trust.ok_or(CacheError::Malformed {
                reason: "missing trust domain",
            })?,
            model_revision: model_revision.ok_or(CacheError::Malformed {
                reason: "missing model revision",
            })?,
            model_config_hash: model_config_hash.ok_or(CacheError::Malformed {
                reason: "missing model config",
            })?,
            tokenizer_revision: tokenizer_revision.ok_or(CacheError::Malformed {
                reason: "missing tokenizer",
            })?,
            adapter: adapter.ok_or(CacheError::Malformed {
                reason: "missing adapter",
            })?,
            kv_dtype: if logical_only {
                KvDtype::Bf16
            } else {
                kv_dtype.ok_or(CacheError::Malformed {
                    reason: "missing dtype",
                })?
            },
            quant_format: if logical_only {
                QuantFormat::None
            } else {
                quant_format.ok_or(CacheError::Malformed {
                    reason: "missing quant",
                })?
            },
            layout: if logical_only {
                LayoutAbi {
                    family: 1,
                    major: 1,
                    minor: 0,
                }
            } else {
                layout.ok_or(CacheError::Malformed {
                    reason: "missing layout",
                })?
            },
            shard: if logical_only {
                ShardPlacement {
                    tp_size: 1,
                    tp_rank: 0,
                    pp_size: 1,
                    pp_rank: 0,
                }
            } else {
                shard.ok_or(CacheError::Malformed {
                    reason: "missing shard",
                })?
            },
            position: if logical_only {
                PositionEncoding::Rope
            } else {
                position.ok_or(CacheError::Malformed {
                    reason: "missing position",
                })?
            },
            rope: if logical_only {
                RopeConfig {
                    theta_bits: 0,
                    scaling: RopeScaling::None,
                    scaling_factor_bits: 0,
                    max_position_embeddings: 1,
                }
            } else {
                rope.ok_or(CacheError::Malformed {
                    reason: "missing rope",
                })?
            },
            prefix_chain: prefix_chain.ok_or(CacheError::Malformed {
                reason: "missing prefix",
            })?,
            block_token_count: if logical_only {
                1
            } else {
                block_token_count.ok_or(CacheError::Malformed {
                    reason: "missing block tokens",
                })?
            },
            valid_length: if logical_only {
                1
            } else {
                valid_length.ok_or(CacheError::Malformed {
                    reason: "missing valid length",
                })?
            },
            multimodal,
        };
        if logical_only
            && (kv_dtype.is_some()
                || quant_format.is_some()
                || layout.is_some()
                || shard.is_some()
                || position.is_some()
                || rope.is_some()
                || block_token_count.is_some()
                || valid_length.is_some())
        {
            return Err(CacheError::Malformed {
                reason: "physical field on logical key",
            });
        }
        // Syntax alone is insufficient: callers also decode exports and keys
        // without passing through a store's admission checks.
        id.validate_wire()?;
        Ok(id)
    }
}

fn check_field(bytes: &[u8], max: u32, reason: &'static str) -> Result<(), CacheError> {
    if bytes.is_empty() || bytes.len() > max as usize {
        return Err(CacheError::IdentityInvalid { reason });
    }
    Ok(())
}

fn write_tlv(w: &mut ByteWriter, tag: u16, value: &[u8]) -> Result<(), CacheError> {
    w.u16(tag);
    w.bytes(value)
}

fn expect_len(value: &[u8], n: usize) -> Result<[u8; 32], CacheError> {
    if value.len() != n || n != 32 {
        return Err(CacheError::Malformed {
            reason: "digest length",
        });
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(value);
    Ok(out)
}

fn expect_u16(value: &[u8]) -> Result<u16, CacheError> {
    if value.len() != 2 {
        return Err(CacheError::Malformed {
            reason: "u16 field",
        });
    }
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn expect_u32(value: &[u8]) -> Result<u32, CacheError> {
    if value.len() != 4 {
        return Err(CacheError::Malformed {
            reason: "u32 field",
        });
    }
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn encode_adapter(adapter: &AdapterIdentity) -> Vec<u8> {
    match adapter {
        AdapterIdentity::None => vec![0],
        AdapterIdentity::Lora { revision } => {
            let mut v = Vec::with_capacity(33);
            v.push(1);
            v.extend_from_slice(&revision.0);
            v
        }
    }
}

fn decode_adapter(value: &[u8]) -> Result<AdapterIdentity, CacheError> {
    match value.first().copied() {
        Some(0) if value.len() == 1 => Ok(AdapterIdentity::None),
        Some(1) if value.len() == 33 => {
            let mut d = [0u8; 32];
            d.copy_from_slice(&value[1..]);
            Ok(AdapterIdentity::Lora {
                revision: Digest(d),
            })
        }
        _ => Err(CacheError::Malformed { reason: "adapter" }),
    }
}

fn encode_layout(layout: &LayoutAbi) -> [u8; 6] {
    let mut o = [0u8; 6];
    o[0..2].copy_from_slice(&layout.family.to_le_bytes());
    o[2..4].copy_from_slice(&layout.major.to_le_bytes());
    o[4..6].copy_from_slice(&layout.minor.to_le_bytes());
    o
}

fn decode_layout(value: &[u8]) -> Result<LayoutAbi, CacheError> {
    if value.len() != 6 {
        return Err(CacheError::Malformed { reason: "layout" });
    }
    Ok(LayoutAbi {
        family: u16::from_le_bytes([value[0], value[1]]),
        major: u16::from_le_bytes([value[2], value[3]]),
        minor: u16::from_le_bytes([value[4], value[5]]),
    })
}

fn encode_shard(shard: &ShardPlacement) -> [u8; 8] {
    let mut o = [0u8; 8];
    o[0..2].copy_from_slice(&shard.tp_size.to_le_bytes());
    o[2..4].copy_from_slice(&shard.tp_rank.to_le_bytes());
    o[4..6].copy_from_slice(&shard.pp_size.to_le_bytes());
    o[6..8].copy_from_slice(&shard.pp_rank.to_le_bytes());
    o
}

fn decode_shard(value: &[u8]) -> Result<ShardPlacement, CacheError> {
    if value.len() != 8 {
        return Err(CacheError::Malformed { reason: "shard" });
    }
    Ok(ShardPlacement {
        tp_size: u16::from_le_bytes([value[0], value[1]]),
        tp_rank: u16::from_le_bytes([value[2], value[3]]),
        pp_size: u16::from_le_bytes([value[4], value[5]]),
        pp_rank: u16::from_le_bytes([value[6], value[7]]),
    })
}

fn encode_rope(rope: &RopeConfig) -> [u8; 22] {
    let mut o = [0u8; 22];
    o[0..8].copy_from_slice(&rope.theta_bits.to_le_bytes());
    o[8..10].copy_from_slice(&(rope.scaling as u16).to_le_bytes());
    o[10..18].copy_from_slice(&rope.scaling_factor_bits.to_le_bytes());
    o[18..22].copy_from_slice(&rope.max_position_embeddings.to_le_bytes());
    o
}

fn decode_rope(value: &[u8]) -> Result<RopeConfig, CacheError> {
    if value.len() != 22 {
        return Err(CacheError::Malformed { reason: "rope" });
    }
    Ok(RopeConfig {
        theta_bits: u64::from_le_bytes(copy8(&value[0..8])),
        scaling: RopeScaling::from_u16(u16::from_le_bytes([value[8], value[9]]))?,
        scaling_factor_bits: u64::from_le_bytes(copy8(&value[10..18])),
        max_position_embeddings: u32::from_le_bytes(copy4(&value[18..22])),
    })
}

fn copy8(value: &[u8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    out.copy_from_slice(value);
    out
}

fn copy4(value: &[u8]) -> [u8; 4] {
    let mut out = [0u8; 4];
    out.copy_from_slice(value);
    out
}

fn encode_prefix(chain: &[Digest]) -> Result<Vec<u8>, CacheError> {
    let mut w = ByteWriter::with_capacity(4 + chain.len() * 32);
    let n = u32::try_from(chain.len()).map_err(|_| CacheError::Malformed {
        reason: "prefix chain",
    })?;
    w.u32(n);
    for link in chain {
        w.raw(&link.0);
    }
    Ok(w.finish())
}

fn decode_prefix(value: &[u8]) -> Result<Vec<Digest>, CacheError> {
    let mut r = ByteReader::new(value);
    let n = r.u32()? as usize;
    if n == 0 || n > 4096 {
        return Err(CacheError::Malformed {
            reason: "prefix chain",
        });
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(Digest(r.array32()?));
    }
    r.finish()?;
    Ok(out)
}

/// True when two identities share logical content fields but differ in a
/// byte-layout field. Used by tests; the store compares encoded keys.
pub fn logical_fields_diverge(a: &CacheIdentity, b: &CacheIdentity) -> bool {
    a.tenant != b.tenant
        || a.trust_domain != b.trust_domain
        || a.model_revision != b.model_revision
        || a.model_config_hash != b.model_config_hash
        || a.tokenizer_revision != b.tokenizer_revision
        || a.adapter != b.adapter
        || a.prefix_chain != b.prefix_chain
        || a.multimodal != b.multimodal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_physical_key, encode_physical_key, synthetic_identity, ResourceLimits};

    #[test]
    fn identity_decoder_rejects_structurally_encoded_invalid_identity() {
        let base = synthetic_identity(1, 1);
        let mut cases = Vec::new();
        let mut id = base.clone();
        id.layout.family = 0;
        cases.push(id);
        let mut id = base.clone();
        id.shard.tp_rank = id.shard.tp_size;
        cases.push(id);
        let mut id = base.clone();
        id.block_token_count = 0;
        cases.push(id);
        let mut id = base.clone();
        id.valid_length = id.block_token_count + 1;
        cases.push(id);
        let mut id = base;
        id.rope.scaling_factor_bits = 1.0f64.to_bits();
        cases.push(id);
        for id in cases {
            let bytes = id.encode_body(false).unwrap();
            assert!(matches!(
                CacheIdentity::from_bytes(&bytes),
                Err(CacheError::IdentityInvalid { .. })
            ));
        }
    }

    #[test]
    fn wire_validation_preserves_hard_bounds_without_imposing_store_defaults() {
        let mut id = synthetic_identity(1, 1);
        id.block_token_count = u32::MAX;
        id.valid_length = u32::MAX;
        id.prefix_chain = vec![Digest([1; 32]); 4096];
        assert_eq!(
            CacheIdentity::from_bytes(&id.to_bytes().unwrap()).unwrap(),
            id
        );
        id.prefix_chain.push(Digest([2; 32]));
        assert!(id.validate_wire().is_err());
        assert!(CacheIdentity::from_bytes(&id.to_bytes().unwrap()).is_err());
    }

    #[test]
    fn physical_keys_separate_all_security_and_layout_fields() {
        let base = synthetic_identity(1, 1);
        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.tenant = TenantId::new(b"other-tenant".to_vec()).unwrap();
        variants.push(changed);
        let mut changed = base.clone();
        changed.trust_domain = TrustDomain::new(b"other-trust".to_vec()).unwrap();
        variants.push(changed);
        let mut changed = base.clone();
        changed.model_revision = Digest([9; 32]);
        variants.push(changed);
        let mut changed = base.clone();
        changed.model_config_hash = Digest([9; 32]);
        variants.push(changed);
        let mut changed = base.clone();
        changed.tokenizer_revision = Digest([9; 32]);
        variants.push(changed);
        let mut changed = base.clone();
        changed.adapter = AdapterIdentity::Lora {
            revision: Digest([9; 32]),
        };
        variants.push(changed);
        let mut changed = base.clone();
        changed.kv_dtype = KvDtype::F16;
        variants.push(changed);
        let mut changed = base.clone();
        changed.quant_format = QuantFormat::Fp8;
        variants.push(changed);
        let mut changed = base.clone();
        changed.layout.minor += 1;
        variants.push(changed);
        let mut changed = base.clone();
        changed.shard.tp_size = 2;
        changed.shard.tp_rank = 1;
        variants.push(changed);
        let mut changed = base.clone();
        changed.position = PositionEncoding::Absolute;
        variants.push(changed);
        let mut changed = base.clone();
        changed.rope.theta_bits = 500_000.0f64.to_bits();
        variants.push(changed);
        let mut changed = base.clone();
        changed.prefix_chain[0] = Digest([9; 32]);
        variants.push(changed);
        let mut changed = base.clone();
        changed.block_token_count += 1;
        variants.push(changed);
        let mut changed = base.clone();
        changed.valid_length -= 1;
        variants.push(changed);
        let mut changed = base.clone();
        changed.multimodal = Some(MultimodalIdentity {
            processor_revision: Digest([8; 32]),
            input_digest: Digest([9; 32]),
        });
        variants.push(changed);
        let limits = IdentityLimits::from_resources(&ResourceLimits::dev_defaults());
        let key = encode_physical_key(&base, &limits).unwrap();
        for changed in variants {
            let other_key = encode_physical_key(&changed, &limits).unwrap();
            assert_ne!(key, other_key);
            assert_eq!(decode_physical_key(&other_key).unwrap(), changed);
        }
    }
}
