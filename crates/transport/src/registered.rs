// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Bounded buffer pool that *simulates* pin/registration.
//!
//! Slots are ordinary `Vec<u8>` allocations. Nothing here calls `ibv_reg_mr`
//! or a NIC. Checkout prefers a slot that already has a binding, and a batch
//! that cannot be satisfied is left untouched. Unknown completion quarantines
//! a slot. Lease expiry does not free it. A new region id is assigned only
//! when an allocation is bound again; retired ids are not recycled.

use gpucache_core_types::{CacheError, QuotaKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotId(u64);

impl SlotId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimCompletion {
    Completed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedBatch {
    id: u64,
    slots: Vec<SlotId>,
}

impl CheckedBatch {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn slots(&self) -> &[SlotId] {
        &self.slots
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    pub idle_unbound: usize,
    pub idle_registered: usize,
    pub checked_out: usize,
    pub quarantined: usize,
    pub registration_ops: u64,
    pub reused_registrations: u64,
    pub backing_reused: u64,
    pub epoch: u64,
    pub held_bytes: u64,
    pub fence_count: u64,
    pub drained_slots: u64,
    pub reclaimed_slots: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegionState {
    Unbound,
    Idle,
    CheckedOut { batch: u64 },
    Quarantined { fenced: bool },
}

struct Region {
    id: Option<SlotId>,
    bytes: Vec<u8>,
    state: RegionState,
    had_registration: bool,
}

/// Fixed set of host buffers. Checkout fails when every slot is busy.
pub struct RegisteredPool {
    slot_bytes: usize,
    regions: Vec<Region>,
    next_id: u64,
    next_batch: u64,
    registration_ops: u64,
    reused_registrations: u64,
    backing_reused: u64,
    epoch: u64,
    fence_count: u64,
    drained_slots: u64,
    reclaimed_slots: u64,
}

impl RegisteredPool {
    pub fn new(capacity: usize, slot_bytes: usize) -> Result<Self, CacheError> {
        if capacity == 0 || capacity > 4096 || slot_bytes == 0 {
            return Err(CacheError::Malformed {
                reason: "buffer pool",
            });
        }
        if slot_bytes > 16 * 1024 * 1024 {
            return Err(CacheError::ObjectTooLarge {
                limit: 16 * 1024 * 1024,
                actual: slot_bytes as u64,
            });
        }
        let mut regions = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            regions.push(Region {
                id: None,
                bytes: vec![0u8; slot_bytes],
                state: RegionState::Unbound,
                had_registration: false,
            });
        }
        Ok(Self {
            slot_bytes,
            regions,
            next_id: 1,
            next_batch: 1,
            registration_ops: 0,
            reused_registrations: 0,
            backing_reused: 0,
            epoch: 1,
            fence_count: 0,
            drained_slots: 0,
            reclaimed_slots: 0,
        })
    }

    pub fn stats(&self) -> PoolStats {
        let mut idle_unbound = 0;
        let mut idle_registered = 0;
        let mut checked_out = 0;
        let mut quarantined = 0;
        for region in &self.regions {
            match region.state {
                RegionState::Unbound => idle_unbound += 1,
                RegionState::Idle => idle_registered += 1,
                RegionState::CheckedOut { .. } => checked_out += 1,
                RegionState::Quarantined { .. } => quarantined += 1,
            }
        }
        PoolStats {
            idle_unbound,
            idle_registered,
            checked_out,
            quarantined,
            registration_ops: self.registration_ops,
            reused_registrations: self.reused_registrations,
            backing_reused: self.backing_reused,
            epoch: self.epoch,
            held_bytes: (self.regions.len() * self.slot_bytes) as u64,
            fence_count: self.fence_count,
            drained_slots: self.drained_slots,
            reclaimed_slots: self.reclaimed_slots,
        }
    }

    /// Copy `payload` into a checked-out slot and zero the tail. The slot is
    /// a host `Vec`, not a NIC registration.
    pub fn copy_in(&mut self, id: SlotId, payload: &[u8]) -> Result<(), CacheError> {
        let slot_bytes = self.slot_bytes;
        let region = self.region_mut(id)?;
        if !matches!(region.state, RegionState::CheckedOut { .. }) {
            return Err(CacheError::NotReclaimable {
                from: gpucache_core_types::BlockState::Absent,
            });
        }
        if payload.len() > slot_bytes {
            return Err(CacheError::ObjectTooLarge {
                limit: slot_bytes as u64,
                actual: payload.len() as u64,
            });
        }
        region.bytes[..payload.len()].copy_from_slice(payload);
        region.bytes[payload.len()..].fill(0);
        let copied = &region.bytes[..payload.len()] == payload;
        let tail_clear = region.bytes[payload.len()..].iter().all(|byte| *byte == 0);
        if !copied || !tail_clear {
            return Err(CacheError::ChecksumMismatch);
        }
        Ok(())
    }

    /// Take `n` idle slots. Already-bound slots are preferred and do not count
    /// as a new registration. The call changes nothing when it returns an error,
    /// including when the region-id space cannot cover every new binding.
    pub fn checkout_batch(&mut self, n: usize) -> Result<CheckedBatch, CacheError> {
        if n == 0 || n > self.regions.len() {
            return Err(CacheError::Malformed {
                reason: "buffer pool",
            });
        }
        if self.next_batch == u64::MAX {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::Registrations,
            });
        }
        let mut idle = Vec::new();
        let mut unbound = Vec::new();
        for (index, region) in self.regions.iter().enumerate() {
            match region.state {
                RegionState::Idle => {
                    if region.id.is_none() {
                        return Err(CacheError::Storage {
                            reason: "buffer pool",
                        });
                    }
                    idle.push(index);
                }
                RegionState::Unbound => unbound.push(index),
                RegionState::CheckedOut { .. } | RegionState::Quarantined { .. } => {}
            }
        }
        if idle.len() + unbound.len() < n {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::Registrations,
            });
        }
        let mut chosen = Vec::with_capacity(n);
        for index in idle {
            if chosen.len() == n {
                break;
            }
            chosen.push(index);
        }
        let fresh = n - chosen.len();
        let available_ids = u64::MAX - self.next_id;
        if fresh as u64 > available_ids {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::Registrations,
            });
        }
        for index in unbound.into_iter().take(fresh) {
            chosen.push(index);
        }

        let batch = self.next_batch;
        let mut next_id = self.next_id;
        let mut planned = Vec::with_capacity(n);
        for index in chosen {
            match self.regions[index].state {
                RegionState::Idle => {
                    let id = self.regions[index].id.ok_or(CacheError::Storage {
                        reason: "buffer pool",
                    })?;
                    planned.push((index, id, false));
                }
                RegionState::Unbound => {
                    let id = SlotId(next_id);
                    next_id = next_id.saturating_add(1);
                    planned.push((index, id, true));
                }
                RegionState::CheckedOut { .. } | RegionState::Quarantined { .. } => {
                    return Err(CacheError::Storage {
                        reason: "buffer pool",
                    });
                }
            }
        }

        self.next_batch = self.next_batch.saturating_add(1);
        self.next_id = next_id;
        let mut slots = Vec::with_capacity(n);
        for (index, id, is_new) in planned {
            if is_new {
                if self.regions[index].had_registration {
                    self.backing_reused = self.backing_reused.saturating_add(1);
                }
                self.regions[index].id = Some(id);
                self.regions[index].had_registration = true;
                self.registration_ops = self.registration_ops.saturating_add(1);
            } else {
                self.reused_registrations = self.reused_registrations.saturating_add(1);
            }
            self.regions[index].state = RegionState::CheckedOut { batch };
            slots.push(id);
        }
        Ok(CheckedBatch { id: batch, slots })
    }

    pub fn complete_batch(
        &mut self,
        batch: &CheckedBatch,
        completion: SimCompletion,
    ) -> Result<(), CacheError> {
        self.ensure_batch(batch)?;
        for region in &mut self.regions {
            let Some(id) = region.id else {
                continue;
            };
            if batch.slots.contains(&id) {
                region.state = match completion {
                    SimCompletion::Completed => RegionState::Idle,
                    SimCompletion::Unknown => RegionState::Quarantined { fenced: false },
                };
            }
        }
        Ok(())
    }

    /// Lease expiry is not proof that a copy has stopped.
    pub fn on_lease_expiry(&mut self, id: SlotId) -> Result<(), CacheError> {
        let region = self.region_mut(id)?;
        if let RegionState::CheckedOut { .. } = region.state {
            region.state = RegionState::Quarantined { fenced: false };
        }
        Ok(())
    }

    /// Caller attests the copy is done. Only a quarantined slot returns to
    /// the unbound pool, under a future id.
    pub fn attest_drain(&mut self, id: SlotId) -> Result<(), CacheError> {
        let region = self.region_mut(id)?;
        if !matches!(region.state, RegionState::Quarantined { .. }) {
            return Err(CacheError::NotReclaimable {
                from: gpucache_core_types::BlockState::Writing,
            });
        }
        region.id = None;
        region.state = RegionState::Unbound;
        self.drained_slots = self.drained_slots.saturating_add(1);
        Ok(())
    }

    /// Move in-flight and already-quarantined slots to fenced quarantine.
    /// Does not free them. If the epoch cannot advance, the pool is unchanged.
    pub fn fence_epoch(&mut self) -> Result<u64, CacheError> {
        let next = self.epoch.checked_add(1).ok_or(CacheError::QuotaExceeded {
            kind: QuotaKind::Registrations,
        })?;
        self.epoch = next;
        self.fence_count = self.fence_count.saturating_add(1);
        for region in &mut self.regions {
            if matches!(
                region.state,
                RegionState::CheckedOut { .. } | RegionState::Quarantined { .. }
            ) {
                region.state = RegionState::Quarantined { fenced: true };
            }
        }
        Ok(self.epoch)
    }

    /// Free slots that fencing already marked. Unfenced quarantine stays.
    pub fn reclaim_fenced(&mut self) -> Result<usize, CacheError> {
        let mut freed = 0;
        for region in &mut self.regions {
            if matches!(region.state, RegionState::Quarantined { fenced: true }) {
                region.id = None;
                region.state = RegionState::Unbound;
                freed += 1;
            }
        }
        self.reclaimed_slots = self.reclaimed_slots.saturating_add(freed as u64);
        Ok(freed)
    }

    #[cfg(test)]
    fn set_next_id_for_test(&mut self, next_id: u64) {
        self.next_id = next_id;
    }

    #[cfg(test)]
    fn set_epoch_for_test(&mut self, epoch: u64) {
        self.epoch = epoch;
    }

    fn ensure_batch(&self, batch: &CheckedBatch) -> Result<(), CacheError> {
        if batch.slots.is_empty() {
            return Err(CacheError::Malformed {
                reason: "buffer pool",
            });
        }
        for id in &batch.slots {
            let region = self.region(*id)?;
            match region.state {
                RegionState::CheckedOut { batch: got } if got == batch.id => {}
                _ => return Err(CacheError::FencingRejected { reason: "buffer" }),
            }
        }
        Ok(())
    }

    fn region(&self, id: SlotId) -> Result<&Region, CacheError> {
        self.regions
            .iter()
            .find(|region| region.id == Some(id))
            .ok_or(CacheError::UnknownGeneration)
    }

    fn region_mut(&mut self, id: SlotId) -> Result<&mut Region, CacheError> {
        self.regions
            .iter_mut()
            .find(|region| region.id == Some(id))
            .ok_or(CacheError::UnknownGeneration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_reused_until_the_slot_is_retired() {
        let mut pool = RegisteredPool::new(1, 16).unwrap();
        let first = pool.checkout_batch(1).unwrap();
        assert_eq!(pool.stats().registration_ops, 1);
        pool.complete_batch(&first, SimCompletion::Completed)
            .unwrap();
        let second = pool.checkout_batch(1).unwrap();
        let stats = pool.stats();
        assert_eq!(stats.registration_ops, 1);
        assert_eq!(stats.reused_registrations, 1);
        assert_eq!(stats.backing_reused, 0);
        assert_eq!(first.slots()[0], second.slots()[0]);
    }

    #[test]
    fn checkout_backpressures_without_growing() {
        let mut pool = RegisteredPool::new(1, 8).unwrap();
        let held = pool.checkout_batch(1).unwrap();
        let err = pool.checkout_batch(1).unwrap_err();
        assert_eq!(
            err,
            CacheError::QuotaExceeded {
                kind: QuotaKind::Registrations,
            }
        );
        assert_eq!(pool.stats().held_bytes, 8);
        assert_eq!(pool.stats().checked_out, 1);
        drop(held);
    }

    #[test]
    fn unknown_completion_and_lease_expiry_do_not_free() {
        let mut pool = RegisteredPool::new(1, 8).unwrap();
        let batch = pool.checkout_batch(1).unwrap();
        let id = batch.slots()[0];
        pool.complete_batch(&batch, SimCompletion::Unknown).unwrap();
        pool.on_lease_expiry(id).unwrap();
        let stats = pool.stats();
        assert_eq!(stats.quarantined, 1);
        assert_eq!(stats.idle_unbound, 0);
        assert_eq!(pool.reclaim_fenced().unwrap(), 0);
        assert_eq!(pool.stats().quarantined, 1);
        pool.attest_drain(id).unwrap();
        assert_eq!(pool.stats().idle_unbound, 1);
        let again = pool.checkout_batch(1).unwrap();
        assert_ne!(again.slots()[0], id);
        assert_eq!(pool.stats().registration_ops, 2);
        assert_eq!(pool.stats().backing_reused, 1);
    }

    #[test]
    fn fence_then_reclaim_frees_only_fenced_slots() {
        let mut pool = RegisteredPool::new(2, 4).unwrap();
        let batch = pool.checkout_batch(1).unwrap();
        let id = batch.slots()[0];
        pool.on_lease_expiry(id).unwrap();
        assert_eq!(pool.reclaim_fenced().unwrap(), 0);
        pool.fence_epoch().unwrap();
        assert_eq!(pool.stats().quarantined, 1);
        assert_eq!(pool.reclaim_fenced().unwrap(), 1);
        assert_eq!(pool.stats().idle_unbound, 2);
        assert_eq!(pool.stats().held_bytes, 8);
    }

    #[test]
    fn failed_checkout_leaves_the_pool_unchanged() {
        let mut pool = RegisteredPool::new(2, 4).unwrap();
        assert!(pool.checkout_batch(3).is_err());
        let stats = pool.stats();
        assert_eq!(stats.idle_unbound, 2);
        assert_eq!(stats.registration_ops, 0);
    }

    #[test]
    fn checkout_prefers_an_idle_registration_over_a_lower_unbound_slot() {
        let mut pool = RegisteredPool::new(2, 8).unwrap();
        let first = pool.checkout_batch(1).unwrap();
        let second = pool.checkout_batch(1).unwrap();
        let idle_id = second.slots()[0];
        pool.complete_batch(&first, SimCompletion::Unknown).unwrap();
        pool.attest_drain(first.slots()[0]).unwrap();
        pool.complete_batch(&second, SimCompletion::Completed)
            .unwrap();
        assert_eq!(pool.stats().idle_unbound, 1);
        assert_eq!(pool.stats().idle_registered, 1);
        let reused = pool.checkout_batch(1).unwrap();
        assert_eq!(reused.slots()[0], idle_id);
        let stats = pool.stats();
        assert_eq!(stats.registration_ops, 2);
        assert_eq!(stats.reused_registrations, 1);
        assert_eq!(stats.idle_unbound, 1);
    }

    #[test]
    fn id_exhaustion_does_not_check_out_a_prefix_of_the_batch() {
        let mut pool = RegisteredPool::new(2, 4).unwrap();
        pool.set_next_id_for_test(u64::MAX - 1);
        let err = pool.checkout_batch(2).unwrap_err();
        assert_eq!(
            err,
            CacheError::QuotaExceeded {
                kind: QuotaKind::Registrations,
            }
        );
        let stats = pool.stats();
        assert_eq!(stats.idle_unbound, 2);
        assert_eq!(stats.checked_out, 0);
        assert_eq!(stats.registration_ops, 0);
        assert_eq!(stats.epoch, 1);
    }

    #[test]
    fn fence_at_epoch_max_leaves_checked_out_slots_in_place() {
        let mut pool = RegisteredPool::new(1, 4).unwrap();
        let batch = pool.checkout_batch(1).unwrap();
        pool.set_epoch_for_test(u64::MAX);
        let err = pool.fence_epoch().unwrap_err();
        assert_eq!(
            err,
            CacheError::QuotaExceeded {
                kind: QuotaKind::Registrations,
            }
        );
        assert_eq!(pool.stats().epoch, u64::MAX);
        assert_eq!(pool.stats().checked_out, 1);
        assert_eq!(pool.stats().quarantined, 0);
        pool.complete_batch(&batch, SimCompletion::Completed)
            .unwrap();
        assert_eq!(pool.stats().idle_registered, 1);
    }

    #[test]
    fn cancellation_storm_requires_drain_and_reports_fence_reclaim_counts() {
        let mut pool = RegisteredPool::new(2, 8).unwrap();
        for _ in 0..1000 {
            let batch = pool.checkout_batch(2).unwrap();
            pool.complete_batch(&batch, SimCompletion::Unknown).unwrap();
            assert!(pool.checkout_batch(1).is_err());
            pool.on_lease_expiry(batch.slots()[0]).unwrap();
            assert_eq!(pool.stats().quarantined, 2);
            pool.attest_drain(batch.slots()[0]).unwrap();
            pool.fence_epoch().unwrap();
            assert_eq!(pool.reclaim_fenced().unwrap(), 1);
            assert_eq!(pool.stats().held_bytes, 16);
        }
        assert_eq!(pool.stats().fence_count, 1000);
        assert_eq!(pool.stats().drained_slots, 1000);
        assert_eq!(pool.stats().reclaimed_slots, 1000);
        assert_eq!(pool.stats().quarantined, 0);
    }
}
