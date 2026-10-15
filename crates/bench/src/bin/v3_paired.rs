// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! V3 paired-experiment placeholder.
//!
//! The required baseline is a normal NIC RDMA path, not TCP. This process
//! prints `NOT_RUN_HARDWARE`, `TCO_UNKNOWN`, and `go_gate=NOT_EVALUATED`.
//! It does not measure TTFT, watts, or token goodput.

use std::process::ExitCode;

use gpucache_bench::v3_paired_report;

fn main() -> ExitCode {
    print!("{}", v3_paired_report());
    ExitCode::SUCCESS
}
