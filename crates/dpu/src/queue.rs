// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Simulated DPU DRAM slab and a storage-queue submit/complete interface.
//!
//! `open_simulated` copies bytes inside this process and labels the completion
//! `NOT_RUN_HARDWARE`. `open_nvme_device`, `open_doca`, and `open_spdk` fail
//! in every build, including `--features dpu-bf3`. Arm process start does not
//! flip `offload_active`.

use std::collections::BTreeMap;

use gpucache_core_types::{
    content_checksum, verify_checksum, BlockState, CacheError, Checksum, QuotaKind, HARD_BYTES_CAP,
};

use crate::degrade::{decide, DegradeDecision, Fault};

// Bound queue-owned metadata independently of payload bytes. These are CPU
// contract limits, not measured NIC queue or registration limits.
const MAX_QUEUE_TASKS: usize = 4096;
const MAX_SLAB_SLOTS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub task_id: u64,
    pub hardware: &'static str,
    pub simulated: bool,
    pub offload_active: bool,
    pub slot: Option<u64>,
    pub payload: Option<Vec<u8>>,
}

/// Arm agent flag. `start` means the process record exists. It does not submit
/// a storage task and it does not activate offload.
#[derive(Debug)]
pub struct ArmAgent {
    started: bool,
}

impl ArmAgent {
    pub fn start() -> Self {
        Self { started: true }
    }

    pub fn process_started(&self) -> bool {
        self.started
    }

    pub fn offload_active(&self) -> bool {
        false
    }
}

struct Slot {
    tenant: Vec<u8>,
    bytes: Vec<u8>,
    checksum: Checksum,
    exposed: bool,
}

struct Slab {
    cap: u64,
    used: u64,
    next: u64,
    slots: BTreeMap<u64, Slot>,
}

impl Slab {
    fn new(cap: u64) -> Result<Self, CacheError> {
        if cap == 0 || cap > HARD_BYTES_CAP as u64 {
            return Err(CacheError::Malformed { reason: "dpu slab" });
        }
        Ok(Self {
            cap,
            used: 0,
            next: 1,
            slots: BTreeMap::new(),
        })
    }

    fn insert(&mut self, tenant: Vec<u8>, bytes: Vec<u8>) -> Result<u64, CacheError> {
        let len = bytes.len() as u64;
        let next = self
            .used
            .checked_add(len)
            .ok_or(CacheError::QuotaExceeded {
                kind: gpucache_core_types::QuotaKind::RetainedBytes,
            })?;
        if next > self.cap || self.next == u64::MAX || self.slots.len() >= MAX_SLAB_SLOTS {
            return Err(CacheError::QuotaExceeded {
                kind: gpucache_core_types::QuotaKind::RetainedBytes,
            });
        }
        let id = self.next;
        self.next += 1;
        self.used = next;
        let checksum = content_checksum(&bytes);
        self.slots.insert(
            id,
            Slot {
                tenant,
                bytes,
                checksum,
                exposed: true,
            },
        );
        Ok(id)
    }

    fn readable_slot(&mut self, tenant: &[u8], id: u64) -> Result<&Slot, CacheError> {
        let Some(slot) = self.slots.get_mut(&id) else {
            return Err(CacheError::Storage {
                reason: "clean miss",
            });
        };
        if slot.tenant.as_slice() != tenant {
            return Err(CacheError::Unauthorized);
        }
        if !slot.exposed {
            return Err(CacheError::Storage {
                reason: "clean miss",
            });
        }
        if verify_checksum(&slot.bytes, slot.checksum).is_err() {
            slot.exposed = false;
            return Err(CacheError::ChecksumMismatch);
        }
        Ok(slot)
    }

    fn read(&mut self, tenant: &[u8], id: u64) -> Result<Vec<u8>, CacheError> {
        Ok(self.readable_slot(tenant, id)?.bytes.clone())
    }

    fn hide_all(&mut self) {
        for slot in self.slots.values_mut() {
            slot.exposed = false;
        }
    }

    fn restart(&mut self) {
        self.slots.clear();
        self.used = 0;
    }
}

enum Pending {
    Write { tenant: Vec<u8>, payload: Vec<u8> },
    Read { tenant: Vec<u8>, slot: u64 },
}

impl Pending {
    fn write_bytes(&self) -> u64 {
        match self {
            Self::Write { payload, .. } => payload.len() as u64,
            Self::Read { .. } => 0,
        }
    }

