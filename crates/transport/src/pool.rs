// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Mutex;

use gpucache_core_types::{CacheError, QuotaKind};

/// Fixed number of reusable read buffers. Checkout fails instead of allocating
/// another one once `capacity` buffers exist.
pub struct Pool {
    inner: Mutex<PoolInner>,
}

struct PoolInner {
    free: Vec<Vec<u8>>,
    allocated: usize,
    capacity: usize,
}

pub struct Pooled {
    buf: Vec<u8>,
    pool: std::sync::Arc<Pool>,
}

impl Pool {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(PoolInner {
                free: Vec::new(),
                allocated: 0,
                capacity: capacity.max(1),
            }),
        }
    }

    pub fn checkout(self: &std::sync::Arc<Self>) -> Result<Pooled, CacheError> {
        let mut guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(buf) = guard.free.pop() {
            return Ok(Pooled {
                buf,
                pool: std::sync::Arc::clone(self),
            });
        }
        if guard.allocated >= guard.capacity {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests,
            });
        }
        guard.allocated += 1;
        drop(guard);
        Ok(Pooled {
            buf: Vec::new(),
            pool: std::sync::Arc::clone(self),
        })
    }
}

impl Pooled {
    pub fn buf_mut(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.buf);
        let mut guard = self
            .pool
            .inner
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        guard.free.push(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn checkout_stops_at_capacity() {
        let pool = Arc::new(Pool::new(2));
        let a = pool.checkout().unwrap();
        let b = pool.checkout().unwrap();
        assert!(matches!(
            pool.checkout(),
            Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests
            })
        ));
        drop(a);
        assert!(pool.checkout().is_ok());
        drop(b);
    }
}
