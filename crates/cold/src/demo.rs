// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! CPU contract for the G4 pack, sink, and refill path.
//!
//! A missing `GPUCACHE_RUSTFS_*` environment is `NOT_RUN_HARDWARE`. The mock
//! result is not a measured RustFS or GDS run.

use core::fmt;

use gpucache_core_types::{content_checksum, synthetic_identity, CacheError};

use crate::http::{live_probe, ProbeStatus};
use crate::pack::{contract_smoke, pack_blocks, PackedBlock};
use crate::sink::{
    refill_generation, refill_matching, AsyncSink, ColdJob, RefillBudget, RefillOutcome,
};
use crate::MockObjectStore;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdReport {
    pub live: &'static str,
}

impl fmt::Display for ColdReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "status=COLD_CONTRACT_OK")?;
        writeln!(f, "mock=PASS")?;
        writeln!(f, "async_sink=DRAINED")?;
        writeln!(f, "refill=CHECKSUM_OK")?;
        writeln!(f, "over_budget=NO_BYTES")?;
        writeln!(f, "wrong_tenant=NO_BYTES")?;
        writeln!(f, "rustfs_live={}", self.live)?;
        writeln!(f, "gds=NOT_RUN_HARDWARE")?;
        Ok(())
    }
}

pub fn run_cold_demo() -> Result<ColdReport, CacheError> {
    let mut store = MockObjectStore::default();
    contract_smoke(&mut store)?;
    let blocks = demo_blocks()?;
    let pack = pack_blocks("demo", &blocks)?;
    let mut sink = AsyncSink::new(2, 1024 * 1024)?;
    sink.enqueue(ColdJob::from_pack(&pack))?;
    if sink.drain(&mut store, 1)? != 1 {
        return Err(CacheError::Storage {
            reason: "cold sink",
        });
    }
    let budget = RefillBudget::new(1024 * 1024, 16)?;
    match refill_generation(&store, &pack.manifest_key, 2, &budget)? {
        RefillOutcome::Loaded { payload } if payload == blocks[1].payload => {}
        _ => {
            return Err(CacheError::ChecksumMismatch);
        }
    }
    let tight = RefillBudget::new(1, 16)?;
    if refill_generation(&store, &pack.manifest_key, 2, &tight)? != RefillOutcome::OverBudget {
        return Err(CacheError::Malformed {
            reason: "refill budget",
        });
    }
    let tenant = blocks[1].identity.tenant.as_bytes();
    match refill_matching(&store, &pack.manifest_key, 2, &budget, tenant)? {
        RefillOutcome::Loaded { payload } if payload == blocks[1].payload => {}
        _ => return Err(CacheError::ChecksumMismatch),
    }
    if refill_matching(&store, &pack.manifest_key, 2, &budget, b"other-tenant").is_ok() {
        return Err(CacheError::Unauthorized);
    }
    let live = match live_probe().status {
        ProbeStatus::NotRunHardware { .. } => "NOT_RUN_HARDWARE",
        ProbeStatus::Passed => "PASSED",
        ProbeStatus::Failed { .. } => {
            return Err(CacheError::Storage {
                reason: "object store",
            })
        }
    };
    Ok(ColdReport { live })
}

fn demo_blocks() -> Result<Vec<PackedBlock>, CacheError> {
    let mut out = Vec::new();
    for (generation, byte) in [(1u64, 9u8), (2, 8u8)] {
        let payload = vec![byte; 16];
        let mut identity = synthetic_identity(byte, 1);
        identity.prefix_chain[0].0[0] = byte;
        out.push(PackedBlock {
            generation,
            checksum: content_checksum(&payload),
            payload,
            identity,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_stays_a_mock_without_an_endpoint() {
        let report = run_cold_demo().unwrap();
        let text = report.to_string();
        assert!(text.contains("status=COLD_CONTRACT_OK"));
        assert!(text.contains("over_budget=NO_BYTES"));
        assert!(text.contains("wrong_tenant=NO_BYTES"));
        assert!(text.contains("gds=NOT_RUN_HARDWARE"));
        if std::env::var_os("GPUCACHE_RUSTFS_ENDPOINT").is_none() {
            assert!(text.contains("rustfs_live=NOT_RUN_HARDWARE"));
        }
    }
}
