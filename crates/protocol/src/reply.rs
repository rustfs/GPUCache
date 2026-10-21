// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{
    BlockState, ByteReader, ByteWriter, CacheError, Checksum, QuotaKind, RequestId, HARD_BYTES_CAP,
};

use crate::api::{
    BatchReply, CommitOutcome, GetHit, InvalidateOutcome, ItemResult, LookupOutcome, MissReason,
    ReleaseOutcome, ReserveOutcome,
};
use crate::codec::{OP_COMMIT, OP_GET, OP_INVALIDATE, OP_LOOKUP, OP_RELEASE, OP_RESERVE};
use crate::negotiate::WIRE_VERSION;

pub const REPLY_MAGIC: &[u8; 4] = b"GCKR";
const OP_FAILED: u16 = 0;

/// Aggregate Get payload allowance after reserving GCKR batch/item metadata.
///
/// Reserve the exact successful-item metadata for every item. Every encoded
/// error is smaller, so failed items cannot consume another item's reservation.
/// This conservative budget must be checked before creating any reader pins.
/// The limit covers the reply body, excluding an outer transport envelope.
pub fn get_reply_payload_budget(
    max_reply_bytes: usize,
    item_count: usize,
) -> Result<u64, CacheError> {
    // Magic/version/op/epoch/count, then request-id/status/GetHit/bytes length.
    const BATCH_BYTES: usize = 4 + 2 + 2 + 8 + 4;
    const ITEM_BYTES: usize = 16 + 1 + 8 + 8 + 32 + 8 + 8 + 4;
    u32::try_from(item_count).map_err(|_| CacheError::QuotaExceeded {
        kind: QuotaKind::BatchItems,
    })?;
    let available = item_count
        .checked_mul(ITEM_BYTES)
        .and_then(|bytes| bytes.checked_add(BATCH_BYTES))
        .and_then(|metadata| max_reply_bytes.checked_sub(metadata))
        .ok_or(CacheError::QuotaExceeded {
            kind: QuotaKind::InFlightBytes,
        })?;
    // ByteWriter::bytes also has an absolute field cap. Keep even an unlimited
    // caller from pinning data whose subsequent reply encoding would fail.
    Ok(available.min(HARD_BYTES_CAP) as u64)
}

/// Encoded store reply. Request frames stay `GCK1`; this is the response side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyBody {
    Lookup(BatchReply<LookupOutcome>),
    Reserve(BatchReply<ReserveOutcome>),
    Commit(BatchReply<CommitOutcome>),
    Get(BatchReply<GetHit>),
    Release(BatchReply<ReleaseOutcome>),
    Invalidate(BatchReply<InvalidateOutcome>),
    Failed {
        op: u16,
        store_epoch: u64,
        error: CacheError,
    },
}

pub fn encode_reply(body: &ReplyBody) -> Result<Vec<u8>, CacheError> {
    let mut w = ByteWriter::with_capacity(64);
    w.raw(REPLY_MAGIC);
    w.u16(WIRE_VERSION);
    match body {
        ReplyBody::Failed {
            op,
            store_epoch,
            error,
        } => {
            w.u16(OP_FAILED);
            w.u16(*op);
            w.u64(*store_epoch);
            write_error(&mut w, error);
        }
        ReplyBody::Lookup(batch) => {
            w.u16(OP_LOOKUP);
            write_batch(&mut w, batch, write_lookup)?;
        }
        ReplyBody::Reserve(batch) => {
            w.u16(OP_RESERVE);
            write_batch(&mut w, batch, write_reserve)?;
        }
        ReplyBody::Commit(batch) => {
            w.u16(OP_COMMIT);
            write_batch(&mut w, batch, write_commit)?;
        }
        ReplyBody::Get(batch) => {
            w.u16(OP_GET);
            write_batch(&mut w, batch, write_get)?;
        }
        ReplyBody::Release(batch) => {
            w.u16(OP_RELEASE);
            write_batch(&mut w, batch, write_release)?;
        }
        ReplyBody::Invalidate(batch) => {
            w.u16(OP_INVALIDATE);
            write_batch(&mut w, batch, write_invalidate)?;
        }
    }
    Ok(w.finish())
}

