// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Hit, miss, and recompute fallback from a clean temp directory.
//!
//! The bytes are synthetic token ids. This is not a vLLM run and not a DPU run.
//! A failed check exits non-zero and does not print `status=FALLBACK_OK`.

use std::process::ExitCode;

use gpucache_transport::run_fault_demo;

fn main() -> ExitCode {
    match run_fault_demo() {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("status=FALLBACK_FAILED");
            eprintln!("error={err}");
            ExitCode::from(2)
        }
    }
}
