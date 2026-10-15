// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::CacheError;

/// Current store wire version. Unknown versions fail closed.
pub const WIRE_VERSION: u16 = 1;
pub const HELLO_OP: u16 = 7;

/// Highest version present on both sides. Empty overlap is an error.
pub fn negotiate(local: &[u16], remote: &[u16]) -> Result<u16, CacheError> {
    if local.is_empty() || remote.is_empty() {
        return Err(CacheError::WireVersionUnsupported {
            local: 0,
            remote: 0,
        });
    }
    let mut best: Option<u16> = None;
    for version in remote {
        if local.contains(version) {
            best = Some(best.map_or(*version, |b| b.max(*version)));
        }
    }
    match best {
        Some(v) => Ok(v),
        None => Err(CacheError::WireVersionUnsupported {
            local: local.iter().copied().max().unwrap_or(0),
            remote: remote.iter().copied().max().unwrap_or(0),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_highest_shared_version() {
        assert_eq!(negotiate(&[1], &[1, 2]).unwrap(), 1);
        assert_eq!(negotiate(&[1, 2], &[2]).unwrap(), 2);
        assert_eq!(negotiate(&[1, 3], &[2, 3, 4]).unwrap(), 3);
    }

    #[test]
    fn rejects_an_empty_overlap() {
        assert!(matches!(
            negotiate(&[1], &[2]),
            Err(CacheError::WireVersionUnsupported { .. })
        ));
        assert!(negotiate(&[], &[1]).is_err());
    }
}
