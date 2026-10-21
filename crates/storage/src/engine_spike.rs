// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! CPU ABI spike for opaque whole blocks. This is not a vLLM adapter or an FFI.
//! The expected identity comes from the caller, never from an untrusted export.

use gpucache_core_types::{
    verify_checksum, CacheError, CacheIdentity, IdentityLimits, ResourceLimits,
};
use gpucache_protocol::{decode_export, encode_export, CommittedExport};

/// Version of this Rust-side spike contract, independent of GCK1/GCKR/GEXP.
pub const ENGINE_SPIKE_ABI_VERSION: u16 = 1;

#[derive(Clone, Debug)]
pub struct EngineBlockContract {
    identity: CacheIdentity,
    length: u64,
}

impl EngineBlockContract {
    pub fn new(abi_version: u16, identity: CacheIdentity, length: u64) -> Result<Self, CacheError> {
        if abi_version != ENGINE_SPIKE_ABI_VERSION {
            return Err(CacheError::Incompatible);
        }
        let limits = ResourceLimits::dev_defaults();
        identity.validate(&IdentityLimits::from_resources(&limits))?;
        if length == 0 || length > limits.max_object_bytes {
            return Err(CacheError::ObjectTooLarge {
                limit: limits.max_object_bytes,
                actual: length,
            });
        }
        Ok(Self { identity, length })
    }

    /// Validate before allocating an import buffer or mutating engine state.
    /// This authenticates neither the producer nor its model computation.
    pub fn validate(&self, block: &CommittedExport) -> Result<(), CacheError> {
        if block.identity != self.identity {
            return Err(CacheError::Incompatible);
        }
        if block.source_generation == 0 {
            return Err(CacheError::UnknownGeneration);
        }
        if block.payload.len() as u64 != self.length {
            return Err(CacheError::LengthMismatch {
                expected: self.length,
                actual: block.payload.len() as u64,
            });
        }
        verify_checksum(&block.payload, block.checksum)
    }

    /// Exercise the existing export codec with a caller-supplied ABI contract.
    /// No tensor conversion, prefix slicing, or model execution occurs.
    pub fn roundtrip(&self, block: &CommittedExport) -> Result<CommittedExport, CacheError> {
        self.validate(block)?;
        let imported = decode_export(&encode_export(block)?)?;
        self.validate(&imported)?;
        Ok(imported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpucache_core_types::{content_checksum, synthetic_identity, Digest, TenantId};

    fn fake() -> CommittedExport {
        let payload = b"FAKE_BLOCK_NOT_ENGINE_OUTPUT".to_vec();
        CommittedExport {
            identity: synthetic_identity(1, 1),
            source_generation: 7,
            checksum: content_checksum(&payload),
            payload,
        }
    }

    #[test]
    fn fake_whole_block_roundtrip_preserves_bytes_and_source_generation() {
        let block = fake();
        let contract =
            EngineBlockContract::new(1, block.identity.clone(), block.payload.len() as u64)
                .unwrap();
        assert_eq!(contract.roundtrip(&block).unwrap(), block);
        assert!(EngineBlockContract::new(2, block.identity.clone(), 1).is_err());
        assert!(EngineBlockContract::new(1, block.identity.clone(), 0).is_err());
    }

    #[test]
    fn wrong_tenant_model_layout_and_partial_block_cannot_be_imported() {
        let block = fake();
        let mut identities = vec![block.identity.clone(); 5];
        identities[0].tenant = TenantId::new(b"other".to_vec()).unwrap();
        identities[1].layout.family += 1;
        identities[2].prefix_chain.push(Digest([5; 32]));
        identities[3].valid_length -= 1;
        identities[4].model_revision = Digest([6; 32]);
        for expected in identities {
            let contract =
                EngineBlockContract::new(1, expected, block.payload.len() as u64).unwrap();
            assert_eq!(contract.roundtrip(&block), Err(CacheError::Incompatible));
        }
    }

    #[test]
    fn corrupt_truncated_and_uncommitted_exports_fail_before_import() {
        let block = fake();
        let contract =
            EngineBlockContract::new(1, block.identity.clone(), block.payload.len() as u64)
                .unwrap();
        let mut changed = block.clone();
        changed.payload[0] ^= 1;
        assert_eq!(
            contract.roundtrip(&changed),
            Err(CacheError::ChecksumMismatch)
        );
        changed = block.clone();
        changed.payload.pop();
        assert!(matches!(
            contract.roundtrip(&changed),
            Err(CacheError::LengthMismatch { .. })
        ));
        changed = block;
        changed.source_generation = 0;
        assert_eq!(
            contract.roundtrip(&changed),
            Err(CacheError::UnknownGeneration)
        );
    }
}