pub fn decode_reply(bytes: &[u8]) -> Result<ReplyBody, CacheError> {
    let mut r = ByteReader::new(bytes);
    let magic = r.raw(4)?;
    if magic != REPLY_MAGIC {
        return Err(CacheError::Malformed { reason: "magic" });
    }
    let version = r.u16()?;
    if version != WIRE_VERSION {
        return Err(CacheError::WireVersionUnsupported {
            local: WIRE_VERSION,
            remote: version,
        });
    }
    let op = r.u16()?;
    let body = match op {
        OP_FAILED => {
            let failed_op = r.u16()?;
            let store_epoch = r.u64()?;
            let error = read_error(&mut r)?;
            ReplyBody::Failed {
                op: failed_op,
                store_epoch,
                error,
            }
        }
        OP_LOOKUP => ReplyBody::Lookup(read_batch(&mut r, read_lookup)?),
        OP_RESERVE => ReplyBody::Reserve(read_batch(&mut r, read_reserve)?),
        OP_COMMIT => ReplyBody::Commit(read_batch(&mut r, read_commit)?),
        OP_GET => ReplyBody::Get(read_batch(&mut r, read_get)?),
        OP_RELEASE => ReplyBody::Release(read_batch(&mut r, read_release)?),
        OP_INVALIDATE => ReplyBody::Invalidate(read_batch(&mut r, read_invalidate)?),
        _ => {
            return Err(CacheError::Malformed {
                reason: "unknown op",
            })
        }
    };
    r.finish()?;
    Ok(body)
}

fn write_batch<T>(
    w: &mut ByteWriter,
    batch: &BatchReply<T>,
    write_ok: fn(&mut ByteWriter, &T) -> Result<(), CacheError>,
) -> Result<(), CacheError> {
    w.u64(batch.store_epoch);
    let n = u32::try_from(batch.items.len()).map_err(|_| CacheError::Malformed {
        reason: "batch length",
    })?;
    w.u32(n);
    for item in &batch.items {
        w.raw(&item.request_id.0);
        match &item.body {
            Ok(value) => {
                w.u8(1);
                write_ok(w, value)?;
            }
            Err(err) => {
                w.u8(0);
                write_error(w, err);
            }
        }
    }
    Ok(())
}

fn read_batch<T>(
    r: &mut ByteReader<'_>,
    read_ok: fn(&mut ByteReader<'_>) -> Result<T, CacheError>,
) -> Result<BatchReply<T>, CacheError> {
    let store_epoch = r.u64()?;
    let n = r.u32()? as usize;
    if n > 1024 {
        return Err(CacheError::QuotaExceeded {
            kind: QuotaKind::BatchItems,
        });
    }
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let mut id = [0u8; 16];
        id.copy_from_slice(r.raw(16)?);
        let request_id = RequestId::new(id)?;
        let body = match r.u8()? {
            1 => Ok(read_ok(r)?),
            0 => Err(read_error(r)?),
            _ => {
                return Err(CacheError::Malformed {
                    reason: "item status",
                })
            }
        };
        items.push(ItemResult { request_id, body });
    }
    Ok(BatchReply { store_epoch, items })
}

fn write_lookup(w: &mut ByteWriter, value: &LookupOutcome) -> Result<(), CacheError> {
    match value {
        LookupOutcome::Hit {
            generation,
            length,
            checksum,
            valid_length,
        } => {
            w.u8(1);
            w.u64(*generation);
            w.u64(*length);
            w.raw(&checksum.0);
            w.u32(*valid_length);
        }
        LookupOutcome::PrefixHit {
            generation,
            length,
            checksum,
            valid_length,
            matched_prefix_blocks,
        } => {
            w.u8(2);
            w.u64(*generation);
            w.u64(*length);
            w.raw(&checksum.0);
            w.u32(*valid_length);
            w.u32(*matched_prefix_blocks);
        }
        LookupOutcome::Miss { reason } => {
            w.u8(3);
            w.u8(miss_byte(*reason));
        }
    }
    Ok(())
}

