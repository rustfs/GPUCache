// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Storage-side BlueField-3 slot for GPUCache.
//!
//! The default build and `--features dpu-bf3` both refuse to open DOCA, SPDK,
//! or an NVMe device. `dpu-demo` prints `NOT_RUN_HARDWARE`. An aarch64 build
//! of this crate is a CPU build, not a BlueField test. Starting the Arm
//! process flag is not an offload.

#![forbid(unsafe_code)]

mod ablation;
mod degrade;
mod demo;
mod queue;
mod reactor;
mod target;

pub use ablation::{Ablation, Knob};
pub use degrade::{all_faults, decide, DegradeDecision, Fault};
pub use demo::{run_dpu_demo, DpuReport};
pub use queue::{
    dpu_feature_selected, dpu_probe, open_doca, open_nvme_device, open_spdk, ArmAgent, Completion,
    StorageQueue, TaskId,
};
pub use reactor::{AffineQueue, ReactorId};
pub use target::{planned_targets, PlannedTarget};
