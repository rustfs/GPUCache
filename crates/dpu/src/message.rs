// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Synchronous host-side messages. No IPC, DMA, DOCA, or device ABI is claimed.
//! The caller is the trusted queue owner. There is no extra unbounded inbox.

use gpucache_core_types::CacheError;

use crate::{Completion, DegradeDecision, Fault, StorageQueue, TaskId};

pub enum SimulationMessage<'a> {
    Write { tenant: &'a [u8], payload: &'a [u8] },
    Read { tenant: &'a [u8], slot: u64 },
    Complete(TaskId),
    CancelUnknown(TaskId),
    Drain(TaskId),
    Fault(Fault),
    RestoreControl,
}

#[derive(Debug)]
pub enum SimulationReply {
    Submitted(TaskId),
    Completed(Completion),
    Acknowledged,
    Degraded(DegradeDecision),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimulationStats {
    pub messages: u64,
    pub quarantined_tasks: usize,
    pub retained_bytes: u64,
    pub drain_attestations: u64,
    pub fault_events: u64,
}

pub struct HostSimulation {
    queue: StorageQueue,
    messages: u64,
    drains: u64,
    faults: u64,
}

impl HostSimulation {
    pub fn new(cap_bytes: u64, max_tasks: usize) -> Result<Self, CacheError> {
        Ok(Self {
            queue: StorageQueue::open_simulated(cap_bytes, max_tasks)?,
            messages: 0,
            drains: 0,
            faults: 0,
        })
    }

    pub fn stats(&self) -> SimulationStats {
        SimulationStats {
            messages: self.messages,
            quarantined_tasks: self.queue.quarantined(),
            retained_bytes: self.queue.retained_bytes(),
            drain_attestations: self.drains,
            fault_events: self.faults,
        }
    }

    pub fn handle(
        &mut self,
        message: SimulationMessage<'_>,
    ) -> Result<SimulationReply, CacheError> {
        self.messages = self.messages.saturating_add(1);
        Ok(match message {
            SimulationMessage::Write { tenant, payload } => {
                SimulationReply::Submitted(self.queue.submit_write(tenant, payload)?)
            }
            SimulationMessage::Read { tenant, slot } => {
                SimulationReply::Submitted(self.queue.submit_read(tenant, slot)?)
            }
            SimulationMessage::Complete(task) => {
                SimulationReply::Completed(self.queue.complete(task)?)
            }
            SimulationMessage::CancelUnknown(task) => {
                self.queue.quarantine(task)?;
                SimulationReply::Acknowledged
            }
            SimulationMessage::Drain(task) => {
                self.queue.attest_drained(task)?;
                self.drains = self.drains.saturating_add(1);
                SimulationReply::Acknowledged
            }
            SimulationMessage::Fault(fault) => {
                let decision = self.queue.apply_fault(fault);
                self.faults = self.faults.saturating_add(1);
                SimulationReply::Degraded(decision)
            }
            SimulationMessage::RestoreControl => {
                self.queue.restore_control();
                SimulationReply::Acknowledged
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_storm_keeps_unknown_completion_credit_until_drain() {
        let mut host = HostSimulation::new(8, 1).unwrap();
        let SimulationReply::Submitted(task) = host
            .handle(SimulationMessage::Write {
                tenant: b"a",
                payload: b"12345678",
            })
            .unwrap()
        else {
            panic!("submit reply")
        };
        for _ in 0..1024 {
            host.handle(SimulationMessage::CancelUnknown(task)).unwrap();
            assert!(host.handle(SimulationMessage::Complete(task)).is_err());
            assert!(host
                .handle(SimulationMessage::Write {
                    tenant: b"a",
                    payload: b"x"
                })
                .is_err());
            assert_eq!(host.stats().quarantined_tasks, 1);
            assert_eq!(host.stats().retained_bytes, 8);
        }
        host.handle(SimulationMessage::Drain(task)).unwrap();
        assert_eq!(host.stats().retained_bytes, 0);
        assert_eq!(host.stats().drain_attestations, 1);
        assert!(host.handle(SimulationMessage::Drain(task)).is_err());
    }

    #[test]
    fn every_injected_fault_is_a_payload_free_recompute_decision() {
        for fault in crate::all_faults() {
            let mut host = HostSimulation::new(8, 2).unwrap();
            let SimulationReply::Degraded(decision) =
                host.handle(SimulationMessage::Fault(*fault)).unwrap()
            else {
                panic!("fault reply")
            };
            assert!(decision.recompute);
            assert!(!decision.wrong_hit);
            assert_eq!(host.stats().fault_events, 1);
        }
    }

    #[test]
    fn control_recovery_does_not_drain_quarantined_work() {
        let mut host = HostSimulation::new(8, 1).unwrap();
        let SimulationReply::Submitted(task) = host
            .handle(SimulationMessage::Write {
                tenant: b"a",
                payload: b"12345678",
            })
            .unwrap()
        else {
            panic!("submit reply")
        };
        host.handle(SimulationMessage::Fault(Fault::ControlPlaneLoss))
            .unwrap();
        assert!(host
            .handle(SimulationMessage::Write {
                tenant: b"a",
                payload: b"x"
            })
            .is_err());
        host.handle(SimulationMessage::RestoreControl).unwrap();
        assert_eq!(host.stats().retained_bytes, 8);
        assert_eq!(host.stats().quarantined_tasks, 1);
        assert!(host.handle(SimulationMessage::Complete(task)).is_err());
        host.handle(SimulationMessage::Drain(task)).unwrap();
        assert!(host
            .handle(SimulationMessage::Write {
                tenant: b"a",
                payload: b"x"
            })
            .is_ok());
    }
}