fn read_lookup(r: &mut ByteReader<'_>) -> Result<LookupOutcome, CacheError> {
    match r.u8()? {
        1 => Ok(LookupOutcome::Hit {
            generation: r.u64()?,
            length: r.u64()?,
            checksum: Checksum(r.array32()?),
            valid_length: r.u32()?,
        }),
        2 => Ok(LookupOutcome::PrefixHit {
            generation: r.u64()?,
            length: r.u64()?,
            checksum: Checksum(r.array32()?),
            valid_length: r.u32()?,
            matched_prefix_blocks: r.u32()?,
        }),
        3 => Ok(LookupOutcome::Miss {
            reason: miss_reason(r.u8()?)?,
        }),
        _ => Err(CacheError::Malformed {
            reason: "lookup tag",
        }),
    }
}

fn write_reserve(w: &mut ByteWriter, value: &ReserveOutcome) -> Result<(), CacheError> {
    w.u64(value.generation);
    w.u8(state_byte(value.state));
    match value.region_id {
        None => w.u8(0),
        Some(id) => {
            w.u8(1);
            w.u64(id);
        }
    }
    w.u64(value.lease_deadline_ms);
    Ok(())
}

fn read_reserve(r: &mut ByteReader<'_>) -> Result<ReserveOutcome, CacheError> {
    let generation = r.u64()?;
    let state = read_state(r.u8()?)?;
    let region_id = match r.u8()? {
        0 => None,
        1 => Some(r.u64()?),
        _ => {
            return Err(CacheError::Malformed {
                reason: "optional u64",
            })
        }
    };
    Ok(ReserveOutcome {
        generation,
        state,
        region_id,
        lease_deadline_ms: r.u64()?,
    })
}

fn write_commit(w: &mut ByteWriter, value: &CommitOutcome) -> Result<(), CacheError> {
    w.u64(value.generation);
    w.u64(value.length);
    w.raw(&value.checksum.0);
    Ok(())
}

fn read_commit(r: &mut ByteReader<'_>) -> Result<CommitOutcome, CacheError> {
    Ok(CommitOutcome {
        generation: r.u64()?,
        length: r.u64()?,
        checksum: Checksum(r.array32()?),
    })
}

fn write_get(w: &mut ByteWriter, value: &GetHit) -> Result<(), CacheError> {
    w.u64(value.generation);
    w.u64(value.length);
    w.raw(&value.checksum.0);
    w.u64(value.region_id);
    w.u64(value.lease_deadline_ms);
    w.bytes(&value.payload)?;
    Ok(())
}

fn read_get(r: &mut ByteReader<'_>) -> Result<GetHit, CacheError> {
    Ok(GetHit {
        generation: r.u64()?,
        length: r.u64()?,
        checksum: Checksum(r.array32()?),
        region_id: r.u64()?,
        lease_deadline_ms: r.u64()?,
        payload: r.bytes(HARD_BYTES_CAP)?.to_vec(),
    })
}

fn write_release(w: &mut ByteWriter, value: &ReleaseOutcome) -> Result<(), CacheError> {
    w.u64(value.generation);
    w.u8(state_byte(value.state));
    w.u8(u8::from(value.quarantined));
    Ok(())
}

fn read_release(r: &mut ByteReader<'_>) -> Result<ReleaseOutcome, CacheError> {
    Ok(ReleaseOutcome {
        generation: r.u64()?,
        state: read_state(r.u8()?)?,
        quarantined: match r.u8()? {
            0 => false,
            1 => true,
            _ => {
                return Err(CacheError::Malformed {
                    reason: "quarantine flag",
                })
            }
        },
    })
}

