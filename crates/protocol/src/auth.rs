// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{CacheError, TenantId, TrustDomain};
use std::fmt;

/// Minimum tenant gate. This is not mTLS and not a production authenticator.
/// The fingerprint is a hash of a credential, never the credential itself.
#[derive(Clone, PartialEq, Eq)]
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

    /// Revalidate at trust boundaries because fields remain public for V1 callers.
    pub fn validate(&self) -> Result<(), CacheError> {
        validate_fingerprint(&self.credential_fingerprint)
    }
}

#[derive(Clone, PartialEq, Eq)]
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

    /// Revalidate policy values assembled without the checked constructor.
    pub fn validate(&self) -> Result<(), CacheError> {
        validate_fingerprint(&self.credential_fingerprint)
    }
}

fn validate_fingerprint(fingerprint: &[u8; 32]) -> Result<(), CacheError> {
    if fingerprints_equal(fingerprint, &[0; 32]) {
        Err(CacheError::Unauthorized)
    } else {
        Ok(())
    }
}

impl fmt::Debug for AuthContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthContext")
            .field("tenant", &self.tenant)
            .field("trust_domain", &self.trust_domain)
            .field("credential_fingerprint", &"[REDACTED]")
            .finish()
    }
}

impl fmt::Debug for TenantPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantPolicy")
            .field("tenant", &self.tenant)
            .field("trust_domain", &self.trust_domain)
            .field("credential_fingerprint", &"[REDACTED]")
            .finish()
    }
}

pub fn fingerprints_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_fingerprints_are_redacted_from_debug_output() {
        let tenant = TenantId::new(b"tenant-a".to_vec()).unwrap();
        let trust = TrustDomain::new(b"trust-a".to_vec()).unwrap();
        let ctx = AuthContext::new(tenant.clone(), trust.clone(), [173; 32]).unwrap();
        let policy = TenantPolicy::new(tenant, trust, [173; 32]).unwrap();
        for output in [format!("{ctx:?}"), format!("{policy:?}")] {
            assert!(output.contains("[REDACTED]"));
            assert!(!output.contains("173"));
        }
    }

    #[test]
    fn revalidation_rejects_public_field_constructor_bypass() {
        let tenant = TenantId::new(b"tenant-a".to_vec()).unwrap();
        let trust = TrustDomain::new(b"trust-a".to_vec()).unwrap();
        let ctx = AuthContext {
            tenant: tenant.clone(),
            trust_domain: trust.clone(),
            credential_fingerprint: [0; 32],
        };
        let policy = TenantPolicy {
            tenant,
            trust_domain: trust,
            credential_fingerprint: [0; 32],
        };
        assert_eq!(ctx.validate(), Err(CacheError::Unauthorized));
        assert_eq!(policy.validate(), Err(CacheError::Unauthorized));
    }
}