    fn reads_slot(&self, tenant: &[u8], id: u64) -> bool {
        matches!(self, Self::Read { tenant: owner, slot } if owner.as_slice() == tenant && *slot == id)
    }
}

/// CPU-only storage task owner. Completion, failure, fault injection and drain
/// attestations are trusted owner operations; a task id is not a tenant token.
/// Returned payloads belong to the caller and are outside the queue's byte cap.
pub struct StorageQueue {
    slab: Slab,
    pending: BTreeMap<u64, Pending>,
    pending_bytes: u64,
    quarantined: BTreeMap<u64, Pending>,
    quarantined_bytes: u64,
    next_task: u64,
    max_inflight: usize,
    control_up: bool,
}

impl StorageQueue {
    pub fn open_simulated(cap_bytes: u64, max_inflight: usize) -> Result<Self, CacheError> {
        if max_inflight == 0 || max_inflight > MAX_QUEUE_TASKS {
            return Err(CacheError::Malformed { reason: "dpu slab" });
        }
        Ok(Self {
            slab: Slab::new(cap_bytes)?,
            pending: BTreeMap::new(),
            pending_bytes: 0,
            quarantined: BTreeMap::new(),
            quarantined_bytes: 0,
            next_task: 1,
            max_inflight,
            control_up: true,
        })
    }

    pub fn control_up(&self) -> bool {
        self.control_up
    }

    pub fn inflight(&self) -> usize {
        self.pending.len()
    }

    /// Tasks with unknown completion still hold their payloads and admission
    /// credit until an explicit drain attestation or simulated restart fence.
    pub fn quarantined(&self) -> usize {
        self.quarantined.len()
    }

    /// All queue-owned payload bytes, including hidden slots and quarantine.
    pub fn retained_bytes(&self) -> u64 {
        self.slab.used + self.pending_bytes + self.quarantined_bytes
    }

