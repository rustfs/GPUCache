// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! Import one length-delimited KV block frame from stdin into a fresh MemoryStore.
//!
//! The frame is a CPU contract object. This tool does not talk to vLLM and does
//! not claim a GPU or DPU path.

use std::io::{self, Read};
use std::process::ExitCode;

use gpucache_protocol::decode_export;
use gpucache_storage::import_committed_block;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        eprintln!("usage: kv-block-xfer import <credential-hex>");
        return ExitCode::from(1);
    };
    if cmd != "import" {
        eprintln!("usage: kv-block-xfer import <credential-hex>");
        return ExitCode::from(1);
    }
    let Some(hex_arg) = args.next() else {
        eprintln!("error=missing credential fingerprint");
        return ExitCode::from(1);
    };
    let Ok(fingerprint) = parse_hex32(&hex_arg) else {
        eprintln!("error=credential fingerprint must be 32 bytes of hex");
        return ExitCode::from(1);
    };
    let mut buf = Vec::new();
    if io::stdin().read_to_end(&mut buf).is_err() {
        eprintln!("error=stdin");
        return ExitCode::from(1);
    }
    let frame = match decode_export(&buf) {
        Ok(frame) => frame,
        Err(err) => {
            eprintln!("error={err}");
            return ExitCode::from(2);
        }
    };
    match import_committed_block(&frame, fingerprint, 0) {
        Ok(report) => {
            println!("status=IMPORTED");
            println!("physical_key={}", report.physical_key_hex);
            println!("logical_key={}", report.logical_key_hex);
            println!("checksum={}", report.checksum_hex);
            println!("length={}", report.length);
            println!("source_generation={}", report.source_generation);
            println!("local_generation={}", report.local_generation);
            println!("state=COMMITTED");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error={err}");
            ExitCode::from(2)
        }
    }
}

fn parse_hex32(text: &str) -> Result<[u8; 32], ()> {
    if text.len() != 64 {
        return Err(());
    }
    let mut out = [0u8; 32];
    let bytes = text.as_bytes();
    for i in 0..32 {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_val(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}
