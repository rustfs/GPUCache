// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Print the paired-baseline matrix. Every GPU row is NOT_RUN_HARDWARE.
//!
//! This process does not measure TTFT, TPOT, or token goodput.

use std::process::ExitCode;

use gpucache_bench::paired_baseline_report;

fn main() -> ExitCode {
    print!("{}", paired_baseline_report());
    ExitCode::SUCCESS
}
