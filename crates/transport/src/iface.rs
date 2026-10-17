// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{CacheError, QuotaKind, HARD_BYTES_CAP};

/// Which byte path a caller is on. `RdmaNixl` is a named slot. This process
/// does not post RDMA work requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
    Tcp,
    RdmaNixl,
}

/// Whether the receipt came from the CPU contract or from a device that was
/// not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwarePath {
    /// Userspace TCP staging. Not a NIC measurement.
    CpuContract,
    NotRunHardware,
}

/// Bytes the caller already holds. Submitting them is a host copy on TCP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferDesc<'a> {
    pub bytes: &'a [u8],
}

/// Why `submit_preferring_rdma` did not stay on the preferred path.
///
/// `RdmaNotLinked` is the CPU contract for this tree. It is not a NIC retry
/// and it is not a measured fallback latency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    /// The receipt's `kind` is the path that was asked for.
    None,
    /// The RDMA slot refused to open. Bytes were staged on TCP instead.
    RdmaNotLinked,
}

/// What `submit` actually did. `staged_bytes` is the length copied into the
/// host staging buffer. It is not a wire-byte count and not a latency.
/// `attempted` is the path the caller preferred. `kind` is the path that ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferReceipt {
    pub kind: TransportKind,
    pub attempted: TransportKind,
    pub fallback: FallbackReason,
    pub host_copies: u32,
    pub registration_reused: bool,
    pub dma_posted: bool,
    pub hardware: HardwarePath,
    pub staged_bytes: u64,
}

pub trait KvTransport {
    fn kind(&self) -> TransportKind;
    fn submit(&mut self, desc: &TransferDesc<'_>) -> Result<TransferReceipt, CacheError>;
}

/// Default correctness path. `submit` copies into a bounded host buffer and
/// drops it. The cache RPC path remains `TcpClient`.
#[derive(Debug)]
pub struct TcpTransport {
    cap: usize,
    staged_bytes: u64,
    submits: u64,
}

impl TcpTransport {
    pub fn new(cap: usize) -> Result<Self, CacheError> {
        if cap == 0 {
            return Err(CacheError::Malformed { reason: "transfer" });
        }
        if cap > HARD_BYTES_CAP {
            return Err(CacheError::ObjectTooLarge {
                limit: HARD_BYTES_CAP as u64,
                actual: cap as u64,
            });
        }
        Ok(Self {
            cap,
            staged_bytes: 0,
            submits: 0,
        })
    }

    pub fn submits(&self) -> u64 {
        self.submits
    }

    pub fn staged_bytes(&self) -> u64 {
        self.staged_bytes
    }
}

impl KvTransport for TcpTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn submit(&mut self, desc: &TransferDesc<'_>) -> Result<TransferReceipt, CacheError> {
        if desc.bytes.is_empty() {
            return Err(CacheError::Malformed { reason: "transfer" });
        }
        if desc.bytes.len() > self.cap {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            });
        }
        let mut staged = Vec::new();
        staged.extend_from_slice(desc.bytes);
        self.staged_bytes = self.staged_bytes.saturating_add(staged.len() as u64);
        self.submits = self.submits.saturating_add(1);
        drop(staged);
        Ok(TransferReceipt {
            kind: TransportKind::Tcp,
            attempted: TransportKind::Tcp,
            fallback: FallbackReason::None,
            host_copies: 1,
            registration_reused: false,
            dma_posted: false,
            hardware: HardwarePath::CpuContract,
            staged_bytes: desc.bytes.len() as u64,
        })
    }
}

/// Prefer the RDMA slot, then stage on TCP when that slot is not linked.
///
/// This build's `NixlTransport::open` always fails closed. The receipt then
/// names TCP as the path that ran and `RdmaNotLinked` as the reason. `dma_posted`
/// stays false. The function does not invent a NIC retry or a bandwidth number.
pub fn submit_preferring_rdma(
    tcp: &mut TcpTransport,
    desc: &TransferDesc<'_>,
) -> Result<TransferReceipt, CacheError> {
    match NixlTransport::open() {
        Ok(mut nixl) => match nixl.submit(desc) {
            Ok(receipt) => {
                if receipt.dma_posted || receipt.hardware != HardwarePath::NotRunHardware {
                    return Err(CacheError::Storage {
                        reason: "rdma not linked",
                    });
                }
                Ok(TransferReceipt {
                    attempted: TransportKind::RdmaNixl,
                    fallback: FallbackReason::None,
                    ..receipt
                })
            }
            Err(CacheError::Storage {
                reason: "rdma not linked",
            }) => tcp_after_rdma_refusal(tcp, desc),
            Err(err) => Err(err),
        },
        Err(CacheError::Storage {
            reason: "rdma not linked",
        }) => tcp_after_rdma_refusal(tcp, desc),
        Err(err) => Err(err),
    }
}

