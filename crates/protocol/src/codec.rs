// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{
    Attestation, ByteReader, ByteWriter, CacheError, CacheIdentity, Checksum, RequestId,
    HARD_BYTES_CAP,
};

use crate::api::{
    CommitItem, GetItem, InvalidateItem, ItemHeader, LookupItem, ReleaseCancelItem, RequestBody,
    ReservePutItem,
};
use crate::auth::AuthContext;
use crate::negotiate::WIRE_VERSION;

pub const WIRE_MAGIC: &[u8; 4] = b"GCK1";
pub const EXPORT_MAGIC: &[u8; 4] = b"GEXP";
pub const EXPORT_VERSION: u16 = 1;

pub const OP_LOOKUP: u16 = 1;
pub const OP_RESERVE: u16 = 2;
pub const OP_COMMIT: u16 = 3;
pub const OP_GET: u16 = 4;
pub const OP_RELEASE: u16 = 5;
pub const OP_INVALIDATE: u16 = 6;
pub const OP_HELLO: u16 = 7;
pub const OP_HELLO_ACK: u16 = 8;

const MAX_ITEMS: usize = 1024;

/// Deadlines are absolute monotonic milliseconds. Zero is rejected.
pub fn ensure_deadline(now_ms: u64, deadline_ms: u64) -> Result<(), CacheError> {
    if deadline_ms == 0 {
        return Err(CacheError::Malformed {
            reason: "deadline required",
        });
    }
    if now_ms >= deadline_ms {
        return Err(CacheError::TimedOut);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedExport {
    pub identity: CacheIdentity,
    pub source_generation: u64,
    pub checksum: Checksum,
    pub payload: Vec<u8>,
}

pub fn encode_request(auth: &AuthContext, body: &RequestBody) -> Result<Vec<u8>, CacheError> {
    let mut w = ByteWriter::with_capacity(128);
    w.raw(WIRE_MAGIC);
    w.u16(WIRE_VERSION);
    w.u16(request_op(body));
    write_auth(&mut w, auth)?;
    w.u32(
        u32::try_from(body.len()).map_err(|_| CacheError::Malformed {
            reason: "batch length",
        })?,
    );
    match body {
        RequestBody::Lookup(items) => {
            for item in items {
                write_header(&mut w, &item.header)?;
                write_identity(&mut w, &item.identity)?;
            }
        }
        RequestBody::ReservePut(items) => {
            for item in items {
                write_header(&mut w, &item.header)?;
                write_identity(&mut w, &item.identity)?;
                w.u64(item.generation);
                w.u64(item.length);
                w.raw(&item.checksum.0);
                write_payload(&mut w, item.payload.as_deref())?;
                w.u64(item.lease_ttl_ms);
            }
        }
        RequestBody::Commit(items) => {
            for item in items {
                write_header(&mut w, &item.header)?;
                w.u64(item.generation);
                w.u64(item.length);
                w.raw(&item.checksum.0);
            }
        }
        RequestBody::Get(items) => {
            for item in items {
                write_header(&mut w, &item.header)?;
                w.u64(item.generation);
                write_identity(&mut w, &item.identity)?;
                w.u64(item.pin_ttl_ms);
            }
        }
        RequestBody::ReleaseCancel(items) => {
            for item in items {
                write_header(&mut w, &item.header)?;
                w.u64(item.generation);
                write_opt_u64(&mut w, item.region_id);
                write_opt_request(&mut w, item.pin_owner)?;
                w.u8(attestation_byte(item.attestation));
            }
        }
        RequestBody::Invalidate(items) => {
            for item in items {
                write_header(&mut w, &item.header)?;
                w.u64(item.generation);
            }
        }
    }
    Ok(w.finish())
}

pub fn decode_request(bytes: &[u8]) -> Result<(AuthContext, RequestBody), CacheError> {
    let mut r = ByteReader::new(bytes);
    expect_magic(&mut r, WIRE_MAGIC)?;
    let version = r.u16()?;
    if version != WIRE_VERSION {
        return Err(CacheError::WireVersionUnsupported {
            local: WIRE_VERSION,
            remote: version,
        });
    }
    let op = r.u16()?;
    let auth = read_auth(&mut r)?;
    let n = r.u32()? as usize;
    if n > MAX_ITEMS {
        return Err(CacheError::QuotaExceeded {
            kind: gpucache_core_types::QuotaKind::BatchItems,
        });
    }
    let body = match op {
        OP_LOOKUP => {
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(LookupItem {
                    header: read_header(&mut r)?,
                    identity: read_identity(&mut r)?,
                });
            }
            RequestBody::Lookup(items)
        }
        OP_RESERVE => {
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(ReservePutItem {
                    header: read_header(&mut r)?,
                    identity: read_identity(&mut r)?,
                    generation: r.u64()?,
                    length: r.u64()?,
                    checksum: Checksum(r.array32()?),
                    payload: read_payload(&mut r)?,
                    lease_ttl_ms: r.u64()?,
                });
            }
            RequestBody::ReservePut(items)
        }
        OP_COMMIT => {
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(CommitItem {
                    header: read_header(&mut r)?,
                    generation: r.u64()?,
                    length: r.u64()?,
                    checksum: Checksum(r.array32()?),
                });
            }
            RequestBody::Commit(items)
        }
        OP_GET => {
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(GetItem {
                    header: read_header(&mut r)?,
                    generation: r.u64()?,
                    identity: read_identity(&mut r)?,
                    pin_ttl_ms: r.u64()?,
                });
            }
            RequestBody::Get(items)
        }
        OP_RELEASE => {
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(ReleaseCancelItem {
                    header: read_header(&mut r)?,
                    generation: r.u64()?,
                    region_id: read_opt_u64(&mut r)?,
                    pin_owner: read_opt_request(&mut r)?,
                    attestation: read_attestation(r.u8()?)?,
                });
            }
            RequestBody::ReleaseCancel(items)
        }
        OP_INVALIDATE => {
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(InvalidateItem {
                    header: read_header(&mut r)?,
                    generation: r.u64()?,
                });
            }
            RequestBody::Invalidate(items)
        }
        _ => {
            return Err(CacheError::Malformed {
                reason: "unknown op",
            })
        }
    };
    r.finish()?;
    Ok((auth, body))
}

