// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Protocol decoder fuzz. Random, truncated, duplicated, and reordered frames
//! must not panic and must not decode as a different successful request.

use gpucache_core_types::{
    content_checksum, synthetic_identity, CacheError, RequestId, HARD_BYTES_CAP,
};
use proptest::prelude::*;

use crate::{
    decode_export, decode_hello, decode_hello_ack, decode_reply, decode_request, encode_export,
    encode_hello, encode_request, AuthContext, CommittedExport, ItemHeader, LookupItem,
    RequestBody, WIRE_VERSION,
};

fn sample_request() -> (AuthContext, RequestBody) {
    let identity = synthetic_identity(1, 1);
    let auth = AuthContext::new(
        identity.tenant.clone(),
        identity.trust_domain.clone(),
        [9u8; 32],
    )
    .unwrap();
    let body = RequestBody::Lookup(vec![LookupItem {
        header: ItemHeader {
            request_id: RequestId::from_counter(1).unwrap(),
            epoch: 1,
            deadline_ms: 50,
        },
        identity,
    }]);
    (auth, body)
}

fn sample_export() -> Vec<u8> {
    let payload = b"synthetic-kv".to_vec();
    encode_export(&CommittedExport {
        identity: synthetic_identity(1, 1),
        source_generation: 1,
        checksum: content_checksum(&payload),
        payload,
    })
    .unwrap()
}

#[test]
fn truncated_duplicate_and_reordered_frames_fail_closed() {
    let (auth, body) = sample_request();
    let frame = encode_request(&auth, &body).unwrap();
    let (decoded_auth, decoded_body) = decode_request(&frame).unwrap();
    assert_eq!(decoded_auth, auth);
    assert_eq!(decoded_body, body);
    for len in 0..frame.len() {
        assert!(
            decode_request(&frame[..len]).is_err(),
            "short frame of {len} bytes must not decode"
        );
    }
    let mut duplicated = frame.clone();
    duplicated.extend_from_slice(&frame);
    assert!(matches!(
        decode_request(&duplicated),
        Err(CacheError::Malformed {
            reason: "trailing bytes"
        })
    ));
    let mut reordered = frame.clone();
    let mid = reordered.len() / 2;
    reordered.rotate_left(mid);
    assert!(decode_request(&reordered).is_err());
    let export = sample_export();
    let mut corrupt = export.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0x01;
    assert!(matches!(
        decode_export(&corrupt),
        Err(CacheError::ChecksumMismatch)
    ));
    assert!(decode_export(&export[..export.len() / 2]).is_err());
    let mut huge = Vec::from(&b"GCK1"[..]);
    huge.extend_from_slice(&WIRE_VERSION.to_le_bytes());
    huge.extend_from_slice(&1u16.to_le_bytes());
    huge.extend_from_slice(&(HARD_BYTES_CAP as u32).saturating_add(1).to_le_bytes());
    assert!(decode_request(&huge).is_err());
    assert!(decode_reply(&frame).is_err());
    assert!(decode_hello(&frame).is_err());
    assert!(decode_hello_ack(&frame).is_err());
    let hello = encode_hello(&[WIRE_VERSION]).unwrap();
    assert_eq!(decode_hello(&hello).unwrap(), vec![WIRE_VERSION]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn random_frames_do_not_panic_and_roundtrip_when_accepted(
        bytes in prop::collection::vec(any::<u8>(), 0..384)
    ) {
        if let Ok((auth, body)) = decode_request(&bytes) {
            let again = encode_request(&auth, &body).expect("accepted request encodes");
            let (auth2, body2) = decode_request(&again).expect("re-encoded request decodes");
            assert_eq!(auth, auth2);
            assert_eq!(body, body2);
        }
        let _ = decode_reply(&bytes);
        let _ = decode_export(&bytes);
        let _ = decode_hello(&bytes);
        let _ = decode_hello_ack(&bytes);
    }
}
