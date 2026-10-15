// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Two localhost clients sharing one file-backed GPUCache service.
//!
//! The bytes are synthetic. This is not a vLLM run and not a DPU run.

use std::process::ExitCode;

use gpucache_transport::run_share_demo;

fn main() -> ExitCode {
    match run_share_demo() {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("status=REUSE_FAILED");
            eprintln!("error={err}");
            ExitCode::from(2)
        }
    }
}
