// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use gpucache_core_types::{
    content_checksum, verify_checksum, ByteReader, ByteWriter, CacheError, CacheIdentity, Checksum,
    HARD_BYTES_CAP,
};

const OBJECT_MAGIC: &[u8; 4] = b"GPKG";
const MANIFEST_MAGIC: &[u8; 4] = b"GPMF";
const OBJECT_VERSION: u16 = 1;
/// Manifest layout this crate will read. Any other version is a miss.
/// Version 3 binds every range to its complete canonical cache identity.
/// Versions 1 and 2 are clean refill misses and must be recomputed.
pub const MANIFEST_VERSION: u16 = 3;

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
    /// Tenant that owns this generation. A refill for a different tenant must
    /// not return the payload.
    pub tenant: Vec<u8>,
    /// SHA-256 of the validated complete canonical CacheIdentity.
    pub identity_digest: Checksum,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub object_checksum: Checksum,
    pub object_key: String,
    pub ranges: Vec<PayloadRange>,
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
    // Validate the final, longer key before building a potentially large pack.
    let _ = content_key(namespace, "manifest", Checksum([0; 32]))?;
    let mut records = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        if blocks[..index]
            .iter()
            .any(|earlier| earlier.generation == block.generation)
        {
            return Err(CacheError::Malformed {
                reason: "cold pack",
            });
        }
        let tenant = block.identity.tenant.as_bytes();
        if tenant.is_empty() || tenant.len() > 128 {
            return Err(CacheError::Malformed { reason: "cold key" });
        }
        if block.payload.is_empty() {
            return Err(CacheError::Malformed {
                reason: "cold pack",
            });
        }
        verify_checksum(&block.payload, block.checksum)?;
        if block.payload.len() > HARD_BYTES_CAP {
            return Err(CacheError::ObjectTooLarge {
                limit: HARD_BYTES_CAP as u64,
                actual: block.payload.len() as u64,
            });
        }
        let digest = identity_digest(&block.identity)?;
        records.push((block, block.identity.to_bytes()?, tenant.to_vec(), digest));
    }
    let mut object = ByteWriter::with_capacity(64);
    object.raw(OBJECT_MAGIC);
    object.u16(OBJECT_VERSION);
    object.u32(u32::try_from(records.len()).unwrap_or(u32::MAX));
    let mut ranges = Vec::with_capacity(records.len());
    for (block, identity, tenant, digest) in &records {
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
            tenant: tenant.clone(),
            identity_digest: *digest,
        });
    }
    let object_bytes = object.finish();
    let object_checksum = content_checksum(&object_bytes);
    let object_key = content_key(namespace, "object", object_checksum)?;
    let mut manifest = ByteWriter::with_capacity(64);
    manifest.raw(MANIFEST_MAGIC);
    manifest.u16(MANIFEST_VERSION);
    manifest.raw(&object_checksum.0);
    manifest.bytes(object_key.as_bytes())?;
    manifest.u32(u32::try_from(ranges.len()).unwrap_or(u32::MAX));
    for range in &ranges {
        manifest.u64(range.generation);
        manifest.u64(range.offset);
        manifest.u64(range.length);
        manifest.raw(&range.checksum.0);
        manifest.bytes(&range.tenant)?;
        manifest.raw(&range.identity_digest.0);
    }
    let manifest_bytes = manifest.finish();
    let manifest_key = content_key(namespace, "manifest", content_checksum(&manifest_bytes))?;
    Ok(Pack {
        object_key,
        manifest_key,
        object_bytes,
        manifest_bytes,
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
    let decoded = read_manifest(&pack.manifest_bytes)?;
    if decoded.object_key != pack.object_key || decoded.ranges.len() != pack.ranges.len() {
        return Err(CacheError::Malformed {
            reason: "cold manifest",
        });
    }
    let range = &decoded.ranges[1];
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

/// Read the manifest version. A version other than [`MANIFEST_VERSION`] is
/// not a hit; [`read_manifest`] rejects it.
pub fn manifest_version(bytes: &[u8]) -> Result<u16, CacheError> {
    let mut r = ByteReader::new(bytes);
    let magic = r.raw(4)?;
    if magic != MANIFEST_MAGIC {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    r.u16()
}

pub fn read_manifest(bytes: &[u8]) -> Result<Manifest, CacheError> {
    let mut r = ByteReader::new(bytes);
    let magic = r.raw(4)?;
    if magic != MANIFEST_MAGIC || r.u16()? != MANIFEST_VERSION {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    let object_checksum = Checksum(r.array32()?);
    let object_key_bytes = r.bytes(1024)?;
    let object_key = std::str::from_utf8(object_key_bytes)
        .map_err(|_| CacheError::Storage {
            reason: "store corrupt",
        })?
        .to_string();
    if !key_ok(&object_key) {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    let n = r.u32()? as usize;
    if n == 0 || n > 1024 {
        return Err(CacheError::Malformed {
            reason: "cold manifest",
        });
    }
    let mut ranges = Vec::with_capacity(n);
    for _ in 0..n {
        let range = PayloadRange {
            generation: r.u64()?,
            offset: r.u64()?,
            length: r.u64()?,
            checksum: Checksum(r.array32()?),
            tenant: r.bytes(128)?.to_vec(),
            identity_digest: Checksum(r.array32()?),
        };
        if range.tenant.is_empty()
            || range.length == 0
            || range.length > HARD_BYTES_CAP as u64
            || range.offset.checked_add(range.length).is_none()
            || ranges
                .iter()
                .any(|earlier: &PayloadRange| earlier.generation == range.generation)
        {
            return Err(CacheError::Malformed {
                reason: "cold manifest",
            });
        }
        ranges.push(range);
    }
    r.finish()?;
    Ok(Manifest {
        object_checksum,
        object_key,
        ranges,
    })
}

pub(crate) fn identity_digest(identity: &CacheIdentity) -> Result<Checksum, CacheError> {
    identity.validate_wire()?;
    Ok(content_checksum(&identity.to_bytes()?))
}

fn digest_hex(checksum: Checksum) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(64);
    for byte in checksum.0 {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 15)]));
    }
    value
}

