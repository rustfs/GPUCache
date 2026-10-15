// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use sha2::{Digest as _, Sha256};

use crate::error::CacheError;

/// SHA-256 of payload bytes. Not a substitute for a transport MAC.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Checksum(pub [u8; 32]);

impl Checksum {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

pub fn content_checksum(payload: &[u8]) -> Checksum {
    let dig = Sha256::digest(payload);
    let mut out = [0u8; 32];
    out.copy_from_slice(&dig);
    Checksum(out)
}

pub fn verify_checksum(payload: &[u8], expected: Checksum) -> Result<(), CacheError> {
    let actual = content_checksum(payload);
    if actual != expected {
        return Err(CacheError::ChecksumMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_stable_and_detects_a_flip() {
        let a = content_checksum(b"kv-block");
        assert_eq!(a, content_checksum(b"kv-block"));
        assert!(verify_checksum(b"kv-block", a).is_ok());
        assert_eq!(
            verify_checksum(b"kv-blokk", a),
            Err(CacheError::ChecksumMismatch)
        );
    }
}
