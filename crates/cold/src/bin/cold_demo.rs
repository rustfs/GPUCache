// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Mock G4 pack, async sink, and cost-bounded refill.
//!
//! Success is `status=COLD_CONTRACT_OK`. Live RustFS stays
//! `NOT_RUN_HARDWARE` unless `GPUCACHE_RUSTFS_*` is set and the probe passes.
//! This is not a GDS run.

use std::process::ExitCode;

use gpucache_cold::run_cold_demo;

fn main() -> ExitCode {
    match run_cold_demo() {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("status=COLD_FAILED");
            eprintln!("error={err}");
            ExitCode::from(2)
        }
    }
}
