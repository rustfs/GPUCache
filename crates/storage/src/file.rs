// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! File-backed shared KV store.
//!
//! Bytes live in ordinary files under a caller-chosen directory. This module
//! does not open block devices, format disks, or issue NVMe admin commands.
//! A lost or corrupt file is a miss. There is no erasure coding and no replica.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use gpucache_core_types::{
    verify_checksum, BlockState, ByteReader, ByteWriter, CacheError, QuotaKind, ResourceLimits,
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

/// Directory-backed store used by the V1 B TCP service.
///
/// Admission evicts the least-recently-used unpinned `COMMITTED` block when a
/// tenant or global retained-byte quota would otherwise fail. Pinned,
/// quarantined, and in-flight blocks are not evicted. `MemoryStore` itself
/// still refuses admission without evicting; only this backend reclaims.
pub struct FileStore {
    root: PathBuf,
    inner: MemoryStore,
    lru: VecDeque<u64>,
    skipped_corrupt: u32,
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
        self.ensure_batch(items.len())?;
        let mut out = Vec::with_capacity(items.len());
        let mut epoch = self.inner.epoch();
        for item in items {
            self.ensure_room(auth.tenant.as_bytes(), item.length, now_ms)?;
            let reply = self.inner.reserve_put(auth, now_ms, vec![item])?;
            epoch = reply.store_epoch;
            let Some(result) = reply.items.into_iter().next() else {
                return Err(CacheError::Malformed {
                    reason: "reserve batch",
                });
            };
            out.push(result);
        }
        Ok(BatchReply {
            store_epoch: epoch,
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
                    Err(err) => Err(err),
                },
                Err(err) => Err(err),
            };
            out.push(ItemResult { request_id, body });
        }
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

    fn ensure_batch(&self, len: usize) -> Result<(), CacheError> {
        if len == 0 {
            return Err(CacheError::Malformed {
                reason: "empty batch",
            });
        }
        if len > self.limits().max_batch_items as usize {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::BatchItems,
            });
        }
        Ok(())
    }

    fn ensure_room(&mut self, tenant: &[u8], extra: u64, now_ms: u64) -> Result<(), CacheError> {
        if extra > self.limits().max_retained_bytes {
            return Ok(());
        }
        loop {
            let global = self.inner.stats().retained_bytes;
            let tenant_used = self.inner.tenant_retained_bytes(tenant);
            let global_ok = global.saturating_add(extra) <= self.limits().max_retained_bytes;
            let tenant_ok =
                tenant_used.saturating_add(extra) <= self.limits().max_retained_bytes_per_tenant;
            if global_ok && tenant_ok {
                return Ok(());
            }
            let only_tenant = if tenant_ok { None } else { Some(tenant) };
            if !self.evict_oldest(now_ms, only_tenant)? {
                return Ok(());
            }
        }
    }

    fn evict_oldest(
        &mut self,
        now_ms: u64,
        only_tenant: Option<&[u8]>,
    ) -> Result<bool, CacheError> {
        let candidates = self.inner.eviction_candidates(now_ms);
        let n = self.lru.len();
        let mut deferred = Vec::new();
        let mut freed = false;
        for _ in 0..n {
            let Some(generation) = self.lru.pop_front() else {
                break;
            };
            let Some(candidate) = candidates.iter().find(|item| item.generation == generation)
            else {
                continue;
            };
            let wrong_tenant =
                only_tenant.is_some_and(|tenant| tenant != candidate.tenant.as_slice());
            if candidate.pinned || wrong_tenant {
                deferred.push(generation);
                continue;
            }
            let state = self.inner.force_evict_committed(generation, now_ms)?;
            self.delete_block(generation);
            if state != BlockState::Absent {
                self.lru.push_back(generation);
                continue;
            }
            freed = true;
            break;
        }
        for generation in deferred {
            self.lru.push_back(generation);
        }
        if freed {
            let _ = self.write_lru();
        }
        Ok(freed)
    }

    fn persist_committed(&mut self, generation: u64) -> Result<(), CacheError> {
        let block = self.inner.export_committed(generation)?;
        let bytes = encode_block(&block)?;
        let path = block_path(&self.root, generation);
        write_atomic(&path, &bytes)
    }

    fn touch(&mut self, generation: u64) {
        self.lru.retain(|existing| *existing != generation);
        self.lru.push_back(generation);
        let _ = self.write_lru();
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
        write_atomic(&self.root.join("MANIFEST"), &w.finish())
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
        write_atomic(&self.root.join("LRU"), &w.finish())
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
    let bytes = fs::read(&path).unwrap_or_default();
    let mut order = VecDeque::new();
    if let Some(gens) = decode_lru(&bytes) {
        for generation in gens {
            if loaded.contains(&generation) && !order.contains(&generation) {
                order.push_back(generation);
            }
        }
    }
    for generation in loaded {
        if !order.contains(generation) {
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

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CacheError> {
    let tmp = path.with_extension("tmp");
    {
        let mut file =
            File::create(&tmp).map_err(|_| CacheError::Storage { reason: "store io" })?;
        file.write_all(bytes)
            .map_err(|_| CacheError::Storage { reason: "store io" })?;
        file.sync_all()
            .map_err(|_| CacheError::Storage { reason: "store io" })?;
    }
    fs::rename(&tmp, path).map_err(|_| CacheError::Storage { reason: "store io" })?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}
