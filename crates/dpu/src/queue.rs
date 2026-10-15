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
    content_checksum, verify_checksum, CacheError, Checksum, HARD_BYTES_CAP,
};

use crate::degrade::{decide, DegradeDecision, Fault};

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

    fn insert(&mut self, tenant: &[u8], bytes: &[u8]) -> Result<u64, CacheError> {
        let len = bytes.len() as u64;
        let next = self
            .used
            .checked_add(len)
            .ok_or(CacheError::QuotaExceeded {
                kind: gpucache_core_types::QuotaKind::RetainedBytes,
            })?;
        if next > self.cap || self.next == u64::MAX {
            return Err(CacheError::QuotaExceeded {
                kind: gpucache_core_types::QuotaKind::RetainedBytes,
            });
        }
        let id = self.next;
        self.next += 1;
        self.used = next;
        self.slots.insert(
            id,
            Slot {
                tenant: tenant.to_vec(),
                bytes: bytes.to_vec(),
                checksum: content_checksum(bytes),
                exposed: true,
            },
        );
        Ok(id)
    }

    fn read(&mut self, tenant: &[u8], id: u64) -> Result<Vec<u8>, CacheError> {
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
        Ok(slot.bytes.clone())
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

pub struct StorageQueue {
    slab: Slab,
    pending: BTreeMap<u64, Pending>,
    next_task: u64,
    max_inflight: usize,
    control_up: bool,
}

impl StorageQueue {
    pub fn open_simulated(cap_bytes: u64, max_inflight: usize) -> Result<Self, CacheError> {
        if max_inflight == 0 {
            return Err(CacheError::Malformed { reason: "dpu slab" });
        }
        Ok(Self {
            slab: Slab::new(cap_bytes)?,
            pending: BTreeMap::new(),
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

    pub fn submit_write(&mut self, tenant: &[u8], payload: &[u8]) -> Result<TaskId, CacheError> {
        self.admit(tenant, payload.len())?;
        self.enqueue(Pending::Write {
            tenant: tenant.to_vec(),
            payload: payload.to_vec(),
        })
    }

    pub fn submit_read(&mut self, tenant: &[u8], slot: u64) -> Result<TaskId, CacheError> {
        self.admit(tenant, 1)?;
        self.enqueue(Pending::Read {
            tenant: tenant.to_vec(),
            slot,
        })
    }

    pub fn complete(&mut self, task: TaskId) -> Result<Completion, CacheError> {
        let Some(pending) = self.pending.remove(&task.0) else {
            return Err(CacheError::UnknownGeneration);
        };
        let (slot, payload) = match pending {
            Pending::Write { tenant, payload } => {
                let id = self.slab.insert(&tenant, &payload)?;
                (Some(id), None)
            }
            Pending::Read { tenant, slot } => {
                let bytes = self.slab.read(&tenant, slot)?;
                (Some(slot), Some(bytes))
            }
        };
        Ok(self.receipt(task, slot, payload))
    }

    pub fn fail(&mut self, task: TaskId) -> Result<(), CacheError> {
        if self.pending.remove(&task.0).is_none() {
            return Err(CacheError::UnknownGeneration);
        }
        Ok(())
    }

    pub fn apply_fault(&mut self, fault: Fault) -> DegradeDecision {
        match fault {
            Fault::DpuRestart => {
                self.pending.clear();
                self.slab.restart();
                self.control_up = true;
            }
            Fault::GpuReset | Fault::NicFlap | Fault::StorageLoss => {
                self.pending.clear();
                self.slab.hide_all();
                self.control_up = true;
            }
            Fault::NvmeTimeout => {
                if let Some(id) = self.pending.keys().next().copied() {
                    self.pending.remove(&id);
                }
            }
            Fault::ControlPlaneLoss => {
                self.pending.clear();
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
        if !self.control_up {
            return Err(CacheError::Cancelled);
        }
        if tenant.is_empty() || tenant.len() > 128 || len == 0 {
            return Err(CacheError::Malformed { reason: "dpu task" });
        }
        if len > HARD_BYTES_CAP || len as u64 > self.slab.cap {
            return Err(CacheError::ObjectTooLarge {
                limit: self.slab.cap,
                actual: len as u64,
            });
        }
        if self.pending.len() >= self.max_inflight || self.next_task == u64::MAX {
            return Err(CacheError::QuotaExceeded {
                kind: gpucache_core_types::QuotaKind::InFlightRequests,
            });
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
        let foreign = queue.submit_read(b"tenant-b", slot).unwrap();
        assert_eq!(
            queue.complete(foreign).unwrap_err(),
            CacheError::Unauthorized
        );
    }

    #[test]
    fn failed_write_is_not_readable() {
        let mut queue = StorageQueue::open_simulated(64, 2).unwrap();
        let write = queue.submit_write(b"tenant-a", b"secret-bytes").unwrap();
        queue.fail(write).unwrap();
        assert_eq!(queue.inflight(), 0);
        let read = queue.submit_read(b"tenant-a", 1).unwrap();
        assert_eq!(
            queue.complete(read).unwrap_err(),
            CacheError::Storage {
                reason: "clean miss"
            }
        );
    }

    #[test]
    fn faults_miss_and_then_accept_new_work() {
        let mut queue = StorageQueue::open_simulated(128, 4).unwrap();
        let write = queue.submit_write(b"tenant-a", b"block").unwrap();
        let wrote = queue.complete(write).unwrap();
        let slot = wrote.slot.unwrap();
        for fault in all_faults() {
            if matches!(fault, Fault::RustFsSlow | Fault::RustFsUnavailable) {
                let before = queue.inflight();
                let decision = queue.apply_fault(*fault);
                assert!(!decision.blocked);
                assert!(!decision.wrong_hit);
                assert_eq!(decision.outcome, "CLEAN_MISS");
                assert_eq!(queue.inflight(), before);
                continue;
            }
            let _ = queue.submit_write(b"tenant-a", b"inflight");
            let decision = queue.apply_fault(*fault);
            assert_eq!(decision.outcome, "CLEAN_MISS");
            assert!(decision.recompute);
            assert!(!decision.wrong_hit);
            assert!(!decision.blocked);
            assert_eq!(queue.inflight(), 0);
            if *fault == Fault::ControlPlaneLoss {
                assert!(!queue.control_up());
                assert_eq!(
                    queue.submit_write(b"tenant-a", b"next").unwrap_err(),
                    CacheError::Cancelled
                );
                queue.restore_control();
            }
            let read = queue.submit_read(b"tenant-a", slot).unwrap();
            assert!(queue.complete(read).is_err());
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
        let read = queue.submit_read(b"tenant-a", slot).unwrap();
        assert_eq!(
            queue.complete(read).unwrap_err(),
            CacheError::ChecksumMismatch
        );
        let again = queue.submit_read(b"tenant-a", slot).unwrap();
        assert_eq!(
            queue.complete(again).unwrap_err(),
            CacheError::Storage {
                reason: "clean miss"
            }
        );
    }
}
