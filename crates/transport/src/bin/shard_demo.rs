// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! One shard, simulated registration, TCP staging. Not an RDMA run.

use std::process::ExitCode;

use gpucache_transport::run_shard_demo;

fn main() -> ExitCode {
    match run_shard_demo() {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("status=SHARD_FAILED");
            eprintln!("error={err}");
            ExitCode::from(2)
        }
    }
}
