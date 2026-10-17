// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::cell::Cell;

use gpucache_cold::{
    pack_blocks, read_manifest, refill_degraded, refill_for_tenant, refill_generation,
    refill_matching, AsyncSink, ColdAvailability, ColdJob, Manifest, MockObjectStore, ObjectMeta,
    ObjectStore, Pack, PackedBlock, RefillBudget, RefillOutcome, MANIFEST_VERSION,
};
use gpucache_core_types::{
    content_checksum, synthetic_identity, AdapterIdentity, ByteWriter, CacheError, CacheIdentity,
    Digest, KvDtype, MultimodalIdentity, PositionEncoding, QuantFormat, TenantId, TrustDomain,
};

#[derive(Default)]
struct CountStore {
    inner: MockObjectStore,
    manifest_ranges: Cell<u32>,
    object_ranges: Cell<u32>,
    heads: Cell<u32>,
}

impl CountStore {
    fn publish(&mut self, pack: &Pack) {
        self.put(&pack.object_key, &pack.object_bytes).unwrap();
        self.put(&pack.manifest_key, &pack.manifest_bytes).unwrap();
    }
}

impl ObjectStore for CountStore {
    fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError> {
        self.inner.put(key, body)
    }

    fn head(&self, key: &str) -> Result<ObjectMeta, CacheError> {
        self.heads.set(self.heads.get() + 1);
        self.inner.head(key)
    }

    fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
        if key.ends_with("/manifest") {
            self.manifest_ranges.set(self.manifest_ranges.get() + 1);
        } else if key.ends_with("/object") {
            self.object_ranges.set(self.object_ranges.get() + 1);
        }
        self.inner.range_get(key, offset, length)
    }

    fn delete(&mut self, key: &str) -> Result<(), CacheError> {
        self.inner.delete(key)
    }
}

fn block(generation: u64, byte: u8, len: usize) -> PackedBlock {
    let payload = vec![byte; len];
    PackedBlock {
        generation,
        identity: synthetic_identity(byte, 1),
        checksum: content_checksum(&payload),
        payload,
    }
}

fn wide_budget() -> RefillBudget {
    RefillBudget::new(8 * 1024 * 1024, 16).unwrap()
}

#[test]
fn drain_then_refill_checks_the_budget_before_the_payload() {
    let blocks = [block(1, 1, 32), block(2, 2, 4096)];
    let pack = pack_blocks("refill", &blocks).unwrap();
    let mut store = CountStore::default();
    let mut sink = AsyncSink::new(2, 8 * 1024 * 1024).unwrap();
    sink.enqueue(ColdJob::from_pack(&pack)).unwrap();
    assert_eq!(sink.drain(&mut store, 1).unwrap(), 1);
    let tight = RefillBudget::new(pack.manifest_bytes.len() as u64, 16).unwrap();
    assert_eq!(
        refill_generation(&store, &pack.manifest_key, 2, &tight, &blocks[1].identity).unwrap(),
        RefillOutcome::OverBudget
    );
    assert_eq!(store.manifest_ranges.get(), 1);
    assert_eq!(store.object_ranges.get(), 0);
    assert_eq!(
        refill_generation(
            &store,
            &pack.manifest_key,
            2,
            &wide_budget(),
            &blocks[1].identity,
        )
        .unwrap(),
        RefillOutcome::Loaded {
            payload: blocks[1].payload.clone(),
        }
    );
    assert_eq!(store.object_ranges.get(), 1);
}

#[test]
fn huge_manifest_is_not_downloaded() {
    let blocks = [block(1, 5, 8), block(2, 6, 8)];
    let pack = pack_blocks("tiny", &blocks).unwrap();
    let mut store = CountStore::default();
    store.publish(&pack);
    assert_eq!(
        refill_generation(
            &store,
            &pack.manifest_key,
            1,
            &RefillBudget::new(1, 16).unwrap(),
            &blocks[0].identity,
        )
        .unwrap(),
        RefillOutcome::OverBudget
    );
    assert_eq!(store.manifest_ranges.get(), 0);
    assert_eq!(store.object_ranges.get(), 0);
}

#[test]
fn refill_charges_manifest_and_payload_to_one_budget() {
    let block = block(1, 1, 32);
    let pack = pack_blocks("total-budget", std::slice::from_ref(&block)).unwrap();
    let mut store = CountStore::default();
    store.publish(&pack);
    let total = pack.manifest_bytes.len() as u64 + 32;
    assert_eq!(
        refill_generation(
            &store,
            &pack.manifest_key,
            1,
            &RefillBudget::new(total - 1, 1).unwrap(),
            &block.identity,
        )
        .unwrap(),
        RefillOutcome::OverBudget
    );
    assert_eq!(store.object_ranges.get(), 0);
    assert!(matches!(
        refill_generation(
            &store,
            &pack.manifest_key,
            1,
            &RefillBudget::new(total, 1).unwrap(),
            &block.identity,
        )
        .unwrap(),
        RefillOutcome::Loaded { .. }
    ));
    assert_eq!(store.object_ranges.get(), 1);
}