pub fn encode_hello(versions: &[u16]) -> Result<Vec<u8>, CacheError> {
    if versions.len() > 32 {
        return Err(CacheError::Malformed {
            reason: "hello versions",
        });
    }
    let mut w = ByteWriter::new();
    w.raw(WIRE_MAGIC);
    w.u16(WIRE_VERSION);
    w.u16(OP_HELLO);
    w.u32(versions.len() as u32);
    for v in versions {
        w.u16(*v);
    }
    Ok(w.finish())
}

pub fn decode_hello(bytes: &[u8]) -> Result<Vec<u16>, CacheError> {
    let mut r = ByteReader::new(bytes);
    expect_magic(&mut r, WIRE_MAGIC)?;
    let version = r.u16()?;
    if version != WIRE_VERSION {
        return Err(CacheError::WireVersionUnsupported {
            local: WIRE_VERSION,
            remote: version,
        });
    }
    if r.u16()? != OP_HELLO {
        return Err(CacheError::Malformed { reason: "hello op" });
    }
    let n = r.u32()? as usize;
    if n == 0 || n > 32 {
        return Err(CacheError::Malformed {
            reason: "hello versions",
        });
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.u16()?);
    }
    r.finish()?;
    Ok(out)
}

pub fn encode_hello_ack(selected: u16, epoch: u64) -> Vec<u8> {
    let mut w = ByteWriter::new();
    w.raw(WIRE_MAGIC);
    w.u16(WIRE_VERSION);
    w.u16(OP_HELLO_ACK);
    w.u16(selected);
    w.u64(epoch);
    w.finish()
}

pub fn decode_hello_ack(bytes: &[u8]) -> Result<(u16, u64), CacheError> {
    let mut r = ByteReader::new(bytes);
    expect_magic(&mut r, WIRE_MAGIC)?;
    let version = r.u16()?;
    if version != WIRE_VERSION {
        return Err(CacheError::WireVersionUnsupported {
            local: WIRE_VERSION,
            remote: version,
        });
    }
    if r.u16()? != OP_HELLO_ACK {
        return Err(CacheError::Malformed {
            reason: "hello ack op",
        });
    }
    let selected = r.u16()?;
    let epoch = r.u64()?;
    r.finish()?;
    Ok((selected, epoch))
}

pub fn encode_export(frame: &CommittedExport) -> Result<Vec<u8>, CacheError> {
    if frame.payload.len() as u64 > HARD_BYTES_CAP as u64 {
        return Err(CacheError::ObjectTooLarge {
            limit: HARD_BYTES_CAP as u64,
            actual: frame.payload.len() as u64,
        });
    }
    gpucache_core_types::verify_checksum(&frame.payload, frame.checksum)?;
    let mut w = ByteWriter::with_capacity(64 + frame.payload.len());
    w.raw(EXPORT_MAGIC);
    w.u16(EXPORT_VERSION);
    write_identity(&mut w, &frame.identity)?;
    w.u64(frame.source_generation);
    w.raw(&frame.checksum.0);
    w.bytes(&frame.payload)?;
    Ok(w.finish())
}