fn write_invalidate(w: &mut ByteWriter, value: &InvalidateOutcome) -> Result<(), CacheError> {
    w.u64(value.generation);
    w.u8(state_byte(value.state));
    Ok(())
}

fn read_invalidate(r: &mut ByteReader<'_>) -> Result<InvalidateOutcome, CacheError> {
    Ok(InvalidateOutcome {
        generation: r.u64()?,
        state: read_state(r.u8()?)?,
    })
}

fn write_error(w: &mut ByteWriter, err: &CacheError) {
    match err {
        CacheError::InvalidTransition { from, .. } => {
            w.u16(1);
            w.u8(state_byte(*from));
        }
        CacheError::NotReclaimable { from } => {
            w.u16(2);
            w.u8(state_byte(*from));
        }
        CacheError::InUse { refs } => {
            w.u16(3);
            w.u32(*refs);
        }
        CacheError::FencingRejected { .. } => w.u16(4),
        CacheError::ChecksumMismatch => w.u16(5),
        CacheError::LengthMismatch { expected, actual } => {
            w.u16(6);
            w.u64(*expected);
            w.u64(*actual);
        }
        CacheError::GenerationMismatch { expected, actual } => {
            w.u16(7);
            w.u64(*expected);
            w.u64(*actual);
        }
        CacheError::UnknownGeneration => w.u16(8),
        CacheError::IdempotencyConflict => w.u16(9),
        CacheError::QuotaExceeded { kind } => {
            w.u16(10);
            w.u8(quota_byte(*kind));
        }
        CacheError::ObjectTooLarge { limit, actual } => {
            w.u16(11);
            w.u64(*limit);
            w.u64(*actual);
        }
        CacheError::Unauthorized => w.u16(12),
        CacheError::TimedOut => w.u16(13),
        CacheError::Cancelled => w.u16(14),
        CacheError::Malformed { .. } => w.u16(15),
        CacheError::WireVersionUnsupported { local, remote } => {
            w.u16(16);
            w.u16(*local);
            w.u16(*remote);
        }
        CacheError::NotExposed { state } => {
            w.u16(17);
            w.u8(state_byte(*state));
        }
        CacheError::Incompatible => w.u16(18),
        CacheError::EpochMismatch { store, request } => {
            w.u16(19);
            w.u64(*store);
            w.u64(*request);
        }
        CacheError::IdentityInvalid { .. } => w.u16(20),
        CacheError::Storage { .. } => w.u16(21),
    }
}

fn read_error(r: &mut ByteReader<'_>) -> Result<CacheError, CacheError> {
    Ok(match r.u16()? {
        1 => CacheError::InvalidTransition {
            from: read_state(r.u8()?)?,
            event: "remote",
        },
        2 => CacheError::NotReclaimable {
            from: read_state(r.u8()?)?,
        },
        3 => CacheError::InUse { refs: r.u32()? },
        4 => CacheError::FencingRejected { reason: "remote" },
        5 => CacheError::ChecksumMismatch,
        6 => CacheError::LengthMismatch {
            expected: r.u64()?,
            actual: r.u64()?,
        },
        7 => CacheError::GenerationMismatch {
            expected: r.u64()?,
            actual: r.u64()?,
        },
        8 => CacheError::UnknownGeneration,
        9 => CacheError::IdempotencyConflict,
        10 => CacheError::QuotaExceeded {
            kind: read_quota(r.u8()?)?,
        },
        11 => CacheError::ObjectTooLarge {
            limit: r.u64()?,
            actual: r.u64()?,
        },
        12 => CacheError::Unauthorized,
        13 => CacheError::TimedOut,
        14 => CacheError::Cancelled,
        15 => CacheError::Malformed { reason: "remote" },
        16 => CacheError::WireVersionUnsupported {
            local: r.u16()?,
            remote: r.u16()?,
        },
        17 => CacheError::NotExposed {
            state: read_state(r.u8()?)?,
        },
        18 => CacheError::Incompatible,
        19 => CacheError::EpochMismatch {
            store: r.u64()?,
            request: r.u64()?,
        },
        20 => CacheError::IdentityInvalid { reason: "remote" },
        21 => CacheError::Storage { reason: "remote" },
        _ => {
            return Err(CacheError::Malformed {
                reason: "error code",
            })
        }
    })
}

