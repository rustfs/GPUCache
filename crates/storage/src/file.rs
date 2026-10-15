// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! File-backed shared KV store.
//!
//! Bytes live in ordinary files under a caller-chosen directory. This module
//! does not open block devices, format disks, or issue NVMe admin commands.
//! A lost or corrupt file is a miss. There is no erasure coding and no replica.

use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use gpucache_core_types::{
    verify_checksum, BlockState, ByteReader, ByteWriter, CacheError, ResourceLimits,
};
use gpucache_protocol::{
    AuthContext, BatchReply, CommitItem, CommitOutcome, GetHit, GetItem, InvalidateItem,
    InvalidateOutcome, ItemResult, LookupItem, LookupOutcome, ReleaseCancelItem, ReleaseOutcome,
    ReserveOutcome, ReservePutItem, TenantPolicy,
};

use crate::memory::{DurableBlock, MemoryStore};

const MANIFEST_MAGIC: &[u8; 4] = b"GMAN";
const MANIFEST_VERSION: u16 = 1;
const BLOCK_MAGIC: &[u8; 4] = b"GBLK";
const BLOCK_VERSION: u16 = 1;
const LRU_MAGIC: &[u8; 4] = b"GLRU";
const LRU_VERSION: u16 = 1;

/// Injected failure for the next durable block write. Production callers leave
/// this unset. It exists so tests can force a short write or a full volume
/// without filling the host disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistFault {
    DiskFull,
    ShortWrite,
}

/// Directory-backed store used by the V1 B TCP service.
///
/// Admission evicts the least-recently-used unpinned `COMMITTED` block when a
/// tenant or global retained-byte quota would otherwise fail. Pinned,
/// quarantined, and in-flight blocks are not evicted. `MemoryStore` itself
/// still refuses admission without evicting; only this backend reclaims.
///
/// A failed durable write is not a hit. The in-memory commit is abandoned and
/// the request id is forgotten, so a replay cannot report success for bytes
/// that never reached the directory.
pub struct FileStore {
    root: PathBuf,
    inner: MemoryStore,
    lru: VecDeque<u64>,
    skipped_corrupt: u32,
    fault: Option<PersistFault>,
    fault_remaining: u32,
}

impl FileStore {
    pub fn open(
        path: impl AsRef<Path>,
        limits: ResourceLimits,
        policies: Vec<TenantPolicy>,
    ) -> Result<Self, CacheError> {
        if policies.is_empty() {
            return Err(CacheError::Unauthorized);
        }
        limits.validate()?;
        let root = prepare_root(path.as_ref())?;
        let mut inner = MemoryStore::new(limits, policies.clone())?;
        let manifest_existed = root.join("MANIFEST").is_file();
        let previous_epoch = read_manifest(&root)?.unwrap_or(1);
        let epoch = if manifest_existed {
            previous_epoch.saturating_add(1).max(1)
        } else {
            1
        };
        inner.restore_epoch(epoch)?;
        let (_loaded, skipped_corrupt, order) = load_blocks(&root, &policies, &mut inner)?;
        let store = Self {
            root,
            inner,
            lru: order,
            skipped_corrupt,
            fault: None,
            fault_remaining: 0,
        };
        store.write_manifest()?;
        store.write_lru()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn epoch(&self) -> u64 {
        self.inner.epoch()
    }

    pub fn limits(&self) -> ResourceLimits {
        self.inner.limits()
    }

    pub fn stats(&self) -> crate::StoreStats {
        self.inner.stats()
    }

    pub fn skipped_corrupt(&self) -> u32 {
        self.skipped_corrupt
    }

    /// Fail the next `times` committed-block writes. `times == 0` clears the hook.
    pub fn inject_persist_fault(&mut self, fault: PersistFault, times: u32) {
        if times == 0 {
            self.fault = None;
            self.fault_remaining = 0;
            return;
        }
        self.fault = Some(fault);
        self.fault_remaining = times;
    }

    pub fn committed_path(&self, generation: u64) -> PathBuf {
        block_path(&self.root, generation)
    }

    pub fn lookup(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<LookupItem>,
    ) -> Result<BatchReply<LookupOutcome>, CacheError> {
        let reply = self.inner.lookup(auth, now_ms, items)?;
        for item in &reply.items {
            if let Ok(
                LookupOutcome::Hit { generation, .. } | LookupOutcome::PrefixHit { generation, .. },
            ) = &item.body
            {
                self.touch(*generation);
            }
        }
        Ok(reply)
    }

    pub fn reserve_put(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<ReservePutItem>,
    ) -> Result<BatchReply<ReserveOutcome>, CacheError> {
        let ids: Vec<_> = items.iter().map(|item| item.header.request_id).collect();
        self.inner.prepare(auth, now_ms, &ids)?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let root = &self.root;
            let lru = &mut self.lru;
            let body = self
                .inner
                .reserve_admitting(auth, now_ms, &item, |inner, extra| {
                    Self::ensure_room(inner, lru, root, auth.tenant.as_bytes(), extra, now_ms)
                });
            out.push(ItemResult {
                request_id: item.header.request_id,
                body,
            });
        }
        Ok(BatchReply {
            store_epoch: self.inner.epoch(),
            items: out,
        })
    }

