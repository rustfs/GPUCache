// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{CacheError, TenantId, TrustDomain};

/// Minimum tenant gate. This is not mTLS and not a production authenticator.
/// The fingerprint is a hash of a credential, never the credential itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthContext {
    pub tenant: TenantId,
    pub trust_domain: TrustDomain,
    pub credential_fingerprint: [u8; 32],
}

impl AuthContext {
    pub fn new(
        tenant: TenantId,
        trust_domain: TrustDomain,
        credential_fingerprint: [u8; 32],
    ) -> Result<Self, CacheError> {
        if credential_fingerprint == [0u8; 32] {
            return Err(CacheError::Unauthorized);
        }
        Ok(Self {
            tenant,
            trust_domain,
            credential_fingerprint,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantPolicy {
    pub tenant: TenantId,
    pub trust_domain: TrustDomain,
    pub credential_fingerprint: [u8; 32],
}

impl TenantPolicy {
    pub fn new(
        tenant: TenantId,
        trust_domain: TrustDomain,
        credential_fingerprint: [u8; 32],
    ) -> Result<Self, CacheError> {
        if credential_fingerprint == [0u8; 32] {
            return Err(CacheError::Unauthorized);
        }
        Ok(Self {
            tenant,
            trust_domain,
            credential_fingerprint,
        })
    }
}

pub fn fingerprints_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
