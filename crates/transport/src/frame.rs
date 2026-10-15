// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::io::{Read, Write};
use std::net::TcpStream;

use gpucache_core_types::CacheError;

use crate::pool::Pooled;

pub struct Envelope {
    pub server_now: u64,
    pub body: Vec<u8>,
}

pub fn write_envelope(
    stream: &mut TcpStream,
    server_now: u64,
    body: &[u8],
    max_frame: usize,
) -> Result<(), CacheError> {
    if body.len() > max_frame {
        return Err(CacheError::ObjectTooLarge {
            limit: max_frame as u64,
            actual: body.len() as u64,
        });
    }
    let payload_len = u32::try_from(8 + body.len()).map_err(|_| CacheError::ObjectTooLarge {
        limit: max_frame as u64,
        actual: body.len() as u64,
    })?;
    let mut buf = Vec::with_capacity(4 + payload_len as usize);
    buf.extend_from_slice(&payload_len.to_le_bytes());
    buf.extend_from_slice(&server_now.to_le_bytes());
    buf.extend_from_slice(body);
    stream.write_all(&buf).map_err(map_io)?;
    Ok(())
}

pub fn read_envelope(
    stream: &mut TcpStream,
    mut buffer: Pooled,
    max_frame: usize,
) -> Result<Envelope, CacheError> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(map_io)?;
    let payload_len = u32::from_le_bytes(len_buf) as usize;
    let max_payload = max_frame.saturating_add(8);
    if payload_len < 8 || payload_len > max_payload {
        return Err(CacheError::ObjectTooLarge {
            limit: max_frame as u64,
            actual: payload_len.saturating_sub(8) as u64,
        });
    }
    buffer.buf_mut().resize(payload_len, 0);
    stream.read_exact(buffer.buf_mut()).map_err(map_io)?;
    let server_now = u64::from_le_bytes(buffer.buf_mut()[..8].try_into().unwrap_or([0; 8]));
    let body = buffer.buf_mut()[8..].to_vec();
    Ok(Envelope { server_now, body })
}

pub fn map_io(err: std::io::Error) -> CacheError {
    if err.kind() == std::io::ErrorKind::TimedOut || err.kind() == std::io::ErrorKind::WouldBlock {
        CacheError::TimedOut
    } else {
        CacheError::Storage {
            reason: "transport io",
        }
    }
}