    pub fn commit(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<CommitItem>,
    ) -> Result<BatchReply<CommitOutcome>, CacheError> {
        let reply = self.inner.commit(auth, now_ms, items)?;
        let mut out = Vec::with_capacity(reply.items.len());
        for result in reply.items {
            let request_id = result.request_id;
            let body = match result.body {
                Ok(outcome) => match self.persist_committed(outcome.generation) {
                    Ok(()) => {
                        self.touch(outcome.generation);
                        Ok(outcome)
                    }
                    Err(err) => {
                        let _ = self.inner.abandon_durable_commit(
                            auth,
                            outcome.generation,
                            request_id,
                            now_ms,
                        );
                        self.delete_block(outcome.generation);
                        self.lru
                            .retain(|generation_in_lru| *generation_in_lru != outcome.generation);
                        Err(err)
                    }
                },
                Err(err) => Err(err),
            };
            out.push(ItemResult { request_id, body });
        }
        let _ = self.write_lru();
        Ok(BatchReply {
            store_epoch: reply.store_epoch,
            items: out,
        })
    }

    pub fn get(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<GetItem>,
    ) -> Result<BatchReply<GetHit>, CacheError> {
        let reply = self.inner.get(auth, now_ms, items)?;
        for item in &reply.items {
            if let Ok(hit) = &item.body {
                self.touch(hit.generation);
            }
        }
        Ok(reply)
    }

    pub fn release_cancel(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<ReleaseCancelItem>,
    ) -> Result<BatchReply<ReleaseOutcome>, CacheError> {
        self.inner.release_cancel(auth, now_ms, items)
    }

    pub fn invalidate(
        &mut self,
        auth: &AuthContext,
        now_ms: u64,
        items: Vec<InvalidateItem>,
    ) -> Result<BatchReply<InvalidateOutcome>, CacheError> {
        let generations: Vec<u64> = items.iter().map(|item| item.generation).collect();
        let reply = self.inner.invalidate(auth, now_ms, items)?;
        for (generation, result) in generations.iter().zip(&reply.items) {
            if result.body.is_ok() {
                self.lru
                    .retain(|generation_in_lru| generation_in_lru != generation);
                self.delete_block(*generation);
            }
        }
        let _ = self.write_lru();
        Ok(reply)
    }

