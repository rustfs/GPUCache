// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::identity::{AdapterIdentity, Digest, KvDtype, QuantFormat};
use crate::key::{decode_physical_key, encode_logical_key, encode_physical_key};
use crate::lifecycle::{BlockState, Event, Lifecycle};
use crate::limits::IdentityLimits;
use crate::{synthetic_identity, TenantId, TrustDomain};
use proptest::prelude::*;

fn limits() -> IdentityLimits {
    IdentityLimits {
        max_field_bytes: 128,
        max_prefix_links: 32,
        max_block_tokens: 4096,
    }
}

fn arb_bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..=max)
}

fn arb_identity() -> impl Strategy<Value = crate::CacheIdentity> {
    (
        arb_bytes(16),
        arb_bytes(16),
        any::<u8>(),
        1u16..=4,
        1u16..=4,
        0u16..=3,
        any::<u8>(),
        any::<bool>(),
    )
        .prop_map(
            |(tenant, trust, nonce, family, tp, rank_off, link_n, lora)| {
                let mut id = synthetic_identity(nonce, family.max(1));
                id.tenant = TenantId::new(tenant).expect("tenant");
                id.trust_domain = TrustDomain::new(trust).expect("trust");
                let tp = tp.max(1);
                id.shard.tp_size = tp;
                id.shard.tp_rank = rank_off % tp;
                id.kv_dtype = match nonce % 3 {
                    0 => KvDtype::Bf16,
                    1 => KvDtype::F16,
                    _ => KvDtype::F32,
                };
                id.quant_format = if nonce % 2 == 0 {
                    QuantFormat::None
                } else {
                    QuantFormat::Fp8
                };
                let mut link = [0u8; 32];
                link[0] = link_n;
                id.prefix_chain = vec![Digest(link)];
                if lora {
                    id.adapter = AdapterIdentity::Lora {
                        revision: Digest([nonce; 32]),
                    };
                }
                id
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn physical_key_roundtrip_is_deterministic(id in arb_identity()) {
        let limits = limits();
        let once = encode_physical_key(&id, &limits).unwrap();
        let twice = encode_physical_key(&id, &limits).unwrap();
        prop_assert_eq!(once.clone(), twice);
        let back = decode_physical_key(&once).unwrap();
        prop_assert_eq!(back.clone(), id);
        let again = encode_physical_key(&back, &limits).unwrap();
        prop_assert_eq!(again, once);
    }

    #[test]
    fn layout_change_keeps_logical_key_and_changes_physical(id in arb_identity(), family in 5u16..=9) {
        let limits = limits();
        let mut other = id.clone();
        other.layout.family = family;
        let logical_a = encode_logical_key(&id, &limits).unwrap();
        let logical_b = encode_logical_key(&other, &limits).unwrap();
        let physical_a = encode_physical_key(&id, &limits).unwrap();
        let physical_b = encode_physical_key(&other, &limits).unwrap();
        prop_assert_eq!(logical_a, logical_b);
        prop_assert_ne!(physical_a, physical_b);
    }

    #[test]
    fn length_delimited_fields_do_not_alias(a in arb_bytes(12), b in arb_bytes(12)) {
        let mut left = synthetic_identity(1, 1);
        let mut right = synthetic_identity(1, 1);
        let mut tenant_l = a.clone();
        tenant_l.extend_from_slice(&b);
        // Split the same byte string at a different boundary.
        let split = (a.len() / 2).max(1).min(a.len());
        left.tenant = TenantId::new(a.clone()).unwrap();
        left.trust_domain = TrustDomain::new(b.clone()).unwrap();
        right.tenant = TenantId::new(a[..split].to_vec()).unwrap();
        right.trust_domain = TrustDomain::new({
            let mut t = a[split..].to_vec();
            t.extend_from_slice(&b);
            t
        }).unwrap();
        let limits = limits();
        if left.tenant.as_bytes() != right.tenant.as_bytes()
            || left.trust_domain.as_bytes() != right.trust_domain.as_bytes()
        {
            let lk = encode_physical_key(&left, &limits).unwrap();
            let rk = encode_physical_key(&right, &limits).unwrap();
            prop_assert_ne!(lk, rk);
        }
    }
}

#[test]
fn naive_concatenation_collision_is_rejected_by_lengths() {
    let limits = limits();
    let mut ab_c = synthetic_identity(3, 1);
    let mut a_bc = synthetic_identity(3, 1);
    ab_c.tenant = TenantId::new(b"ab".to_vec()).unwrap();
    ab_c.trust_domain = TrustDomain::new(b"c".to_vec()).unwrap();
    a_bc.tenant = TenantId::new(b"a".to_vec()).unwrap();
    a_bc.trust_domain = TrustDomain::new(b"bc".to_vec()).unwrap();
    let left = encode_physical_key(&ab_c, &limits).unwrap();
    let right = encode_physical_key(&a_bc, &limits).unwrap();
    assert_ne!(left, right);
    assert!(!left.windows(12).any(|w| w == b"hello prompt"));
}

#[test]
fn readable_flag_is_only_set_in_committed() {
    let mut life = Lifecycle::absent();
    let events = [
        Event::Reserve,
        Event::BeginWrite,
        Event::Commit,
        Event::ReaderPin,
        Event::BeginEvict,
    ];
    for ev in events {
        let before = life.state;
        life.apply(ev).unwrap();
        if life.is_hit_visible() {
            assert_eq!(life.state, BlockState::Committed);
        }
        let _ = before;
    }
    assert!(!life.is_hit_visible());
}

#[test]
fn random_illegal_events_do_not_invent_a_hit() {
    let mut lcg = 0x1234_5678_u64;
    let mut life = Lifecycle::absent();
    for _ in 0..200 {
        lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
        let ev = match lcg % 8 {
            0 => Event::Commit,
            1 => Event::BeginEvict,
            2 => Event::FinishEvict,
            3 => Event::ReclaimFenced,
            4 => Event::ChecksumRejected,
            5 => Event::CorruptHold,
            6 => Event::ReaderPin,
            _ => Event::BeginWrite,
        };
        let _ = life.apply(ev);
        if life.is_hit_visible() {
            assert_eq!(life.state, BlockState::Committed);
        } else {
            assert_ne!(life.state, BlockState::Committed);
        }
    }
}
