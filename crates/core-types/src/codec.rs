// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::error::CacheError;

/// Absolute decoder cap. Store quotas are tighter; this only stops hostile lengths
/// from allocating before validation.
pub const HARD_BYTES_CAP: usize = 64 * 1024 * 1024;

pub struct ByteWriter {
    buf: Vec<u8>,
}

impl ByteWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Length-delimited bytes. The length is part of the encoding so two
    /// concatenations of different splits cannot collide.
    pub fn bytes(&mut self, bytes: &[u8]) -> Result<(), CacheError> {
        if bytes.len() > HARD_BYTES_CAP {
            return Err(CacheError::ObjectTooLarge {
                limit: HARD_BYTES_CAP as u64,
                actual: bytes.len() as u64,
            });
        }
        self.u32(
            u32::try_from(bytes.len()).map_err(|_| CacheError::Malformed {
                reason: "field length exceeds u32",
            })?,
        );
        self.raw(bytes);
        Ok(())
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl Default for ByteWriter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ByteReader<'a> {
    buf: &'a [u8],
    i: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, i: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.i)
    }

    pub fn finish(self) -> Result<(), CacheError> {
        if self.i != self.buf.len() {
            return Err(CacheError::Malformed {
                reason: "trailing bytes",
            });
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CacheError> {
        let end = self.i.checked_add(n).ok_or(CacheError::Malformed {
            reason: "length overflow",
        })?;
        if end > self.buf.len() {
            return Err(CacheError::Malformed {
                reason: "truncated buffer",
            });
        }
        let out = &self.buf[self.i..end];
        self.i = end;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, CacheError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, CacheError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32, CacheError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, CacheError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn bytes(&mut self, max: usize) -> Result<&'a [u8], CacheError> {
        let len = self.u32()? as usize;
        if len > max || len > HARD_BYTES_CAP {
            return Err(CacheError::ObjectTooLarge {
                limit: max.min(HARD_BYTES_CAP) as u64,
                actual: len as u64,
            });
        }
        self.take(len)
    }

    pub fn array32(&mut self) -> Result<[u8; 32], CacheError> {
        let b = self.take(32)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(b);
        Ok(out)
    }

    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], CacheError> {
        if n > HARD_BYTES_CAP {
            return Err(CacheError::ObjectTooLarge {
                limit: HARD_BYTES_CAP as u64,
                actual: n as u64,
            });
        }
        self.take(n)
    }
}