    fn ensure_room(
        inner: &mut MemoryStore,
        lru: &mut VecDeque<u64>,
        root: &Path,
        tenant: &[u8],
        extra: u64,
        now_ms: u64,
    ) -> Result<bool, CacheError> {
        let limits = inner.limits();
        if extra > limits.max_retained_bytes || extra > limits.max_retained_bytes_per_tenant {
            return Ok(false);
        }
        // One snapshot and index per admission, rather than rescanning all
        // slots and linearly searching the snapshot for every victim.
        let candidates: std::collections::HashMap<_, _> = inner
            .eviction_candidates(now_ms)
            .into_iter()
            .map(|candidate| (candidate.generation, candidate))
            .collect();
        let mut global = inner.stats().retained_bytes;
        let mut tenant_used = inner.tenant_retained_bytes(tenant);
        // If the tenant limit is tight, reclaim from that tenant first. Keep
        // skipped entries in their original relative order.
        for tenant_only in [true, false] {
            let mut kept = VecDeque::with_capacity(lru.len());
            while let Some(generation) = lru.pop_front() {
                let global_ok = global.saturating_add(extra) <= limits.max_retained_bytes;
                let tenant_ok =
                    tenant_used.saturating_add(extra) <= limits.max_retained_bytes_per_tenant;
                let needs_room = if tenant_only {
                    !tenant_ok
                } else {
                    !global_ok && tenant_ok
                };
                let Some(candidate) = candidates.get(&generation) else {
                    continue;
                };
                let same_tenant = candidate.tenant.as_slice() == tenant;
                if !needs_room || candidate.pinned || (tenant_only && !same_tenant) {
                    kept.push_back(generation);
                    continue;
                }
                if inner.force_evict_prepared(generation)? == BlockState::Absent {
                    let _ = fs::remove_file(block_path(root, generation));
                    global = global.saturating_sub(candidate.length);
                    if same_tenant {
                        tenant_used = tenant_used.saturating_sub(candidate.length);
                    }
                } else {
                    kept.push_back(generation);
                }
            }
            *lru = kept;
        }
        Ok(true)
    }

    fn persist_committed(&mut self, generation: u64) -> Result<(), CacheError> {
        let block = self.inner.export_committed(generation)?;
        let bytes = encode_block(&block)?;
        let path = block_path(&self.root, generation);
        write_atomic(&path, &bytes, self.take_fault())
    }

    fn take_fault(&mut self) -> Option<PersistFault> {
        let fault = self.fault?;
        self.fault_remaining = self.fault_remaining.saturating_sub(1);
        if self.fault_remaining == 0 {
            self.fault = None;
        }
        Some(fault)
    }

    fn touch(&mut self, generation: u64) {
        self.lru.retain(|existing| *existing != generation);
        self.lru.push_back(generation);
    }

    fn delete_block(&self, generation: u64) {
        let path = block_path(&self.root, generation);
        if path.is_file() {
            let _ = fs::remove_file(path);
        }
    }

    fn write_manifest(&self) -> Result<(), CacheError> {
        let mut w = ByteWriter::with_capacity(16);
        w.raw(MANIFEST_MAGIC);
        w.u16(MANIFEST_VERSION);
        w.u64(self.inner.epoch());
        write_atomic(&self.root.join("MANIFEST"), &w.finish(), None)
    }

    fn write_lru(&self) -> Result<(), CacheError> {
        let mut w = ByteWriter::with_capacity(16 + self.lru.len() * 8);
        w.raw(LRU_MAGIC);
        w.u16(LRU_VERSION);
        let n = u32::try_from(self.lru.len()).unwrap_or(u32::MAX);
        w.u32(n);
        for generation in &self.lru {
            w.u64(*generation);
        }
        write_atomic(&self.root.join("LRU"), &w.finish(), None)
    }
}

fn prepare_root(path: &Path) -> Result<PathBuf, CacheError> {
    reject_path(path)?;
    if path.exists() {
        let meta = fs::metadata(path).map_err(|_| CacheError::Storage { reason: "store io" })?;
        if !meta.is_dir() {
            return Err(CacheError::Malformed {
                reason: "data path",
            });
        }
        reject_special(&meta)?;
    } else {
        fs::create_dir_all(path).map_err(|_| CacheError::Storage { reason: "store io" })?;
    }
    let root = fs::canonicalize(path).map_err(|_| CacheError::Storage { reason: "store io" })?;
    reject_path(&root)?;
    fs::create_dir_all(root.join("blocks"))
        .map_err(|_| CacheError::Storage { reason: "store io" })?;
    Ok(root)
}

fn reject_path(path: &Path) -> Result<(), CacheError> {
    if path.as_os_str().is_empty() || path == Path::new("/") {
        return Err(CacheError::Malformed {
            reason: "data path",
        });
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CacheError::Malformed {
            reason: "data path",
        });
    }
    let text = path.to_string_lossy();
    if text.starts_with("/dev") || text.starts_with("/proc") || text.starts_with("/sys") {
        return Err(CacheError::Malformed {
            reason: "data path",
        });
    }
    Ok(())
}