fn content_key(namespace: &str, kind: &str, checksum: Checksum) -> Result<String, CacheError> {
    let key = format!("{namespace}/{}/{kind}", digest_hex(checksum));
    if !key_ok(namespace) || !key_ok(&key) {
        return Err(CacheError::Malformed { reason: "cold key" });
    }
    Ok(key)
}

fn content_namespace<'a>(
    key: &'a str,
    kind: &str,
    checksum: Checksum,
) -> Result<&'a str, CacheError> {
    let suffix = format!("/{kind}");
    let Some(prefix) = key.strip_suffix(&suffix) else {
        return Err(CacheError::Malformed { reason: "cold key" });
    };
    let Some((namespace, digest)) = prefix.rsplit_once('/') else {
        return Err(CacheError::Malformed { reason: "cold key" });
    };
    if !key_ok(key) || !key_ok(namespace) {
        return Err(CacheError::Malformed { reason: "cold key" });
    }
    if digest != digest_hex(checksum) {
        return Err(CacheError::ChecksumMismatch);
    }
    Ok(namespace)
}

/// Validate the content-addressed manifest and its reference to the object.
pub(crate) fn validate_manifest_binding(
    manifest_key: &str,
    manifest_bytes: &[u8],
    manifest: &Manifest,
) -> Result<(), CacheError> {
    let manifest_namespace =
        content_namespace(manifest_key, "manifest", content_checksum(manifest_bytes))?;
    let object_namespace =
        content_namespace(&manifest.object_key, "object", manifest.object_checksum)?;
    if manifest_namespace != object_namespace {
        return Err(CacheError::Malformed { reason: "cold key" });
    }
    Ok(())
}

