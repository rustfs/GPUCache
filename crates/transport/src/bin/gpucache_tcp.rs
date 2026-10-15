// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0
//! File-backed GPUCache TCP service.
//!
//! Default bind is loopback. A fingerprint is required. The process creates
//! files only under `--data-dir` and does not format a device.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;

use gpucache_core_types::{ResourceLimits, TenantId, TrustDomain};
use gpucache_protocol::TenantPolicy;
use gpucache_storage::FileStore;
use gpucache_transport::{serve, ServerConfig};

fn main() -> ExitCode {
    match serve_from_args(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error={message}");
            eprintln!(
                "usage: gpucache-tcp serve --data-dir DIR --tenant ID --trust DOMAIN --fingerprint HEX64 [--bind 127.0.0.1:9735] [--allow-remote]"
            );
            ExitCode::from(1)
        }
    }
}

fn serve_from_args(args: impl Iterator<Item = String>) -> Result<(), &'static str> {
    let mut args = args.peekable();
    if args.next().as_deref() != Some("serve") {
        return Err("usage");
    }
    let mut bind: SocketAddr = "127.0.0.1:9735".parse().map_err(|_| "bind")?;
    let mut data_dir: Option<PathBuf> = None;
    let mut tenant: Option<String> = None;
    let mut trust: Option<String> = None;
    let mut fingerprint: Option<String> = None;
    let mut allow_remote = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                let value = args.next().ok_or("bind")?;
                bind = value.parse().map_err(|_| "bind")?;
            }
            "--data-dir" => data_dir = Some(PathBuf::from(args.next().ok_or("data dir")?)),
            "--tenant" => tenant = Some(args.next().ok_or("tenant")?),
            "--trust" => trust = Some(args.next().ok_or("trust")?),
            "--fingerprint" => fingerprint = Some(args.next().ok_or("fingerprint")?),
            "--allow-remote" => allow_remote = true,
            _ => return Err("usage"),
        }
    }
    let data_dir = data_dir.ok_or("data dir")?;
    let tenant = TenantId::new(tenant.ok_or("tenant")?.into_bytes()).map_err(|_| "tenant")?;
    let trust = TrustDomain::new(trust.ok_or("trust")?.into_bytes()).map_err(|_| "trust")?;
    let fingerprint = parse_hex32(&fingerprint.ok_or("fingerprint")?).map_err(|_| "fingerprint")?;
    let policy = TenantPolicy::new(tenant, trust, fingerprint).map_err(|_| "fingerprint")?;
    let store = FileStore::open(&data_dir, ResourceLimits::dev_defaults(), vec![policy])
        .map_err(|_| "data dir")?;
    let cfg = ServerConfig {
        bind,
        allow_remote,
        max_frame_bytes: 2 * 1024 * 1024,
        ..ServerConfig::default()
    };
    let handle = serve(store, cfg).map_err(|_| "bind")?;
    println!("listening={}", handle.addr());
    thread::park();
    Ok(())
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