    pub fn submit_write(&mut self, tenant: &[u8], payload: &[u8]) -> Result<TaskId, CacheError> {
        self.admit(tenant, payload.len())?;
        if self.retained_bytes() + payload.len() as u64 > self.slab.cap {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RetainedBytes,
            });
        }
        let reserved_writes = self
            .pending
            .values()
            .chain(self.quarantined.values())
            .filter(|task| matches!(task, Pending::Write { .. }))
            .count();
        let reserved_slots = self.slab.slots.len() + reserved_writes;
        if reserved_slots >= MAX_SLAB_SLOTS || reserved_writes as u64 >= u64::MAX - self.slab.next {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RetainedBytes,
            });
        }
        self.pending_bytes += payload.len() as u64;
        self.enqueue(Pending::Write {
            tenant: tenant.to_vec(),
            payload: payload.to_vec(),
        })
    }

    pub fn submit_read(&mut self, tenant: &[u8], slot: u64) -> Result<TaskId, CacheError> {
        self.admit(tenant, 1)?;
        // Bind only to a slot that exists and is readable now. A guessed id
        // must never become a read of a slot allocated by a later write.
        self.slab.readable_slot(tenant, slot)?;
        self.enqueue(Pending::Read {
            tenant: tenant.to_vec(),
            slot,
        })
    }

    pub fn complete(&mut self, task: TaskId) -> Result<Completion, CacheError> {
        if self.quarantined.contains_key(&task.0) {
            return Err(CacheError::FencingRejected {
                reason: "task quarantined",
            });
        }
        let Some(pending) = self.pending.remove(&task.0) else {
            return Err(CacheError::UnknownGeneration);
        };
        let (slot, payload) = match pending {
            Pending::Write { tenant, payload } => {
                self.pending_bytes -= payload.len() as u64;
                let id = self.slab.insert(tenant, payload)?;
                (Some(id), None)
            }
            Pending::Read { tenant, slot } => {
                let bytes = self.slab.read(&tenant, slot)?;
                (Some(slot), Some(bytes))
            }
        };
        Ok(self.receipt(task, slot, payload))
    }

    /// Report a confirmed terminal failure. Unknown completion (including a
    /// timeout or cancellation request) must use `quarantine` instead.
    pub fn fail(&mut self, task: TaskId) -> Result<(), CacheError> {
        if self.quarantined.contains_key(&task.0) {
            return Err(CacheError::FencingRejected {
                reason: "task quarantined",
            });
        }
        let pending = self
            .pending
            .remove(&task.0)
            .ok_or(CacheError::UnknownGeneration)?;
        if let Pending::Write { payload, .. } = pending {
            self.pending_bytes -= payload.len() as u64;
        }
        Ok(())
    }

    pub fn quarantine(&mut self, task: TaskId) -> Result<(), CacheError> {
        if self.quarantined.contains_key(&task.0) {
            return Ok(());
        }
        let pending = self
            .pending
            .remove(&task.0)
            .ok_or(CacheError::UnknownGeneration)?;
        let bytes = pending.write_bytes();
        self.pending_bytes -= bytes;
        self.quarantined_bytes += bytes;
        self.quarantined.insert(task.0, pending);
        Ok(())
    }

    /// Trusted CPU simulation attestation that this task can no longer touch
    /// its buffers. A timer or control-plane restoration is not an attestation.
    /// A hardware backend would need a verified device drain before this call.
    pub fn attest_drained(&mut self, task: TaskId) -> Result<(), CacheError> {
        let pending = self
            .quarantined
            .remove(&task.0)
            .ok_or(CacheError::UnknownGeneration)?;
        self.quarantined_bytes -= pending.write_bytes();
        Ok(())
    }

    /// Hide a tenant's slot. Already queued reads must observe the invalidation
    /// at completion. The slot remains charged until it is safely reclaimed.
    pub fn invalidate(&mut self, tenant: &[u8], slot: u64) -> Result<(), CacheError> {
        self.check_control_and_tenant(tenant)?;
        let stored = self
            .slab
            .slots
            .get_mut(&slot)
            .ok_or(CacheError::UnknownGeneration)?;
        if stored.tenant.as_slice() != tenant {
            return Err(CacheError::Unauthorized);
        }
        stored.exposed = false;
        Ok(())
    }

    /// Reclaim an invalidated CPU slot only when no active or quarantined read
    /// references it. This does not attest a hardware registration drain.
    pub fn reclaim_invalidated(&mut self, tenant: &[u8], slot: u64) -> Result<(), CacheError> {
        self.check_control_and_tenant(tenant)?;
        let stored = self
            .slab
            .slots
            .get(&slot)
            .ok_or(CacheError::UnknownGeneration)?;
        if stored.tenant.as_slice() != tenant {
            return Err(CacheError::Unauthorized);
        }
        if stored.exposed {
            return Err(CacheError::NotReclaimable {
                from: BlockState::Committed,
            });
        }
        let refs = self
            .pending
            .values()
            .chain(self.quarantined.values())
            .filter(|task| task.reads_slot(tenant, slot))
            .count();
        if refs > 0 {
            return Err(CacheError::InUse { refs: refs as u32 });
        }
        let removed = self
            .slab
            .slots
            .remove(&slot)
            .ok_or(CacheError::UnknownGeneration)?;
        self.slab.used -= removed.bytes.len() as u64;
        Ok(())
    }

    fn quarantine_all(&mut self) {
        self.quarantined.append(&mut self.pending);
        self.quarantined_bytes += self.pending_bytes;
        self.pending_bytes = 0;
    }

    pub fn apply_fault(&mut self, fault: Fault) -> DegradeDecision {
        match fault {
            Fault::DpuRestart => {
                // Explicit CPU epoch fence. Keep monotonic ids and do not
                // restore control authority lost before this restart.
                self.pending.clear();
                self.pending_bytes = 0;
                self.quarantined.clear();
                self.quarantined_bytes = 0;
                self.slab.restart();
            }
            Fault::GpuReset | Fault::NicFlap | Fault::StorageLoss => {
                self.quarantine_all();
                self.slab.hide_all();
            }
            Fault::NvmeTimeout => {
                // This fault has no task id, so no outstanding task can be
                // assumed complete. Targeted failures use quarantine(task).
                self.quarantine_all();
            }
            Fault::ControlPlaneLoss => {
                self.quarantine_all();
                self.slab.hide_all();
                self.control_up = false;
            }
            Fault::RustFsSlow | Fault::RustFsUnavailable => {}
        }
        decide(fault)
    }

    pub fn restore_control(&mut self) {
        self.control_up = true;
    }

    fn admit(&self, tenant: &[u8], len: usize) -> Result<(), CacheError> {
        self.check_control_and_tenant(tenant)?;
        if len == 0 {
            return Err(CacheError::Malformed { reason: "dpu task" });
        }
        if len > HARD_BYTES_CAP || len as u64 > self.slab.cap {
            return Err(CacheError::ObjectTooLarge {
                limit: self.slab.cap,
                actual: len as u64,
            });
        }
        if self.pending.len() + self.quarantined.len() >= self.max_inflight
            || self.next_task == u64::MAX
        {
            return Err(CacheError::QuotaExceeded {
                kind: gpucache_core_types::QuotaKind::InFlightRequests,
            });
        }
        Ok(())
    }

    fn check_control_and_tenant(&self, tenant: &[u8]) -> Result<(), CacheError> {
        if !self.control_up {
            return Err(CacheError::Cancelled);
        }
        if tenant.is_empty() || tenant.len() > 128 {
            return Err(CacheError::Malformed { reason: "dpu task" });
        }
        Ok(())
    }

    fn enqueue(&mut self, pending: Pending) -> Result<TaskId, CacheError> {
        let id = self.next_task;
        self.next_task += 1;
        self.pending.insert(id, pending);
        Ok(TaskId(id))
    }

    fn receipt(&self, task: TaskId, slot: Option<u64>, payload: Option<Vec<u8>>) -> Completion {
        Completion {
            task_id: task.0,
            hardware: dpu_probe(),
            simulated: true,
            offload_active: false,
            slot,
            payload,
        }
    }
}