pub fn decode_export(bytes: &[u8]) -> Result<CommittedExport, CacheError> {
    let mut r = ByteReader::new(bytes);
    expect_magic(&mut r, EXPORT_MAGIC)?;
    let version = r.u16()?;
    if version != EXPORT_VERSION {
        return Err(CacheError::WireVersionUnsupported {
            local: EXPORT_VERSION,
            remote: version,
        });
    }
    let identity = read_identity(&mut r)?;
    let source_generation = r.u64()?;
    let checksum = Checksum(r.array32()?);
    let payload = r.bytes(HARD_BYTES_CAP)?.to_vec();
    r.finish()?;
    gpucache_core_types::verify_checksum(&payload, checksum)?;
    Ok(CommittedExport {
        identity,
        source_generation,
        checksum,
        payload,
    })
}

pub fn request_op(body: &RequestBody) -> u16 {
    match body {
        RequestBody::Lookup(_) => OP_LOOKUP,
        RequestBody::ReservePut(_) => OP_RESERVE,
        RequestBody::Commit(_) => OP_COMMIT,
        RequestBody::Get(_) => OP_GET,
        RequestBody::ReleaseCancel(_) => OP_RELEASE,
        RequestBody::Invalidate(_) => OP_INVALIDATE,
    }
}

fn write_auth(w: &mut ByteWriter, auth: &AuthContext) -> Result<(), CacheError> {
    w.bytes(auth.tenant.as_bytes())?;
    w.bytes(auth.trust_domain.as_bytes())?;
    w.raw(&auth.credential_fingerprint);
    Ok(())
}

fn read_auth(r: &mut ByteReader<'_>) -> Result<AuthContext, CacheError> {
    let tenant = gpucache_core_types::TenantId::new(r.bytes(4096)?.to_vec())?;
    let trust = gpucache_core_types::TrustDomain::new(r.bytes(4096)?.to_vec())?;
    let credential_fingerprint = r.array32()?;
    AuthContext::new(tenant, trust, credential_fingerprint)
}

fn write_header(w: &mut ByteWriter, header: &ItemHeader) -> Result<(), CacheError> {
    w.raw(&header.request_id.0);
    w.u64(header.epoch);
    w.u64(header.deadline_ms);
    Ok(())
}

fn read_header(r: &mut ByteReader<'_>) -> Result<ItemHeader, CacheError> {
    let mut id = [0u8; 16];
    id.copy_from_slice(r.raw(16)?);
    Ok(ItemHeader {
        request_id: RequestId::new(id)?,
        epoch: r.u64()?,
        deadline_ms: r.u64()?,
    })
}

fn write_identity(w: &mut ByteWriter, id: &CacheIdentity) -> Result<(), CacheError> {
    w.bytes(&id.to_bytes()?)
}

fn read_identity(r: &mut ByteReader<'_>) -> Result<CacheIdentity, CacheError> {
    let bytes = r.bytes(1024 * 1024)?;
    CacheIdentity::from_bytes(bytes)
}

fn write_payload(w: &mut ByteWriter, payload: Option<&[u8]>) -> Result<(), CacheError> {
    match payload {
        None => {
            w.u8(0);
            Ok(())
        }
        Some(bytes) => {
            w.u8(1);
            w.bytes(bytes)
        }
    }
}

fn read_payload(r: &mut ByteReader<'_>) -> Result<Option<Vec<u8>>, CacheError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(r.bytes(HARD_BYTES_CAP)?.to_vec())),
        _ => Err(CacheError::Malformed {
            reason: "payload tag",
        }),
    }
}

fn write_opt_u64(w: &mut ByteWriter, value: Option<u64>) {
    match value {
        None => w.u8(0),
        Some(v) => {
            w.u8(1);
            w.u64(v);
        }
    }
}

fn read_opt_u64(r: &mut ByteReader<'_>) -> Result<Option<u64>, CacheError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(r.u64()?)),
        _ => Err(CacheError::Malformed {
            reason: "optional u64",
        }),
    }
}

fn write_opt_request(w: &mut ByteWriter, value: Option<RequestId>) -> Result<(), CacheError> {
    match value {
        None => w.u8(0),
        Some(id) => {
            w.u8(1);
            w.raw(&id.0);
        }
    }
    Ok(())
}

fn read_opt_request(r: &mut ByteReader<'_>) -> Result<Option<RequestId>, CacheError> {
    match r.u8()? {
        0 => Ok(None),
        1 => {
            let mut id = [0u8; 16];
            id.copy_from_slice(r.raw(16)?);
            Ok(Some(RequestId::new(id)?))
        }
        _ => Err(CacheError::Malformed {
            reason: "optional request",
        }),
    }
}

