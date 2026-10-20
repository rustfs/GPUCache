// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! A fake-block ABI exercise. Never reports real engine integration as PASS.
use gpucache_core_types::{content_checksum, synthetic_identity};
use gpucache_protocol::CommittedExport;
use gpucache_storage::{import_committed_block, EngineBlockContract, ENGINE_SPIKE_ABI_VERSION};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let payload = b"FAKE_BLOCK_NOT_ENGINE_OUTPUT".to_vec();
    let block = CommittedExport {
        identity: synthetic_identity(1, 1),
        source_generation: 1,
        checksum: content_checksum(&payload),
        payload,
    };
    let contract = EngineBlockContract::new(
        ENGINE_SPIKE_ABI_VERSION,
        block.identity.clone(),
        block.payload.len() as u64,
    )?;
    let imported = contract.roundtrip(&block)?;
    import_committed_block(&imported, [9; 32], 0)?;
    println!("status=FAKE_BLOCK_CONTRACT_OK");
    println!("engine=vLLM_NOT_LINKED");
    println!("real_export_import=NOT_RUN_HARDWARE");
    println!("gpu_model_run=NOT_RUN_BUDGET");
    println!("performance=NOT_A_PERFORMANCE_RESULT");
    Ok(())
}