pub fn dpu_feature_selected() -> bool {
    cfg!(feature = "dpu-bf3")
}

pub fn dpu_probe() -> &'static str {
    "NOT_RUN_HARDWARE"
}

pub fn open_nvme_device() -> Result<(), CacheError> {
    let _selected = dpu_feature_selected();
    Err(CacheError::Storage {
        reason: "nvme not linked",
    })
}

pub fn open_doca() -> Result<(), CacheError> {
    let _selected = dpu_feature_selected();
    Err(CacheError::Storage {
        reason: "doca not linked",
    })
}

pub fn open_spdk() -> Result<(), CacheError> {
    let _selected = dpu_feature_selected();
    Err(CacheError::Storage {
        reason: "spdk not linked",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::degrade::all_faults;

    #[test]
    fn arm_start_is_not_offload_and_devices_stay_closed() {
        let agent = ArmAgent::start();
        assert!(agent.process_started());
        assert!(!agent.offload_active());
        assert_eq!(dpu_probe(), "NOT_RUN_HARDWARE");
        for open in [open_nvme_device, open_doca, open_spdk] {
            assert!(matches!(open(), Err(CacheError::Storage { .. })));
        }
        if cfg!(feature = "dpu-bf3") {
            assert!(dpu_feature_selected());
        } else {
            assert!(!dpu_feature_selected());
        }
        assert!(matches!(
            open_nvme_device(),
            Err(CacheError::Storage {
                reason: "nvme not linked"
            })
        ));
    }

    #[test]
    fn simulated_write_then_read_roundtrips_without_claiming_offload() {
        let mut queue = StorageQueue::open_simulated(64, 4).unwrap();
        let write = queue.submit_write(b"tenant-a", b"kv-block").unwrap();
        assert_eq!(queue.inflight(), 1);
        let wrote = queue.complete(write).unwrap();
        assert!(wrote.simulated);
        assert!(!wrote.offload_active);
        assert_eq!(wrote.hardware, "NOT_RUN_HARDWARE");
        assert!(wrote.payload.is_none());
        let slot = wrote.slot.unwrap();
        let read = queue.submit_read(b"tenant-a", slot).unwrap();
        let got = queue.complete(read).unwrap();
        assert_eq!(got.payload.as_deref(), Some(b"kv-block".as_slice()));
        assert_eq!(
            queue.submit_read(b"tenant-b", slot).unwrap_err(),
            CacheError::Unauthorized
        );
    }

    #[test]
    fn failed_write_is_not_readable() {
        let mut queue = StorageQueue::open_simulated(64, 2).unwrap();
        let write = queue.submit_write(b"tenant-a", b"secret-bytes").unwrap();
        queue.fail(write).unwrap();
        assert_eq!(queue.inflight(), 0);
        assert_eq!(
            queue.submit_read(b"tenant-a", 1).unwrap_err(),
            CacheError::Storage {
                reason: "clean miss"
            }
        );
    }

    #[test]
    fn faults_miss_and_then_accept_new_work() {
        for fault in all_faults() {
            let mut queue = StorageQueue::open_simulated(128, 4).unwrap();
            let write = queue.submit_write(b"tenant-a", b"block").unwrap();
            let slot = queue.complete(write).unwrap().slot.unwrap();
            if matches!(fault, Fault::RustFsSlow | Fault::RustFsUnavailable) {
                let before = queue.inflight();
                let decision = queue.apply_fault(*fault);
                assert!(!decision.blocked);
                assert!(!decision.wrong_hit);
                assert_eq!(decision.outcome, "CLEAN_MISS");
                assert_eq!(queue.inflight(), before);
                continue;
            }
            let old_write = queue.submit_write(b"tenant-a", b"inflight").unwrap();
            let decision = queue.apply_fault(*fault);
            assert_eq!(decision.outcome, "CLEAN_MISS");
            assert!(decision.recompute);
            assert!(!decision.wrong_hit);
            assert!(!decision.blocked);
            assert_eq!(queue.inflight(), 0);
            assert!(queue.complete(old_write).is_err());
            if *fault == Fault::ControlPlaneLoss {
                assert!(!queue.control_up());
                assert_eq!(
                    queue.submit_write(b"tenant-a", b"next").unwrap_err(),
                    CacheError::Cancelled
                );
                queue.restore_control();
            }
            if *fault == Fault::NvmeTimeout {
                let read = queue.submit_read(b"tenant-a", slot).unwrap();
                assert_eq!(
                    queue.complete(read).unwrap().payload.as_deref(),
                    Some(b"block".as_slice())
                );
            } else {
                assert!(queue.submit_read(b"tenant-a", slot).is_err());
            }
            let again = queue.submit_write(b"tenant-a", b"fresh").unwrap();
            let done = queue.complete(again).unwrap();
            assert!(done.payload.is_none());
            assert!(!done.offload_active);
        }
    }

    #[test]
    fn checksum_flip_hides_the_slot() {
        let mut queue = StorageQueue::open_simulated(64, 2).unwrap();
        let write = queue.submit_write(b"tenant-a", b"stable").unwrap();
        let slot = queue.complete(write).unwrap().slot.unwrap();
        {
            let stored = queue.slab.slots.get_mut(&slot).unwrap();
            stored.bytes[0] ^= 0xff;
        }
        assert_eq!(
            queue.submit_read(b"tenant-a", slot).unwrap_err(),
            CacheError::ChecksumMismatch
        );
        assert_eq!(
            queue.submit_read(b"tenant-a", slot).unwrap_err(),
            CacheError::Storage {
                reason: "clean miss"
            }
        );
    }
    #[test]
    fn pending_writes_share_the_slab_budget_and_release_credit_on_failure() {
        let mut queue = StorageQueue::open_simulated(8, 4).unwrap();
        let first = queue.submit_write(b"tenant-a", b"12345678").unwrap();
        assert!(matches!(
            queue.submit_write(b"tenant-a", b"x"),
            Err(CacheError::QuotaExceeded { .. })
        ));
        assert_eq!(queue.pending_bytes, 8);
        queue.fail(first).unwrap();
        assert_eq!(queue.pending_bytes, 0);
        let first = queue.submit_write(b"tenant-a", b"12345678").unwrap();
        queue.complete(first).unwrap();
        assert_eq!(queue.pending_bytes, 0);
        assert!(matches!(
            queue.submit_write(b"tenant-a", b"x"),
            Err(CacheError::QuotaExceeded { .. })
        ));
        queue.apply_fault(Fault::DpuRestart);
        queue.submit_write(b"tenant-a", b"12345678").unwrap();
    }

    #[test]
    fn faults_keep_unknown_writes_charged_until_drain_or_restart() {
        for fault in [
            Fault::DpuRestart,
            Fault::GpuReset,
            Fault::NicFlap,
            Fault::StorageLoss,
            Fault::NvmeTimeout,
            Fault::ControlPlaneLoss,
        ] {
            let mut queue = StorageQueue::open_simulated(8, 4).unwrap();
            let first = queue.submit_write(b"tenant-a", b"1234").unwrap();
            queue.complete(first).unwrap();
            let pending = queue.submit_write(b"tenant-a", b"5678").unwrap();
            queue.apply_fault(fault);
            assert_eq!(queue.pending_bytes, 0);
            if fault == Fault::ControlPlaneLoss {
                queue.restore_control();
            }
            if fault == Fault::DpuRestart {
                assert_eq!(queue.quarantined(), 0);
                assert_eq!(queue.retained_bytes(), 0);
            } else {
                assert_eq!(queue.quarantined(), 1);
                assert_eq!(queue.retained_bytes(), 8);
                assert!(queue.submit_write(b"tenant-a", b"x").is_err());
                assert!(queue.complete(pending).is_err());
                assert!(queue.fail(pending).is_err());
                assert_eq!(queue.retained_bytes(), 8);
                queue.attest_drained(pending).unwrap();
                assert_eq!(queue.retained_bytes(), 4);
            }
            queue.submit_write(b"tenant-a", b"5678").unwrap();
        }
    }

    #[test]
    fn timeout_quarantines_every_affected_task_and_late_completion_never_publishes() {
        let mut queue = StorageQueue::open_simulated(8, 2).unwrap();
        let first = queue.submit_write(b"tenant-a", b"1234").unwrap();
        let second = queue.submit_write(b"tenant-b", b"5678").unwrap();
        queue.apply_fault(Fault::NvmeTimeout);
        assert_eq!(queue.inflight(), 0);
        assert_eq!(queue.quarantined(), 2);
        assert_eq!(queue.retained_bytes(), 8);
        for task in [first, second] {
            queue.quarantine(task).unwrap();
            assert_eq!(
                queue.complete(task).unwrap_err(),
                CacheError::FencingRejected {
                    reason: "task quarantined"
                }
            );
        }
        assert!(queue.slab.slots.is_empty());
        assert!(matches!(
            queue.submit_read(b"tenant-a", 1),
            Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightRequests
            })
        ));
        queue.attest_drained(first).unwrap();
        assert_eq!(queue.retained_bytes(), 4);
        let next = queue.submit_write(b"tenant-a", b"abcd").unwrap();
        let slot = queue.complete(next).unwrap().slot.unwrap();
        assert_eq!(queue.slab.slots[&slot].tenant, b"tenant-a");
        assert_eq!(queue.retained_bytes(), 8);
        assert_eq!(
            queue.attest_drained(first).unwrap_err(),
            CacheError::UnknownGeneration
        );
    }

    #[test]
    fn repeated_faults_preserve_exact_quarantine_credit_and_require_a_drain() {
        let mut queue = StorageQueue::open_simulated(8, 2).unwrap();
        let write = queue.submit_write(b"tenant-a", b"12345678").unwrap();
        assert_eq!(
            queue.attest_drained(write).unwrap_err(),
            CacheError::UnknownGeneration
        );
        assert_eq!(queue.inflight(), 1);
        for fault in [
            Fault::NvmeTimeout,
            Fault::NvmeTimeout,
            Fault::NicFlap,
            Fault::StorageLoss,
            Fault::GpuReset,
        ] {
            queue.apply_fault(fault);
            assert_eq!(queue.quarantined(), 1);
            assert_eq!(queue.retained_bytes(), 8);
            assert!(queue.submit_write(b"tenant-a", b"x").is_err());
        }
        queue.attest_drained(write).unwrap();
        assert_eq!(queue.quarantined(), 0);
        assert_eq!(queue.retained_bytes(), 0);
        assert!(queue.complete(write).is_err());
        queue.submit_write(b"tenant-a", b"12345678").unwrap();
    }

    #[test]
    fn unrelated_faults_never_restore_lost_control_authority() {
        let mut queue = StorageQueue::open_simulated(16, 4).unwrap();
        let write = queue.submit_write(b"tenant-a", b"1234").unwrap();
        queue.apply_fault(Fault::ControlPlaneLoss);
        for fault in all_faults() {
            queue.apply_fault(*fault);
            assert!(!queue.control_up());
            assert_eq!(
                queue.submit_write(b"tenant-a", b"x").unwrap_err(),
                CacheError::Cancelled
            );
        }
        queue.restore_control();
        assert!(queue.complete(write).is_err());
        let new = queue.submit_write(b"tenant-a", b"fresh").unwrap();
        queue.complete(new).unwrap();
    }

    #[test]
    fn control_restore_keeps_quarantine_and_hidden_slots() {
        let mut queue = StorageQueue::open_simulated(8, 2).unwrap();
        let write = queue.submit_write(b"tenant-a", b"1234").unwrap();
        let slot = queue.complete(write).unwrap().slot.unwrap();
        let pending = queue.submit_write(b"tenant-a", b"5678").unwrap();
        queue.apply_fault(Fault::ControlPlaneLoss);
        queue.restore_control();
        assert_eq!(queue.quarantined(), 1);
        assert_eq!(queue.retained_bytes(), 8);
        assert!(queue.submit_write(b"tenant-a", b"x").is_err());
        assert!(queue.submit_read(b"tenant-a", slot).is_err());
        assert!(queue.complete(pending).is_err());
    }

    #[test]
    fn invalidation_is_tenant_scoped_and_queued_reads_cannot_escape_it() {
        let mut queue = StorageQueue::open_simulated(8, 4).unwrap();
        let write = queue.submit_write(b"tenant-a", b"12345678").unwrap();
        let slot = queue.complete(write).unwrap().slot.unwrap();
        assert_eq!(
            queue.invalidate(b"tenant-b", slot).unwrap_err(),
            CacheError::Unauthorized
        );
        assert!(queue.slab.slots[&slot].exposed);
        assert_eq!(
            queue.reclaim_invalidated(b"tenant-b", slot).unwrap_err(),
            CacheError::Unauthorized
        );
        assert!(matches!(
            queue.reclaim_invalidated(b"tenant-a", slot),
            Err(CacheError::NotReclaimable { .. })
        ));
        let read = queue.submit_read(b"tenant-a", slot).unwrap();
        queue.invalidate(b"tenant-a", slot).unwrap();
        assert_eq!(
            queue.reclaim_invalidated(b"tenant-a", slot).unwrap_err(),
            CacheError::InUse { refs: 1 }
        );
        assert!(queue.complete(read).is_err());
        queue.reclaim_invalidated(b"tenant-a", slot).unwrap();
        assert_eq!(queue.retained_bytes(), 0);
        let next = queue.submit_write(b"tenant-b", b"new-data").unwrap();
        let new_slot = queue.complete(next).unwrap().slot.unwrap();
        assert_ne!(new_slot, slot);
        assert!(queue.submit_read(b"tenant-a", slot).is_err());
    }

    #[test]
    fn quarantined_reads_prevent_reclamation_until_attested_drained() {
        let mut queue = StorageQueue::open_simulated(8, 2).unwrap();
        let write = queue.submit_write(b"tenant-a", b"12345678").unwrap();
        let slot = queue.complete(write).unwrap().slot.unwrap();
        let read = queue.submit_read(b"tenant-a", slot).unwrap();
        queue.apply_fault(Fault::NicFlap);
        assert_eq!(queue.quarantined(), 1);
        assert_eq!(
            queue.reclaim_invalidated(b"tenant-a", slot).unwrap_err(),
            CacheError::InUse { refs: 1 }
        );
        queue.attest_drained(read).unwrap();
        queue.reclaim_invalidated(b"tenant-a", slot).unwrap();
        assert_eq!(queue.retained_bytes(), 0);
    }

    #[test]
    fn restart_fences_old_task_and_slot_ids_without_reusing_them() {
        let mut queue = StorageQueue::open_simulated(16, 4).unwrap();
        let write = queue.submit_write(b"tenant-a", b"1234").unwrap();
        let old_slot = queue.complete(write).unwrap().slot.unwrap();
        let old_write = queue.submit_write(b"tenant-a", b"5678").unwrap();
        let old_read = queue.submit_read(b"tenant-a", old_slot).unwrap();
        queue.apply_fault(Fault::NicFlap);
        queue.apply_fault(Fault::DpuRestart);
        assert_eq!(queue.retained_bytes(), 0);
        assert_eq!(queue.quarantined(), 0);
        let new_write = queue.submit_write(b"tenant-b", b"new-data").unwrap();
        let new_slot = queue.complete(new_write).unwrap().slot.unwrap();
        assert!(new_write.0 > old_read.0);
        assert!(new_slot > old_slot);
        for task in [write, old_write, old_read] {
            assert_eq!(
                queue.complete(task).unwrap_err(),
                CacheError::UnknownGeneration
            );
        }
        assert!(queue.submit_read(b"tenant-a", old_slot).is_err());
        assert_eq!(queue.retained_bytes(), 8);
    }

    #[test]
    fn queue_and_slot_metadata_have_independent_hard_bounds() {
        assert!(StorageQueue::open_simulated(16, MAX_QUEUE_TASKS + 1).is_err());
        let mut queue = StorageQueue::open_simulated(8192, 2).unwrap();
        for _ in 0..MAX_SLAB_SLOTS - 1 {
            let task = queue.submit_write(b"tenant-a", b"x").unwrap();
            queue.complete(task).unwrap();
        }
        let last = queue.submit_write(b"tenant-a", b"x").unwrap();
        // Reserve the metadata slot when admitting the write, not at complete.
        assert!(queue.submit_write(b"tenant-a", b"x").is_err());
        queue.quarantine(last).unwrap();
        assert!(queue.submit_write(b"tenant-a", b"x").is_err());
        queue.attest_drained(last).unwrap();
        let last = queue.submit_write(b"tenant-a", b"x").unwrap();
        queue.complete(last).unwrap();
        assert_eq!(queue.slab.slots.len(), MAX_SLAB_SLOTS);
        assert!(queue.retained_bytes() < queue.slab.cap);
        assert!(queue.submit_write(b"tenant-a", b"x").is_err());
    }

    #[test]
    fn nonexistent_read_cannot_bind_to_a_future_slot() {
        let mut queue = StorageQueue::open_simulated(8, 2).unwrap();
        assert_eq!(
            queue.submit_read(b"tenant-a", 1).unwrap_err(),
            CacheError::Storage {
                reason: "clean miss"
            }
        );
        assert_eq!(queue.inflight(), 0);
        assert_eq!(queue.next_task, 1);
        let write = queue.submit_write(b"tenant-a", b"new-data").unwrap();
        let slot = queue.complete(write).unwrap().slot.unwrap();
        assert_eq!(slot, 1);
        assert_eq!(queue.inflight(), 0);
        let read = queue.submit_read(b"tenant-a", slot).unwrap();
        assert_eq!(
            queue.complete(read).unwrap().payload.as_deref(),
            Some(b"new-data".as_slice())
        );
        queue.invalidate(b"tenant-a", slot).unwrap();
        assert!(queue.submit_read(b"tenant-a", slot).is_err());
        assert_eq!(queue.inflight(), 0);
    }

    #[test]
    fn checksum_is_rechecked_after_read_admission() {
        let mut queue = StorageQueue::open_simulated(8, 2).unwrap();
        let write = queue.submit_write(b"tenant-a", b"new-data").unwrap();
        let slot = queue.complete(write).unwrap().slot.unwrap();
        let read = queue.submit_read(b"tenant-a", slot).unwrap();
        queue.slab.slots.get_mut(&slot).unwrap().bytes[0] ^= 0xff;
        assert_eq!(
            queue.complete(read).unwrap_err(),
            CacheError::ChecksumMismatch
        );
        assert_eq!(queue.inflight(), 0);
        assert!(!queue.slab.slots[&slot].exposed);
        assert!(queue.submit_read(b"tenant-a", slot).is_err());
    }

    #[test]
    fn malformed_inputs_and_exhausted_ids_do_not_consume_credit() {
        let mut queue = StorageQueue::open_simulated(8, 2).unwrap();
        for tenant in [&b""[..], &[b'a'; 129][..]] {
            assert!(queue.submit_write(tenant, b"x").is_err());
            assert!(queue.submit_read(tenant, 1).is_err());
        }
        assert!(queue.submit_write(b"tenant-a", b"").is_err());
        assert!(queue.submit_write(b"tenant-a", b"123456789").is_err());
        queue.next_task = u64::MAX;
        assert!(queue.submit_write(b"tenant-a", b"x").is_err());
        assert_eq!(queue.inflight(), 0);
        assert_eq!(queue.retained_bytes(), 0);
        queue.next_task = 1;
        queue.slab.next = u64::MAX;
        assert!(queue.submit_write(b"tenant-a", b"x").is_err());
        assert_eq!(queue.retained_bytes(), 0);
        queue.slab.next = u64::MAX - 1;
        let last = queue.submit_write(b"tenant-a", b"x").unwrap();
        assert!(queue.submit_write(b"tenant-a", b"y").is_err());
        assert_eq!(queue.retained_bytes(), 1);
        assert_eq!(queue.complete(last).unwrap().slot, Some(u64::MAX - 1));
        assert!(queue.submit_write(b"tenant-a", b"z").is_err());
    }
}