fn state_byte(state: BlockState) -> u8 {
    match state {
        BlockState::Absent => 0,
        BlockState::Reserved => 1,
        BlockState::Writing => 2,
        BlockState::Committed => 3,
        BlockState::Evicting => 4,
        BlockState::Quarantined => 5,
    }
}

fn read_state(value: u8) -> Result<BlockState, CacheError> {
    Ok(match value {
        0 => BlockState::Absent,
        1 => BlockState::Reserved,
        2 => BlockState::Writing,
        3 => BlockState::Committed,
        4 => BlockState::Evicting,
        5 => BlockState::Quarantined,
        _ => {
            return Err(CacheError::Malformed {
                reason: "block state",
            })
        }
    })
}

fn quota_byte(kind: QuotaKind) -> u8 {
    match kind {
        QuotaKind::InFlightRequests => 0,
        QuotaKind::Registrations => 1,
        QuotaKind::InFlightBytes => 2,
        QuotaKind::RetainedBytes => 3,
        QuotaKind::TenantRetainedBytes => 4,
        QuotaKind::BatchItems => 5,
        QuotaKind::IdempotencyTable => 6,
        QuotaKind::PrefixLinks => 7,
        QuotaKind::RateLimit => 8,
    }
}

fn read_quota(value: u8) -> Result<QuotaKind, CacheError> {
    Ok(match value {
        0 => QuotaKind::InFlightRequests,
        1 => QuotaKind::Registrations,
        2 => QuotaKind::InFlightBytes,
        3 => QuotaKind::RetainedBytes,
        4 => QuotaKind::TenantRetainedBytes,
        5 => QuotaKind::BatchItems,
        6 => QuotaKind::IdempotencyTable,
        7 => QuotaKind::PrefixLinks,
        8 => QuotaKind::RateLimit,
        _ => {
            return Err(CacheError::Malformed {
                reason: "quota kind",
            })
        }
    })
}

fn miss_byte(reason: MissReason) -> u8 {
    match reason {
        MissReason::Absent => 1,
        MissReason::NotCommitted => 2,
        MissReason::Incompatible => 3,
        MissReason::Evicting => 4,
        MissReason::Quarantined => 5,
        MissReason::Expired => 6,
    }
}