fn attestation_byte(value: Attestation) -> u8 {
    match value {
        Attestation::KnownIdle => 1,
        Attestation::Unknown => 2,
        Attestation::Drained => 3,
    }
}

fn read_attestation(value: u8) -> Result<Attestation, CacheError> {
    match value {
        1 => Ok(Attestation::KnownIdle),
        2 => Ok(Attestation::Unknown),
        3 => Ok(Attestation::Drained),
        _ => Err(CacheError::Malformed {
            reason: "attestation",
        }),
    }
}

fn expect_magic(r: &mut ByteReader<'_>, magic: &[u8; 4]) -> Result<(), CacheError> {
    let got = r.raw(4)?;
    if got != magic {
        return Err(CacheError::Malformed { reason: "magic" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negotiate::negotiate;
    use gpucache_core_types::{content_checksum, synthetic_identity, TenantId, TrustDomain};

    fn auth() -> AuthContext {
        AuthContext::new(
            TenantId::new(b"tenant-a".to_vec()).unwrap(),
            TrustDomain::new(b"td-local".to_vec()).unwrap(),
            [7u8; 32],
        )
        .unwrap()
    }

    fn header(n: u128) -> ItemHeader {
        ItemHeader {
            request_id: RequestId::from_counter(n).unwrap(),
            epoch: 1,
            deadline_ms: 50,
        }
    }

    #[test]
    fn request_roundtrip_and_truncated_frame_fail() {
        let payload = b"synthetic-kv".to_vec();
        let body = RequestBody::ReservePut(vec![ReservePutItem {
            header: header(1),
            identity: synthetic_identity(1, 1),
            generation: 0,
            length: payload.len() as u64,
            checksum: content_checksum(&payload),
            payload: Some(payload.clone()),
            lease_ttl_ms: 20,
        }]);
        let bytes = encode_request(&auth(), &body).unwrap();
        let (got_auth, got_body) = decode_request(&bytes).unwrap();
        assert_eq!(got_auth, auth());
        assert_eq!(got_body, body);
        assert!(decode_request(&bytes[..bytes.len() - 1]).is_err());
        let mut flipped = bytes.clone();
        flipped[0] ^= 0xff;
        assert!(decode_request(&flipped).is_err());
    }

    #[test]
    fn export_rejects_a_bad_checksum_without_returning_bytes() {
        let payload = b"block-bytes".to_vec();
        let frame = CommittedExport {
            identity: synthetic_identity(2, 1),
            source_generation: 4,
            checksum: content_checksum(&payload),
            payload: payload.clone(),
        };
        let bytes = encode_export(&frame).unwrap();
        let back = decode_export(&bytes).unwrap();
        assert_eq!(back.payload, payload);
        let mut bad = bytes.clone();
        let idx = bad.len() - 1;
        bad[idx] ^= 0x01;
        assert_eq!(decode_export(&bad), Err(CacheError::ChecksumMismatch));
    }

    #[test]
    fn hello_negotiates_and_deadlines_fail_closed() {
        let hello = encode_hello(&[1]).unwrap();
        let remote = decode_hello(&hello).unwrap();
        assert_eq!(negotiate(&[1], &remote).unwrap(), 1);
        let ack = encode_hello_ack(1, 1);
        assert_eq!(decode_hello_ack(&ack).unwrap(), (1, 1));
        assert_eq!(ensure_deadline(10, 10), Err(CacheError::TimedOut));
        assert!(ensure_deadline(10, 0).is_err());
        assert!(ensure_deadline(10, 11).is_ok());
    }

    #[test]
    fn every_batch_op_roundtrips() {
        let id = synthetic_identity(4, 2);
        let cases = vec![
            RequestBody::Lookup(vec![LookupItem {
                header: header(2),
                identity: id.clone(),
            }]),
            RequestBody::Commit(vec![CommitItem {
                header: header(3),
                generation: 9,
                length: 4,
                checksum: content_checksum(b"abcd"),
            }]),
            RequestBody::Get(vec![GetItem {
                header: header(4),
                generation: 9,
                identity: id.clone(),
                pin_ttl_ms: 5,
            }]),
            RequestBody::ReleaseCancel(vec![ReleaseCancelItem {
                header: header(5),
                generation: 9,
                region_id: Some(3),
                pin_owner: Some(RequestId::from_counter(4).unwrap()),
                attestation: Attestation::Drained,
            }]),
            RequestBody::Invalidate(vec![InvalidateItem {
                header: header(6),
                generation: 9,
            }]),
        ];
        for body in cases {
            let bytes = encode_request(&auth(), &body).unwrap();
            let (_, back) = decode_request(&bytes).unwrap();
            assert_eq!(back, body);
        }
    }
}
