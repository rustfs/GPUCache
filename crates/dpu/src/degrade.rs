// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Failure outcomes for the V3 degrade matrix.
//!
//! Every listed fault is a clean miss. The engine recomputes. The decision
//! carries no payload and does not report a wrong hit. `blocked=false` means
//! the caller may fall back to recompute; it does not promise queue admission.
//! Unknown completions retain quarantine credit until explicit drain or a
//! simulated restart fence. Effects live on `StorageQueue::apply_fault`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    DpuRestart,
    GpuReset,
    NicFlap,
    StorageLoss,
    NvmeTimeout,
    ControlPlaneLoss,
    RustFsSlow,
    RustFsUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DegradeDecision {
    pub fault: Fault,
    pub outcome: &'static str,
    pub recompute: bool,
    pub wrong_hit: bool,
    pub blocked: bool,
}

pub fn decide(fault: Fault) -> DegradeDecision {
    DegradeDecision {
        fault,
        outcome: "CLEAN_MISS",
        recompute: true,
        wrong_hit: false,
        blocked: false,
    }
}

pub fn all_faults() -> &'static [Fault] {
    &[
        Fault::DpuRestart,
        Fault::GpuReset,
        Fault::NicFlap,
        Fault::StorageLoss,
        Fault::NvmeTimeout,
        Fault::ControlPlaneLoss,
        Fault::RustFsSlow,
        Fault::RustFsUnavailable,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fault_is_a_clean_miss() {
        for fault in all_faults() {
            let decision = decide(*fault);
            assert_eq!(decision.outcome, "CLEAN_MISS");
            assert!(decision.recompute);
            assert!(!decision.wrong_hit);
            assert!(!decision.blocked);
        }
    }
}