fn miss_reason(value: u8) -> Result<MissReason, CacheError> {
    Ok(match value {
        1 => MissReason::Absent,
        2 => MissReason::NotCommitted,
        3 => MissReason::Incompatible,
        4 => MissReason::Evicting,
        5 => MissReason::Quarantined,
        6 => MissReason::Expired,
        _ => {
            return Err(CacheError::Malformed {
                reason: "miss reason",
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpucache_core_types::{content_checksum, BlockState};

    fn id(n: u128) -> RequestId {
        RequestId::from_counter(n).unwrap()
    }

    #[test]
    fn get_budget_matches_the_wire_and_covers_error_metadata() {
        for count in [0, 1, 2, 64, 1024] {
            let batch = BatchReply {
                store_epoch: u64::MAX,
                items: (0..count)
                    .map(|n| ItemResult {
                        request_id: id(n as u128 + 1),
                        body: Ok(GetHit {
                            generation: u64::MAX,
                            length: 3,
                            checksum: content_checksum(b"abc"),
                            region_id: u64::MAX,
                            lease_deadline_ms: u64::MAX,
                            payload: b"abc".to_vec(),
                        }),
                    })
                    .collect(),
            };
            let frame_bytes = encode_reply(&ReplyBody::Get(batch.clone())).unwrap().len();
            assert_eq!(
                get_reply_payload_budget(frame_bytes, count),
                Ok(count as u64 * 3)
            );
            // Include the widest error encoding (two u64 fields) and the
            // quota error emitted when the successful payload would not fit.
            for error in [
                CacheError::EpochMismatch {
                    store: u64::MAX,
                    request: u64::MAX,
                },
                CacheError::QuotaExceeded {
                    kind: QuotaKind::InFlightBytes,
                },
                CacheError::Unauthorized,
            ] {
                let mut denied = batch.clone();
                for item in &mut denied.items {
                    item.body = Err(error.clone());
                }
                assert!(encode_reply(&ReplyBody::Get(denied)).unwrap().len() <= frame_bytes);
            }
        }
        assert!(get_reply_payload_budget(104, 1).is_err());
        assert_eq!(get_reply_payload_budget(105, 1), Ok(0));
        assert!(get_reply_payload_budget(usize::MAX, usize::MAX).is_err());
        assert_eq!(
            get_reply_payload_budget(usize::MAX, 1),
            Ok(HARD_BYTES_CAP as u64)
        );
        let failed = encode_reply(&ReplyBody::Failed {
            op: OP_GET,
            store_epoch: u64::MAX,
            error: CacheError::QuotaExceeded {
                kind: QuotaKind::InFlightBytes,
            },
        })
        .unwrap();
        // Smaller than even the server's minimum 64-byte reply-body ceiling.
        assert_eq!(failed.len(), 21);
        assert!(decode_reply(&failed).is_ok());
    }

    #[test]
    fn reply_roundtrip_covers_hit_prefix_and_failure() {
        let sum = content_checksum(b"abc");
        let lookup = ReplyBody::Lookup(BatchReply {
            store_epoch: 4,
            items: vec![
                ItemResult {
                    request_id: id(1),
                    body: Ok(LookupOutcome::Hit {
                        generation: 9,
                        length: 3,
                        checksum: sum,
                        valid_length: 16,
                    }),
                },
                ItemResult {
                    request_id: id(2),
                    body: Ok(LookupOutcome::PrefixHit {
                        generation: 3,
                        length: 3,
                        checksum: sum,
                        valid_length: 16,
                        matched_prefix_blocks: 1,
                    }),
                },
                ItemResult {
                    request_id: id(3),
                    body: Err(CacheError::Unauthorized),
                },
            ],
        });
        let bytes = encode_reply(&lookup).unwrap();
        assert_eq!(decode_reply(&bytes).unwrap(), lookup);
        assert!(decode_reply(&bytes[..bytes.len() - 1]).is_err());

        let failed = ReplyBody::Failed {
            op: OP_GET,
            store_epoch: 4,
            error: CacheError::EpochMismatch {
                store: 4,
                request: 1,
            },
        };
        let bytes = encode_reply(&failed).unwrap();
        assert_eq!(decode_reply(&bytes).unwrap(), failed);

        let got = ReplyBody::Get(BatchReply {
            store_epoch: 4,
            items: vec![ItemResult {
                request_id: id(8),
                body: Ok(GetHit {
                    generation: 9,
                    length: 3,
                    checksum: sum,
                    region_id: 2,
                    lease_deadline_ms: 50,
                    payload: b"abc".to_vec(),
                }),
            }],
        });
        assert_eq!(decode_reply(&encode_reply(&got).unwrap()).unwrap(), got);

        let reserved = ReplyBody::Reserve(BatchReply {
            store_epoch: 1,
            items: vec![ItemResult {
                request_id: id(4),
                body: Ok(ReserveOutcome {
                    generation: 2,
                    state: BlockState::Writing,
                    region_id: Some(1),
                    lease_deadline_ms: 10,
                }),
            }],
        });
        assert_eq!(
            decode_reply(&encode_reply(&reserved).unwrap()).unwrap(),
            reserved
        );
    }
}