#[test]
fn wrong_tenant_refill_does_not_download_the_payload() {
    let block = block(1, 1, 32);
    let pack = pack_blocks("tenant", std::slice::from_ref(&block)).unwrap();
    let mut store = CountStore::default();
    store.publish(&pack);
    let mut foreign = block.identity.clone();
    foreign.tenant = TenantId::new(b"other-tenant".to_vec()).unwrap();
    assert_eq!(
        refill_matching(&store, &pack.manifest_key, 1, &wide_budget(), &foreign),
        Err(CacheError::Unauthorized)
    );
    assert_eq!(store.object_ranges.get(), 0);
    assert_eq!(
        refill_matching(
            &store,
            &pack.manifest_key,
            1,
            &wide_budget(),
            &block.identity
        )
        .unwrap(),
        RefillOutcome::Loaded {
            payload: block.payload,
        }
    );
    assert_eq!(store.object_ranges.get(), 1);
}

#[test]
fn every_public_refill_entry_requires_and_checks_the_expected_identity() {
    let block = block(1, 1, 32);
    let pack = pack_blocks("entrypoints", std::slice::from_ref(&block)).unwrap();
    let mut store = CountStore::default();
    store.publish(&pack);
    let mut wrong = block.identity.clone();
    wrong.model_revision.0[0] ^= 1;
    let budget = wide_budget();
    for result in [
        refill_generation(&store, &pack.manifest_key, 1, &budget, &wrong),
        refill_matching(&store, &pack.manifest_key, 1, &budget, &wrong),
        refill_degraded(
            &store,
            &pack.manifest_key,
            1,
            &budget,
            ColdAvailability::Ready,
            &wrong,
        ),
        refill_for_tenant(
            &store,
            &pack.manifest_key,
            1,
            &budget,
            ColdAvailability::Ready,
            &wrong,
        ),
    ] {
        assert_eq!(result.unwrap(), RefillOutcome::Missing);
    }
    assert_eq!(store.object_ranges.get(), 0);
}

#[test]
fn complete_identity_mismatches_are_misses_before_payload_io() {
    let block = block(1, 1, 32);
    let pack = pack_blocks("identity", std::slice::from_ref(&block)).unwrap();
    let mut store = CountStore::default();
    store.publish(&pack);
    let base = block.identity;
    let mut variants: Vec<CacheIdentity> = Vec::new();
    let mut changed = base.clone();
    changed.trust_domain = TrustDomain::new(b"another-domain".to_vec()).unwrap();
    variants.push(changed);
    for field in 0..4 {
        let mut changed = base.clone();
        match field {
            0 => changed.model_revision.0[0] ^= 1,
            1 => changed.model_config_hash.0[0] ^= 1,
            2 => changed.tokenizer_revision.0[0] ^= 1,
            _ => changed.prefix_chain[0].0[0] ^= 1,
        }
        variants.push(changed);
    }
    let mut changed = base.clone();
    changed.adapter = AdapterIdentity::Lora {
        revision: Digest([9; 32]),
    };
    variants.push(changed);
    let mut changed = base.clone();
    changed.kv_dtype = KvDtype::F16;
    variants.push(changed);
    let mut changed = base.clone();
    changed.quant_format = QuantFormat::Fp8;
    variants.push(changed);
    let mut changed = base.clone();
    changed.layout.minor += 1;
    variants.push(changed);
    let mut changed = base.clone();
    changed.shard.tp_size = 2;
    changed.shard.tp_rank = 1;
    variants.push(changed);
    let mut changed = base.clone();
    changed.position = PositionEncoding::Absolute;
    variants.push(changed);
    let mut changed = base.clone();
    changed.rope.theta_bits = 500_000.0f64.to_bits();
    variants.push(changed);
    let mut changed = base.clone();
    changed.valid_length -= 1;
    variants.push(changed);
    let mut changed = base.clone();
    changed.block_token_count += 1;
    variants.push(changed);
    let mut changed = base.clone();
    changed.multimodal = Some(MultimodalIdentity {
        processor_revision: Digest([2; 32]),
        input_digest: Digest([3; 32]),
    });
    variants.push(changed);
    for expected in variants {
        assert_eq!(
            refill_matching(&store, &pack.manifest_key, 1, &wide_budget(), &expected).unwrap(),
            RefillOutcome::Missing
        );
    }
    assert_eq!(store.object_ranges.get(), 0);
}

fn encode_manifest(manifest: &Manifest, version: u16) -> Vec<u8> {
    let mut bytes = ByteWriter::new();
    bytes.raw(b"GPMF");
    bytes.u16(version);
    bytes.raw(&manifest.object_checksum.0);
    bytes.bytes(manifest.object_key.as_bytes()).unwrap();
    bytes.u32(manifest.ranges.len() as u32);
    for range in &manifest.ranges {
        bytes.u64(range.generation);
        bytes.u64(range.offset);
        bytes.u64(range.length);
        bytes.raw(&range.checksum.0);
        if version >= 2 {
            bytes.bytes(&range.tenant).unwrap();
        }
        if version >= 3 {
            bytes.raw(&range.identity_digest.0);
        }
    }
    bytes.finish()
}

