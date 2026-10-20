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
    last_now_ms: Option<u64>,
    limits: BTreeMap<Vec<u8>, u32>,
    windows: BTreeMap<Vec<u8>, Window>,
}

impl RateGate {
    pub fn new(default_limit: u32, window_ms: u64, max_tenants: usize) -> Result<Self, CacheError> {
        if default_limit == 0 || window_ms == 0 || max_tenants == 0 || max_tenants > 1024 {
            return Err(CacheError::Malformed { reason: "rate" });
        }
        Ok(Self {
            default_limit,
            window_ms,
            max_tenants,
            last_now_ms: None,
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
        self.admit_batch(tenant, now_ms, 1)
    }

    /// Charge a complete batch atomically, so a batch cannot bypass the item
    /// budget or consume a partial admission when it exceeds the budget.
    pub fn admit_batch(
        &mut self,
        tenant: &[u8],
        now_ms: u64,
        items: usize,
    ) -> Result<(), CacheError> {
        if tenant.is_empty() || tenant.len() > 128 {
            return Err(CacheError::Malformed { reason: "rate" });
        }
        let items = u32::try_from(items)
            .ok()
            .filter(|items| *items != 0)
            .ok_or(CacheError::Malformed { reason: "rate" })?;
        if self.last_now_ms.is_some_and(|last| now_ms < last) {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            });
        }
        self.last_now_ms = Some(now_ms);
        let limit = self
            .limits
            .get(tenant)
            .copied()
            .unwrap_or(self.default_limit);
        if items > limit {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            });
        }
        // Expired tenants can release table capacity. Never evict a live
        // window: doing so would reset that tenant's admission budget.
        self.windows
            .retain(|_, window| now_ms.saturating_sub(window.start_ms) < self.window_ms);
        if !self.windows.contains_key(tenant) && self.windows.len() >= self.max_tenants {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            });
        }
        let window = self.windows.entry(tenant.to_vec()).or_insert(Window {
            start_ms: now_ms,
            used: 0,
        });
        let Some(used) = window.used.checked_add(items).filter(|used| *used <= limit) else {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            });
        };
        window.used = used;
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

    #[test]
    fn batch_failure_preserves_the_remaining_budget() {
        let mut gate = RateGate::new(3, 100, 1).unwrap();
        gate.admit_batch(b"tenant-a", 0, 2).unwrap();
        assert_eq!(
            gate.admit_batch(b"tenant-a", 1, 2),
            Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit,
            })
        );
        gate.admit(b"tenant-a", 2).unwrap();
        assert!(gate.admit(b"tenant-b", 3).is_err());
        gate.admit(b"tenant-b", 100).unwrap();
        assert_eq!(gate.windows.len(), 1);
        assert!(gate.admit_batch(b"tenant-b", 101, 0).is_err());
        assert!(RateGate::new(1, 100, 1025).is_err());
    }

    #[test]
    fn oversized_first_batch_does_not_take_the_only_tenant_slot() {
        let mut gate = RateGate::new(1, 100, 1).unwrap();
        assert_eq!(
            gate.admit_batch(b"tenant-a", 0, 2),
            Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit
            })
        );
        assert!(gate.windows.is_empty());
        gate.admit(b"tenant-b", 0).unwrap();
        assert_eq!(gate.windows.len(), 1);
    }
}
