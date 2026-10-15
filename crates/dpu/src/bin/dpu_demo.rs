// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! In-process storage-queue contract. Hardware stays NOT_RUN_HARDWARE.
//!
//! `status=DPU_CONTRACT_OK` means the simulated submit/complete path matched
//! bytes. It is not a BlueField run, a DOCA run, or a performance result.

use std::process::ExitCode;

use gpucache_dpu::run_dpu_demo;

fn main() -> ExitCode {
    match run_dpu_demo() {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("status=DPU_FAILED");
            eprintln!("error={err}");
            ExitCode::from(2)
        }
    }
}
