// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Thread-affinity contract for a future SPDK reactor.
//!
//! SPDK queue pairs and other reactor-affine objects must be created on the
//! reactor thread and must stay there. They are not Tokio tasks and must not
//! move across Tokio workers. This module does not start SPDK, Tokio, or a
//! thread. `submit_on` only checks that the caller names the bound reactor.

use gpucache_core_types::CacheError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReactorId(pub u32);

pub struct AffineQueue {
    bound: ReactorId,
}

impl AffineQueue {
    pub fn bind(bound: ReactorId) -> Self {
        Self { bound }
    }

    pub fn bound(&self) -> ReactorId {
        self.bound
    }

    pub fn submit_on(&self, current: ReactorId) -> Result<(), CacheError> {
        if current != self.bound {
            return Err(CacheError::Storage {
                reason: "reactor affine",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_rejects_a_different_reactor() {
        let queue = AffineQueue::bind(ReactorId(3));
        queue.submit_on(ReactorId(3)).unwrap();
        let err = queue.submit_on(ReactorId(4)).unwrap_err();
        assert_eq!(
            err,
            CacheError::Storage {
                reason: "reactor affine"
            }
        );
        assert_eq!(queue.bound(), ReactorId(3));
    }
}