fn reject_special(meta: &fs::Metadata) -> Result<(), CacheError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let kind = meta.file_type();
        if kind.is_block_device() || kind.is_char_device() || kind.is_fifo() || kind.is_socket() {
            return Err(CacheError::Malformed {
                reason: "data path",
            });
        }
    }
    let _ = meta;
    Ok(())
}

fn read_manifest(root: &Path) -> Result<Option<u64>, CacheError> {
    let path = root.join("MANIFEST");
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = read_capped(&path, 64)?;
    let mut r = ByteReader::new(&bytes);
    let magic = r.raw(4)?;
    if magic != MANIFEST_MAGIC {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    let version = r.u16()?;
    if version != MANIFEST_VERSION {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    let epoch = r.u64()?;
    r.finish()?;
    if epoch == 0 {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    Ok(Some(epoch))
}

fn load_blocks(
    root: &Path,
    policies: &[TenantPolicy],
    inner: &mut MemoryStore,
) -> Result<(u32, u32, VecDeque<u64>), CacheError> {
    let dir = root.join("blocks");
    let mut paths = Vec::new();
    let entries = fs::read_dir(&dir).map_err(|_| CacheError::Storage { reason: "store io" })?;
    for entry in entries {
        let entry = entry.map_err(|_| CacheError::Storage { reason: "store io" })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("gblk") {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    let mut loaded = Vec::new();
    let mut skipped = 0u32;
    let max_file = inner.limits().max_object_bytes.saturating_add(1024 * 1024);
    for path in paths {
        match read_block(&path, max_file) {
            Ok(block) => {
                let allowed = policies.iter().any(|policy| {
                    policy.tenant == block.identity.tenant
                        && policy.trust_domain == block.identity.trust_domain
                });
                let generation = block.generation;
                if allowed && inner.restore_committed(block).is_ok() {
                    loaded.push(generation);
                } else {
                    skipped = skipped.saturating_add(1);
                    reject_file(&path);
                }
            }
            Err(_) => {
                skipped = skipped.saturating_add(1);
                reject_file(&path);
            }
        }
    }
    let order = read_lru(root, &loaded);
    Ok((
        u32::try_from(loaded.len()).unwrap_or(u32::MAX),
        skipped,
        order,
    ))
}

fn read_lru(root: &Path, loaded: &[u64]) -> VecDeque<u64> {
    let path = root.join("LRU");
    let bytes = read_capped(&path, 10 + 1_000_000 * 8).unwrap_or_default();
    let loaded_set: HashSet<_> = loaded.iter().copied().collect();
    let mut seen = HashSet::new();
    let mut order = VecDeque::new();
    if let Some(gens) = decode_lru(&bytes) {
        for generation in gens {
            if loaded_set.contains(&generation) && seen.insert(generation) {
                order.push_back(generation);
            }
        }
    }
    for generation in loaded {
        if seen.insert(*generation) {
            order.push_back(*generation);
        }
    }
    order
}

fn decode_lru(bytes: &[u8]) -> Option<Vec<u64>> {
    let mut r = ByteReader::new(bytes);
    let magic = r.raw(4).ok()?;
    if magic != LRU_MAGIC || r.u16().ok()? != LRU_VERSION {
        return None;
    }
    let n = r.u32().ok()? as usize;
    if n > 1_000_000 {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.u64().ok()?);
    }
    r.finish().ok()?;
    Some(out)
}

fn read_block(path: &Path, max_file: u64) -> Result<DurableBlock, CacheError> {
    let meta = fs::metadata(path).map_err(|_| CacheError::Storage { reason: "store io" })?;
    if meta.len() > max_file {
        return Err(CacheError::ObjectTooLarge {
            limit: max_file,
            actual: meta.len(),
        });
    }
    let bytes = read_capped(path, max_file as usize)?;
    let mut r = ByteReader::new(&bytes);
    let magic = r.raw(4)?;
    if magic != BLOCK_MAGIC {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    if r.u16()? != BLOCK_VERSION {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    let generation = r.u64()?;
    let checksum = gpucache_core_types::Checksum(r.array32()?);
    let identity_bytes = r.bytes(1024 * 1024)?;
    let identity = gpucache_core_types::CacheIdentity::from_bytes(identity_bytes)?;
    let payload = r.bytes(gpucache_core_types::HARD_BYTES_CAP)?.to_vec();
    r.finish()?;
    verify_checksum(&payload, checksum)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if file_name != format!("{generation:016x}.gblk") {
        return Err(CacheError::Storage {
            reason: "store corrupt",
        });
    }
    Ok(DurableBlock {
        generation,
        identity,
        checksum,
        payload,
    })
}

fn encode_block(block: &DurableBlock) -> Result<Vec<u8>, CacheError> {
    verify_checksum(&block.payload, block.checksum)?;
    let identity = block.identity.to_bytes()?;
    let mut w = ByteWriter::with_capacity(64 + identity.len() + block.payload.len());
    w.raw(BLOCK_MAGIC);
    w.u16(BLOCK_VERSION);
    w.u64(block.generation);
    w.raw(&block.checksum.0);
    w.bytes(&identity)?;
    w.bytes(&block.payload)?;
    Ok(w.finish())
}

fn block_path(root: &Path, generation: u64) -> PathBuf {
    root.join("blocks").join(format!("{generation:016x}.gblk"))
}

fn reject_file(path: &Path) {
    let dest = path.with_extension("rejected");
    let _ = fs::rename(path, dest);
}

fn read_capped(path: &Path, max: usize) -> Result<Vec<u8>, CacheError> {
    let mut file = File::open(path).map_err(|_| CacheError::Storage { reason: "store io" })?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = file
            .read(&mut chunk)
            .map_err(|_| CacheError::Storage { reason: "store io" })?;
        if n == 0 {
            break;
        }
        if buf.len().saturating_add(n) > max {
            return Err(CacheError::ObjectTooLarge {
                limit: max as u64,
                actual: buf.len() as u64 + n as u64,
            });
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

fn classify_store_io(err: &std::io::Error) -> CacheError {
    if err.kind() == std::io::ErrorKind::StorageFull || err.raw_os_error() == Some(28) {
        CacheError::Storage {
            reason: "disk full",
        }
    } else {
        CacheError::Storage { reason: "store io" }
    }
}

fn write_atomic(path: &Path, bytes: &[u8], fault: Option<PersistFault>) -> Result<(), CacheError> {
    if matches!(fault, Some(PersistFault::DiskFull)) {
        return Err(CacheError::Storage {
            reason: "disk full",
        });
    }
    let tmp = path.with_extension("tmp");
    let result = write_temp(&tmp, bytes, fault);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(classify_store_io(&err));
    }
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn write_temp(tmp: &Path, bytes: &[u8], fault: Option<PersistFault>) -> Result<(), CacheError> {
    let mut file = File::create(tmp).map_err(|err| classify_store_io(&err))?;
    let slice = if matches!(fault, Some(PersistFault::ShortWrite)) {
        &bytes[..bytes.len() / 2]
    } else {
        bytes
    };
    file.write_all(slice)
        .map_err(|err| classify_store_io(&err))?;
    file.sync_all().map_err(|err| classify_store_io(&err))?;
    if matches!(fault, Some(PersistFault::ShortWrite)) {
        return Err(CacheError::Storage {
            reason: "short write",
        });
    }
    Ok(())
}

#[cfg(test)]
mod io_class_tests {
    use super::classify_store_io;
    use gpucache_core_types::CacheError;

    #[test]
    fn storage_full_and_enospc_map_to_disk_full() {
        let full = std::io::Error::new(std::io::ErrorKind::StorageFull, "full");
        assert_eq!(
            classify_store_io(&full),
            CacheError::Storage {
                reason: "disk full",
            }
        );
        let enospc = std::io::Error::from_raw_os_error(28);
        assert_eq!(
            classify_store_io(&enospc),
            CacheError::Storage {
                reason: "disk full",
            }
        );
        let other = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(
            classify_store_io(&other),
            CacheError::Storage { reason: "store io" }
        );
    }
}
