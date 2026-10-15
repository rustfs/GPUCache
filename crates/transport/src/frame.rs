// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::io::{IoSlice, Read, Write};
use std::net::TcpStream;

use gpucache_core_types::CacheError;

use crate::pool::Pooled;

pub struct Envelope {
    pub server_now: u64,
    body: Pooled,
}

impl Envelope {
    pub fn body(&self) -> &[u8] {
        self.body.as_slice()
    }
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
    let payload_len = body
        .len()
        .checked_add(8)
        .and_then(|len| u32::try_from(len).ok())
        .ok_or(CacheError::ObjectTooLarge {
            limit: max_frame as u64,
            actual: body.len() as u64,
        })?;
    let mut header = [0u8; 12];
    header[..4].copy_from_slice(&payload_len.to_le_bytes());
    header[4..].copy_from_slice(&server_now.to_le_bytes());
    write_parts(stream, &header, body).map_err(map_io)
}

// Keep the payload borrowed. A short vectored write can stop in either slice.
fn write_parts(writer: &mut impl Write, header: &[u8], body: &[u8]) -> std::io::Result<()> {
    let mut parts = [IoSlice::new(header), IoSlice::new(body)];
    let mut remaining = &mut parts[..];
    while !remaining.is_empty() {
        match writer.write_vectored(remaining) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut remaining, n),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
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
    let mut now = [0u8; 8];
    stream.read_exact(&mut now).map_err(map_io)?;
    buffer.buf_mut().resize(payload_len - 8, 0);
    stream.read_exact(buffer.buf_mut()).map_err(map_io)?;
    Ok(Envelope {
        server_now: u64::from_le_bytes(now),
        body: buffer,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::Pool;
    use std::net::TcpListener;
    use std::sync::Arc;

    struct ShortWriter {
        bytes: Vec<u8>,
        limit: usize,
        interrupted: bool,
    }
    impl Write for ShortWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let n = buf.len().min(self.limit);
            self.bytes.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_writes_and_interrupts_preserve_the_wire_bytes() {
        for limit in 1..32 {
            let mut writer = ShortWriter {
                bytes: Vec::new(),
                limit,
                interrupted: false,
            };
            write_parts(&mut writer, b"header", b"payload").unwrap();
            assert_eq!(writer.bytes, b"headerpayload");
        }
        let mut writer = ShortWriter {
            bytes: Vec::new(),
            limit: 0,
            interrupted: true,
        };
        assert_eq!(
            write_parts(&mut writer, b"header", b"payload")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WriteZero
        );
    }

    #[test]
    fn envelope_owns_the_pooled_buffer_and_preserves_wire_format() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut receiver, _) = listener.accept().unwrap();
        let pool = Arc::new(Pool::new(1));
        let mut buffer = pool.checkout().unwrap();
        buffer.buf_mut().resize(32, 0);
        let allocation = buffer.as_slice().as_ptr();
        write_envelope(&mut sender, 42, b"payload", 32).unwrap();
        let envelope = read_envelope(&mut receiver, buffer, 32).unwrap();
        assert_eq!(envelope.server_now, 42);
        assert_eq!(envelope.body(), b"payload");
        assert_eq!(envelope.body().as_ptr(), allocation);
        assert!(pool.checkout().is_err());
        drop(envelope);
        assert_eq!(pool.checkout().unwrap().as_slice().as_ptr(), allocation);
        write_envelope(&mut sender, 7, b"abc", 32).unwrap();
        let mut raw = [0u8; 15];
        receiver.read_exact(&mut raw).unwrap();
        assert_eq!(&raw[..4], &11u32.to_le_bytes());
        assert_eq!(&raw[4..12], &7u64.to_le_bytes());
        assert_eq!(&raw[12..], b"abc");
    }
}