fn tcp_after_rdma_refusal(
    tcp: &mut TcpTransport,
    desc: &TransferDesc<'_>,
) -> Result<TransferReceipt, CacheError> {
    let mut receipt = tcp.submit(desc)?;
    receipt.attempted = TransportKind::RdmaNixl;
    receipt.kind = TransportKind::Tcp;
    receipt.fallback = FallbackReason::RdmaNotLinked;
    receipt.dma_posted = false;
    receipt.hardware = HardwarePath::CpuContract;
    Ok(receipt)
}

/// NIXL slot. Constructing this value does not open a device.
///
/// `open` fails in every build, including `--features rdma-nixl`. libnixl is
/// not linked. See `docs/adr/0004-nixl-transport.md`.
#[derive(Debug)]
pub struct NixlTransport {
    _unlinked: (),
}

impl NixlTransport {
    pub fn open() -> Result<Self, CacheError> {
        let _selected = rdma_feature_selected();
        Err(CacheError::Storage {
            reason: "rdma not linked",
        })
    }

    #[cfg(test)]
    fn unlinked() -> Self {
        Self { _unlinked: () }
    }
}

impl KvTransport for NixlTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::RdmaNixl
    }

    fn submit(&mut self, _desc: &TransferDesc<'_>) -> Result<TransferReceipt, CacheError> {
        Err(CacheError::Storage {
            reason: "rdma not linked",
        })
    }
}

/// `true` only when the crate is built with `--features rdma-nixl`.
/// The feature does not mean a device was opened.
pub fn rdma_feature_selected() -> bool {
    cfg!(feature = "rdma-nixl")
}

/// Stable label for logs and demos. Never a timing.
pub fn rdma_probe() -> &'static str {
    "NOT_RUN_HARDWARE"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_submit_is_one_host_copy() {
        let mut tcp = TcpTransport::new(64).unwrap();
        let receipt = tcp.submit(&TransferDesc { bytes: b"kv-bytes" }).unwrap();
        assert_eq!(receipt.kind, TransportKind::Tcp);
        assert_eq!(receipt.attempted, TransportKind::Tcp);
        assert_eq!(receipt.fallback, FallbackReason::None);
        assert_eq!(receipt.host_copies, 1);
        assert!(!receipt.dma_posted);
        assert!(!receipt.registration_reused);
        assert_eq!(receipt.hardware, HardwarePath::CpuContract);
        assert_eq!(receipt.staged_bytes, 8);
        assert_eq!(tcp.submits(), 1);
        assert_eq!(tcp.kind(), TransportKind::Tcp);
    }

    #[test]
    fn tcp_submit_backpressures_past_the_cap() {
        let mut tcp = TcpTransport::new(4).unwrap();
        let err = tcp.submit(&TransferDesc { bytes: b"12345" }).unwrap_err();
        assert_eq!(
            err,
            CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            }
        );
        assert_eq!(tcp.submits(), 0);
        assert_eq!(tcp.staged_bytes(), 0);
    }

    #[test]
    fn rdma_open_does_not_post() {
        assert_eq!(rdma_probe(), "NOT_RUN_HARDWARE");
        assert!(matches!(
            NixlTransport::open(),
            Err(CacheError::Storage {
                reason: "rdma not linked"
            })
        ));
        let mut nixl = NixlTransport::unlinked();
        assert_eq!(nixl.kind(), TransportKind::RdmaNixl);
        assert!(matches!(
            nixl.submit(&TransferDesc { bytes: b"x" }),
            Err(CacheError::Storage {
                reason: "rdma not linked"
            })
        ));
    }

    #[test]
    fn rdma_refusal_falls_back_to_tcp_without_posting() {
        let mut tcp = TcpTransport::new(32).unwrap();
        let receipt =
            submit_preferring_rdma(&mut tcp, &TransferDesc { bytes: b"kv-block" }).unwrap();
        assert_eq!(receipt.attempted, TransportKind::RdmaNixl);
        assert_eq!(receipt.kind, TransportKind::Tcp);
        assert_eq!(receipt.fallback, FallbackReason::RdmaNotLinked);
        assert!(!receipt.dma_posted);
        assert_eq!(receipt.hardware, HardwarePath::CpuContract);
        assert_eq!(receipt.host_copies, 1);
        assert_eq!(receipt.staged_bytes, 8);
        assert_eq!(tcp.submits(), 1);
        assert_eq!(rdma_probe(), "NOT_RUN_HARDWARE");
    }

    #[test]
    fn rdma_fallback_keeps_tcp_backpressure() {
        let mut tcp = TcpTransport::new(2).unwrap();
        let err = submit_preferring_rdma(&mut tcp, &TransferDesc { bytes: b"abcd" }).unwrap_err();
        assert_eq!(
            err,
            CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            }
        );
        assert_eq!(tcp.submits(), 0);
        assert_eq!(tcp.staged_bytes(), 0);
    }
}