#[test]
fn legacy_v1_and_v2_fixtures_are_clean_misses_without_payload_reads() {
    let block = block(1, 1, 32);
    let pack = pack_blocks("legacy", std::slice::from_ref(&block)).unwrap();
    let manifest = read_manifest(&pack.manifest_bytes).unwrap();
    for version in [1, 2] {
        let bytes = encode_manifest(&manifest, version);
        let mut store = CountStore::default();
        store.put("legacy/manifest", &bytes).unwrap();
        store.put(&pack.object_key, &pack.object_bytes).unwrap();
        assert_eq!(
            refill_generation(
                &store,
                "legacy/manifest",
                1,
                &wide_budget(),
                &block.identity
            )
            .unwrap(),
            RefillOutcome::Missing
        );
        assert_eq!(store.manifest_ranges.get(), 1);
        assert_eq!(store.object_ranges.get(), 0);
    }
}

fn manifest_key(namespace: &str, bytes: &[u8]) -> String {
    let digest: String = content_checksum(bytes)
        .0
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{namespace}/{digest}/manifest")
}

#[test]
fn malformed_v3_manifest_fails_closed_before_payload_io() {
    let block = block(1, 1, 32);
    let pack = pack_blocks("malformed", std::slice::from_ref(&block)).unwrap();
    let mut empty_tenant = read_manifest(&pack.manifest_bytes).unwrap();
    empty_tenant.ranges[0].tenant.clear();
    let mut duplicate = read_manifest(&pack.manifest_bytes).unwrap();
    duplicate.ranges.push(duplicate.ranges[0].clone());
    let mut overflow = read_manifest(&pack.manifest_bytes).unwrap();
    overflow.ranges[0].offset = u64::MAX;
    let mut trailing = pack.manifest_bytes.clone();
    trailing.push(0);
    let mut magic = pack.manifest_bytes.clone();
    magic[0] ^= 1;
    for bytes in [
        b"GPMF".to_vec(),
        magic,
        trailing,
        encode_manifest(&empty_tenant, MANIFEST_VERSION),
        encode_manifest(&duplicate, MANIFEST_VERSION),
        encode_manifest(&overflow, MANIFEST_VERSION),
    ] {
        let key = manifest_key("malformed", &bytes);
        let mut store = CountStore::default();
        store.put(&key, &bytes).unwrap();
        assert!(refill_matching(&store, &key, 1, &wide_budget(), &block.identity).is_err());
        assert_eq!(store.object_ranges.get(), 0);
    }
}

#[test]
fn corrupt_payload_and_content_key_substitution_return_no_loaded_bytes() {
    let block = block(1, 1, 32);
    let pack = pack_blocks("corruption", std::slice::from_ref(&block)).unwrap();
    let mut store = CountStore::default();
    store.publish(&pack);
    let mut corrupt = pack.object_bytes.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    store.put(&pack.object_key, &corrupt).unwrap();
    assert_eq!(
        refill_matching(
            &store,
            &pack.manifest_key,
            1,
            &wide_budget(),
            &block.identity
        ),
        Err(CacheError::ChecksumMismatch)
    );
    store
        .put("corruption/incorrect/manifest", &pack.manifest_bytes)
        .unwrap();
    assert!(refill_matching(
        &store,
        "corruption/incorrect/manifest",
        1,
        &wide_budget(),
        &block.identity,
    )
    .is_err());
    assert_eq!(store.object_ranges.get(), 1);
}

#[test]
fn invalid_expected_identity_fails_before_any_store_call() {
    let mut expected = synthetic_identity(1, 1);
    expected.valid_length = 0;
    let store = CountStore::default();
    assert!(refill_matching(&store, "ns/manifest", 1, &wide_budget(), &expected).is_err());
    assert_eq!(store.heads.get(), 0);
    assert_eq!(store.manifest_ranges.get(), 0);
    assert_eq!(store.object_ranges.get(), 0);
}

#[test]
fn missing_generation_and_range_limit_do_not_download_payloads() {
    let blocks = [block(1, 1, 32), block(2, 2, 32)];
    let pack = pack_blocks("missing", &blocks).unwrap();
    let mut store = CountStore::default();
    store.publish(&pack);
    assert_eq!(
        refill_matching(
            &store,
            &pack.manifest_key,
            9,
            &wide_budget(),
            &blocks[0].identity
        )
        .unwrap(),
        RefillOutcome::Missing
    );
    assert_eq!(
        refill_matching(
            &store,
            &pack.manifest_key,
            1,
            &RefillBudget::new(1_000_000, 1).unwrap(),
            &blocks[0].identity
        )
        .unwrap(),
        RefillOutcome::OverBudget
    );
    assert_eq!(store.object_ranges.get(), 0);
}
