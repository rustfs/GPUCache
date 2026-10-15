// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Per-tenant admission window. The caller passes `now_ms`. This module does
//! not sleep and does not read a device clock, so it stays deterministic under
//! qemu-user.

use std::collections::BTreeMap;

use gpucache_core_types::{CacheError, QuotaKind};

struct Window {
    start_ms: u64,
    used: u32,
}

pub struct RateGate {
    default_limit: u32,
    window_ms: u64,
    max_tenants: usize,
    limits: BTreeMap<Vec<u8>, u32>,
    windows: BTreeMap<Vec<u8>, Window>,
}

impl RateGate {
    pub fn new(default_limit: u32, window_ms: u64, max_tenants: usize) -> Result<Self, CacheError> {
        if default_limit == 0 || window_ms == 0 || max_tenants == 0 {
            return Err(CacheError::Malformed { reason: "rate" });
        }
        Ok(Self {
            default_limit,
            window_ms,
            max_tenants,
            limits: BTreeMap::new(),
            windows: BTreeMap::new(),
        })
    }

    pub fn set_limit(&mut self, tenant: &[u8], limit: u32) -> Result<(), CacheError> {
        if tenant.is_empty() || tenant.len() > 128 || limit == 0 {
            return Err(CacheError::Malformed { reason: "rate" });
        }
        if !self.limits.contains_key(tenant) && self.limits.len() >= self.max_tenants {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            });
        }
        self.limits.insert(tenant.to_vec(), limit);
        Ok(())
    }

    pub fn admit(&mut self, tenant: &[u8], now_ms: u64) -> Result<(), CacheError> {
        if tenant.is_empty() || tenant.len() > 128 {
            return Err(CacheError::Malformed { reason: "rate" });
        }
        let limit = self
            .limits
            .get(tenant)
            .copied()
            .unwrap_or(self.default_limit);
        if !self.windows.contains_key(tenant) && self.windows.len() >= self.max_tenants {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            });
        }
        let window = self.windows.entry(tenant.to_vec()).or_insert(Window {
            start_ms: now_ms,
            used: 0,
        });
        if now_ms.saturating_sub(window.start_ms) >= self.window_ms {
            window.start_ms = now_ms;
            window.used = 0;
        }
        if window.used >= limit {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            });
        }
        window.used = window.used.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noisy_neighbor_does_not_spend_the_quiet_tenant_budget() {
        let mut gate = RateGate::new(2, 1_000, 8).unwrap();
        gate.set_limit(b"noisy", 1).unwrap();
        gate.admit(b"noisy", 10).unwrap();
        assert_eq!(
            gate.admit(b"noisy", 20).unwrap_err(),
            CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            }
        );
        gate.admit(b"quiet", 20).unwrap();
        gate.admit(b"quiet", 20).unwrap();
        assert_eq!(
            gate.admit(b"quiet", 20).unwrap_err(),
            CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            }
        );
        gate.admit(b"quiet", 1_020).unwrap();
        gate.admit(b"noisy", 5).unwrap_err();
    }

    #[test]
    fn clock_rewind_does_not_refresh_the_window() {
        let mut gate = RateGate::new(1, 1_000, 2).unwrap();
        gate.admit(b"tenant-a", 500).unwrap();
        assert!(gate.admit(b"tenant-a", 100).is_err());
        assert_eq!(
            gate.admit(b"tenant-a", 500).unwrap_err(),
            CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            }
        );
    }
}
