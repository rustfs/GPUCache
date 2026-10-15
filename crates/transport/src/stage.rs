// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::CacheError;

use crate::iface::{KvTransport, TransferDesc, TransferReceipt};
use crate::registered::{RegisteredPool, SimCompletion};

/// Checkout a simulated registration batch, copy the payloads through TCP
/// staging, then complete or quarantine the batch.
///
/// A failed submit quarantines the slots (`SimCompletion::Unknown`). The
/// receipt's `registration_reused` flag reports whether this checkout reused
/// an existing binding. `dma_posted` stays false.
pub fn stage_through_tcp(
    pool: &mut RegisteredPool,
    tcp: &mut impl KvTransport,
    payloads: &[&[u8]],
) -> Result<TransferReceipt, CacheError> {
    if payloads.is_empty() || payloads.len() > 64 {
        return Err(CacheError::Malformed { reason: "transfer" });
    }
    if payloads.iter().any(|payload| payload.is_empty()) {
        return Err(CacheError::Malformed { reason: "transfer" });
    }
    let before = pool.stats().reused_registrations;
    let batch = pool.checkout_batch(payloads.len())?;
    for (id, payload) in batch.slots().iter().copied().zip(payloads.iter().copied()) {
        if let Err(err) = pool.copy_in(id, payload) {
            let _ = pool.complete_batch(&batch, SimCompletion::Unknown);
            return Err(err);
        }
    }
    let mut joined = Vec::new();
    for payload in payloads {
        joined.extend_from_slice(payload);
    }
    let submitted = tcp.submit(&TransferDesc { bytes: &joined });
    let completion = if submitted.is_ok() {
        SimCompletion::Completed
    } else {
        SimCompletion::Unknown
    };
    pool.complete_batch(&batch, completion)?;
    let mut receipt = submitted?;
    receipt.registration_reused = pool.stats().reused_registrations > before;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iface::{HardwarePath, TcpTransport, TransportKind};

    #[test]
    fn stage_reuses_registration_on_the_second_batch() {
        let mut pool = RegisteredPool::new(1, 32).unwrap();
        let mut tcp = TcpTransport::new(64).unwrap();
        let first = stage_through_tcp(&mut pool, &mut tcp, &[b"abcd"]).unwrap();
        assert!(!first.registration_reused);
        assert!(!first.dma_posted);
        assert_eq!(first.kind, TransportKind::Tcp);
        assert_eq!(first.hardware, HardwarePath::CpuContract);
        let second = stage_through_tcp(&mut pool, &mut tcp, &[b"efgh"]).unwrap();
        assert!(second.registration_reused);
        assert_eq!(pool.stats().quarantined, 0);
        assert_eq!(tcp.submits(), 2);
    }

    #[test]
    fn oversized_stage_quarantines_instead_of_freeing() {
        let mut pool = RegisteredPool::new(1, 8).unwrap();
        let mut tcp = TcpTransport::new(2).unwrap();
        let err = stage_through_tcp(&mut pool, &mut tcp, &[b"abcd"]).unwrap_err();
        assert!(matches!(err, CacheError::QuotaExceeded { .. }));
        assert_eq!(pool.stats().quarantined, 1);
        assert_eq!(tcp.submits(), 0);
        assert!(pool.checkout_batch(1).is_err());
    }
}