/// Reject a manually assembled job that could overwrite another published pack
/// or claim a range/identity absent from its object. No storage calls are made.
pub(crate) fn validate_pack_job(
    object_key: &str,
    object_bytes: &[u8],
    manifest_key: &str,
    manifest_bytes: &[u8],
) -> Result<(), CacheError> {
    let manifest = read_manifest(manifest_bytes)?;
    validate_manifest_binding(manifest_key, manifest_bytes, &manifest)?;
    if manifest.object_key != object_key
        || content_checksum(object_bytes) != manifest.object_checksum
    {
        return Err(CacheError::ChecksumMismatch);
    }
    let mut object = ByteReader::new(object_bytes);
    if object.raw(4)? != OBJECT_MAGIC
        || object.u16()? != OBJECT_VERSION
        || object.u32()? as usize != manifest.ranges.len()
    {
        return Err(CacheError::Malformed {
            reason: "cold pack",
        });
    }
    for range in &manifest.ranges {
        let generation = object.u64()?;
        let checksum = Checksum(object.array32()?);
        let identity = CacheIdentity::from_bytes(object.bytes(HARD_BYTES_CAP)?)?;
        let offset = object_bytes.len() - object.remaining() + 4;
        let payload = object.bytes(HARD_BYTES_CAP)?;
        if generation != range.generation
            || checksum != range.checksum
            || range.offset != offset as u64
            || range.length != payload.len() as u64
            || range.tenant != identity.tenant.as_bytes()
            || range.identity_digest != identity_digest(&identity)?
        {
            return Err(CacheError::Malformed {
                reason: "cold pack",
            });
        }
        verify_checksum(payload, checksum)?;
    }
    object.finish()
}

pub(crate) fn key_ok(key: &str) -> bool {
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

    #[test]
    fn unknown_manifest_version_is_not_a_hit() {
        let blocks = sample_blocks().unwrap();
        let pack = pack_blocks("ver", &blocks).unwrap();
        assert_eq!(
            manifest_version(&pack.manifest_bytes).unwrap(),
            MANIFEST_VERSION
        );
        let mut bad = pack.manifest_bytes.clone();
        bad[4] = 9;
        bad[5] = 0;
        assert_eq!(manifest_version(&bad).unwrap(), 9);
        assert!(matches!(
            read_manifest(&bad),
            Err(CacheError::Storage {
                reason: "store corrupt"
            })
        ));
    }

    #[test]
    fn duplicate_generation_is_rejected() {
        let mut blocks = sample_blocks().unwrap();
        blocks[1].generation = blocks[0].generation;
        assert_eq!(
            pack_blocks("dup", &blocks).unwrap_err(),
            CacheError::Malformed {
                reason: "cold pack"
            }
        );
    }

    #[test]
    fn content_derived_keys_validate_the_final_length() {
        let blocks = sample_blocks().unwrap();
        let pack = pack_blocks(&"n".repeat(182), &blocks).unwrap();
        assert_eq!(pack.manifest_key.len(), 256);
        assert!(pack.object_key.len() <= 256);
        validate_pack_job(
            &pack.object_key,
            &pack.object_bytes,
            &pack.manifest_key,
            &pack.manifest_bytes,
        )
        .unwrap();
        assert_eq!(
            pack_blocks(&"n".repeat(183), &blocks).unwrap_err(),
            CacheError::Malformed { reason: "cold key" }
        );
    }

    #[test]
    fn publication_rejects_forged_identity_even_with_content_derived_keys() {
        let blocks = sample_blocks().unwrap();
        let pack = pack_blocks("forged", &blocks).unwrap();
        let mut manifest_bytes = pack.manifest_bytes.clone();
        // The final field is the last range's complete identity digest.
        *manifest_bytes.last_mut().unwrap() ^= 1;
        let manifest_key =
            content_key("forged", "manifest", content_checksum(&manifest_bytes)).unwrap();
        assert_eq!(
            validate_pack_job(
                &pack.object_key,
                &pack.object_bytes,
                &manifest_key,
                &manifest_bytes,
            )
            .unwrap_err(),
            CacheError::Malformed {
                reason: "cold pack"
            }
        );
    }

    #[test]
    fn publication_rejects_cross_namespace_and_body_substitution() {
        let blocks = sample_blocks().unwrap();
        let pack = pack_blocks("bound", &blocks).unwrap();
        let foreign_manifest = content_key(
            "foreign",
            "manifest",
            content_checksum(&pack.manifest_bytes),
        )
        .unwrap();
        assert!(validate_pack_job(
            &pack.object_key,
            &pack.object_bytes,
            &foreign_manifest,
            &pack.manifest_bytes,
        )
        .is_err());
        let mut wrong_body = pack.object_bytes.clone();
        *wrong_body.last_mut().unwrap() ^= 1;
        assert_eq!(
            validate_pack_job(
                &pack.object_key,
                &wrong_body,
                &pack.manifest_key,
                &pack.manifest_bytes,
            )
            .unwrap_err(),
            CacheError::ChecksumMismatch
        );
    }
}
