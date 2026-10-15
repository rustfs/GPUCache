// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Deterministic synthetic KV payload used by CPU tests and the fallback demo.
//!
//! The payload is a versioned list of token ids. Comparison is exact equality
//! of those ids. This is not a vLLM tensor, and it does not define a numeric
//! tolerance for a GPU kernel. A real engine export stays `NOT_RUN_HARDWARE`.

use gpucache_core_types::{ByteReader, ByteWriter, CacheError};

const MAGIC: &[u8; 4] = b"SKV1";
const VERSION: u16 = 1;
const MAX_TOKENS: usize = 4096;

/// Encode token ids as a synthetic KV block. The bytes are the token ids.
pub fn encode_synthetic_kv(token_ids: &[u32]) -> Result<Vec<u8>, CacheError> {
    if token_ids.is_empty() || token_ids.len() > MAX_TOKENS {
        return Err(CacheError::Malformed {
            reason: "synthetic tokens",
        });
    }
    let mut w = ByteWriter::with_capacity(10 + token_ids.len() * 4);
    w.raw(MAGIC);
    w.u16(VERSION);
    w.u32(
        u32::try_from(token_ids.len()).map_err(|_| CacheError::Malformed {
            reason: "synthetic tokens",
        })?,
    );
    for id in token_ids {
        w.u32(*id);
    }
    Ok(w.finish())
}

/// Decode a synthetic block. Trailing bytes, a bad magic, and a short buffer fail.
pub fn decode_synthetic_kv(bytes: &[u8]) -> Result<Vec<u32>, CacheError> {
    let mut r = ByteReader::new(bytes);
    let magic = r.raw(4)?;
    if magic != MAGIC {
        return Err(CacheError::Malformed {
            reason: "synthetic kv",
        });
    }
    if r.u16()? != VERSION {
        return Err(CacheError::Malformed {
            reason: "synthetic kv",
        });
    }
    let n = usize::try_from(r.u32()?).map_err(|_| CacheError::Malformed {
        reason: "synthetic tokens",
    })?;
    if n == 0 || n > MAX_TOKENS {
        return Err(CacheError::Malformed {
            reason: "synthetic tokens",
        });
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(r.u32()?);
    }
    r.finish()?;
    Ok(out)
}

/// Local recompute used when the cache misses. It is a pure function of the
/// token ids the caller already has. It does not read another tenant's block.
pub fn deterministic_recompute(token_ids: &[u32]) -> Result<Vec<u8>, CacheError> {
    encode_synthetic_kv(token_ids)
}

#[cfg(test)]
mod tests {
    use super::{decode_synthetic_kv, deterministic_recompute, encode_synthetic_kv};
    use gpucache_core_types::CacheError;

    #[test]
    fn exact_token_roundtrip_and_rejected_mutations() {
        let tokens = vec![11, 22, 33, 44];
        let bytes = encode_synthetic_kv(&tokens).unwrap();
        assert_eq!(decode_synthetic_kv(&bytes).unwrap(), tokens);
        assert_eq!(
            decode_synthetic_kv(&deterministic_recompute(&tokens).unwrap()).unwrap(),
            tokens
        );
        assert!(encode_synthetic_kv(&[]).is_err());
        assert!(matches!(
            decode_synthetic_kv(&bytes[..bytes.len() - 1]),
            Err(CacheError::Malformed { .. })
        ));
        let mut flipped = bytes.clone();
        flipped[0] ^= 0x01;
        assert!(decode_synthetic_kv(&flipped).is_err());
        let mut trailed = bytes.clone();
        trailed.push(0);
        assert!(matches!(
            decode_synthetic_kv(&trailed),
            Err(CacheError::Malformed {
                reason: "trailing bytes"
            })
        ));
    }
}
