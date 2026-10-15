// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{
    content_checksum, verify_checksum, ByteReader, ByteWriter, CacheError, CacheIdentity, Checksum,
    HARD_BYTES_CAP,
};

const OBJECT_MAGIC: &[u8; 4] = b"GPKG";
const MANIFEST_MAGIC: &[u8; 4] = b"GPMF";
const VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    pub len: u64,
    pub checksum: Checksum,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedBlock {
    pub generation: u64,
    pub identity: CacheIdentity,
    pub checksum: Checksum,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadRange {
    pub generation: u64,
    pub offset: u64,
    pub length: u64,
    pub checksum: Checksum,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pack {
    pub object_key: String,
    pub manifest_key: String,
    pub object_bytes: Vec<u8>,
    pub manifest_bytes: Vec<u8>,
    pub ranges: Vec<PayloadRange>,
}

pub trait ObjectStore {
    fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError>;
    fn head(&self, key: &str) -> Result<ObjectMeta, CacheError>;
    fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError>;
    fn delete(&mut self, key: &str) -> Result<(), CacheError>;
}

pub fn pack_blocks(namespace: &str, blocks: &[PackedBlock]) -> Result<Pack, CacheError> {
    if blocks.is_empty() || blocks.len() > 1024 {
        return Err(CacheError::Malformed {
            reason: "cold pack",
        });
    }
    if !key_ok(namespace) {
        return Err(CacheError::Malformed { reason: "cold key" });
    }
    let mut records = Vec::with_capacity(blocks.len());
    for block in blocks {
        verify_checksum(&block.payload, block.checksum)?;
        if block.payload.len() > HARD_BYTES_CAP {
            return Err(CacheError::ObjectTooLarge {
                limit: HARD_BYTES_CAP as u64,
                actual: block.payload.len() as u64,
            });
        }
        records.push((block, block.identity.to_bytes()?));
    }
    let mut object = ByteWriter::with_capacity(64);
    object.raw(OBJECT_MAGIC);
    object.u16(VERSION);
    object.u32(u32::try_from(records.len()).unwrap_or(u32::MAX));
    let mut ranges = Vec::with_capacity(records.len());
    for (block, identity) in &records {
        object.u64(block.generation);
        object.raw(&block.checksum.0);
        object.bytes(identity)?;
        let offset = object.len() + 4;
        object.bytes(&block.payload)?;
        ranges.push(PayloadRange {
            generation: block.generation,
            offset: offset as u64,
            length: block.payload.len() as u64,
            checksum: block.checksum,
        });
    }
    let object_bytes = object.finish();
    let object_checksum = content_checksum(&object_bytes);
    let object_key = format!("{namespace}/object");
    let manifest_key = format!("{namespace}/manifest");
    let mut manifest = ByteWriter::with_capacity(64);
    manifest.raw(MANIFEST_MAGIC);
    manifest.u16(VERSION);
    manifest.raw(&object_checksum.0);
    manifest.bytes(object_key.as_bytes())?;
    manifest.u32(u32::try_from(ranges.len()).unwrap_or(u32::MAX));
    for range in &ranges {
        manifest.u64(range.generation);
        manifest.u64(range.offset);
        manifest.u64(range.length);
        manifest.raw(&range.checksum.0);
    }
    Ok(Pack {
        object_key,
        manifest_key,
        object_bytes,
        manifest_bytes: manifest.finish(),
        ranges,
    })
}

/// Put, Head, RangeGet, and Delete against a packed object. This is the contract
/// smoke. It does not talk to a network unless `store` does.
pub fn contract_smoke(store: &mut dyn ObjectStore) -> Result<(), CacheError> {
    let blocks = sample_blocks()?;
    let pack = pack_blocks("smoke", &blocks)?;
    store.put(&pack.object_key, &pack.object_bytes)?;
    store.put(&pack.manifest_key, &pack.manifest_bytes)?;
    let object_meta = store.head(&pack.object_key)?;
    if object_meta.len != pack.object_bytes.len() as u64
        || object_meta.checksum != content_checksum(&pack.object_bytes)
    {
        return Err(CacheError::ChecksumMismatch);
    }
    let manifest_meta = store.head(&pack.manifest_key)?;
    if manifest_meta.checksum != content_checksum(&pack.manifest_bytes) {
        return Err(CacheError::ChecksumMismatch);
    }
    let decoded = decode_manifest(&pack.manifest_bytes)?;
    if decoded.len() != pack.ranges.len() {
        return Err(CacheError::Malformed {
            reason: "cold manifest",
        });
    }
    let range = &decoded[1];
    let got = store.range_get(&pack.object_key, range.offset, range.length)?;
    verify_checksum(&got, range.checksum)?;
    if got != blocks[1].payload {
        return Err(CacheError::ChecksumMismatch);
    }
    store.delete(&pack.object_key)?;
    store.delete(&pack.manifest_key)?;
    if store.head(&pack.object_key).is_ok() {
        return Err(CacheError::Storage {
            reason: "object remains",
        });
    }
    Ok(())
}

fn sample_blocks() -> Result<Vec<PackedBlock>, CacheError> {
    let mut out = Vec::new();
    for (generation, byte) in [(1u64, 1u8), (2, 2u8)] {
        let payload = vec![byte; 8];
        let mut identity = gpucache_core_types::synthetic_identity(byte, 1);
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

fn decode_manifest(bytes: &[u8]) -> Result<Vec<PayloadRange>, CacheError> {
    let mut r = ByteReader::new(bytes);
    let magic = r.raw(4)?;
    if magic != MANIFEST_MAGIC || r.u16()? != VERSION {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    let _object_checksum = r.array32()?;
    let _object_key = r.bytes(1024)?;
    let n = r.u32()? as usize;
    if n == 0 || n > 1024 {
        return Err(CacheError::Malformed {
            reason: "cold manifest",
        });
    }
    let mut ranges = Vec::with_capacity(n);
    for _ in 0..n {
        ranges.push(PayloadRange {
            generation: r.u64()?,
            offset: r.u64()?,
            length: r.u64()?,
            checksum: Checksum(r.array32()?),
        });
    }
    r.finish()?;
    Ok(ranges)
}

fn key_ok(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 256
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b'/')
        && !key.contains("//")
        && !key.starts_with('/')
        && !key.ends_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockObjectStore;

    #[test]
    fn mock_put_head_range_get_delete() {
        let mut store = MockObjectStore::default();
        contract_smoke(&mut store).unwrap();
    }
}
