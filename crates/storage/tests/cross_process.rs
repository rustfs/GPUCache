// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::io::Write;
use std::process::{Command, Stdio};

use gpucache_core_types::{
    content_checksum, encode_physical_key, synthetic_identity, IdentityLimits,
};
use gpucache_protocol::{encode_export, CommittedExport};

const FP_HEX: &str = "0909090909090909090909090909090909090909090909090909090909090909";

#[test]
fn cross_process_export_import_roundtrip() {
    let payload = b"cross-process-synthetic-kv".to_vec();
    let identity = synthetic_identity(1, 1);
    let frame = encode_export(&CommittedExport {
        identity: identity.clone(),
        source_generation: 11,
        checksum: content_checksum(&payload),
        payload: payload.clone(),
    })
    .unwrap();
    let physical = encode_physical_key(
        &identity,
        &IdentityLimits {
            max_field_bytes: 128,
            max_prefix_links: 1024,
            max_block_tokens: 1_048_576,
        },
    )
    .unwrap();
    let output = run_import(&frame);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("status=IMPORTED"));
    assert!(stdout.contains("state=COMMITTED"));
    assert!(stdout.contains(&format!("length={}", payload.len())));
    assert!(stdout.contains(&format!("checksum={}", hex(&content_checksum(&payload).0))));
    assert!(stdout.contains(&format!("physical_key={}", hex(&physical))));
    assert!(stdout.contains("source_generation=11"));
}

#[test]
fn cross_process_corrupt_frame_is_rejected() {
    let payload = b"cross-process-synthetic-kv".to_vec();
    let mut frame = encode_export(&CommittedExport {
        identity: synthetic_identity(1, 1),
        source_generation: 11,
        checksum: content_checksum(&payload),
        payload,
    })
    .unwrap();
    let last = frame.len() - 1;
    frame[last] ^= 0x5a;
    let output = run_import(&frame);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("checksum"), "stderr={stderr}");
}

fn xfer_bin() -> String {
    std::env::var("CARGO_BIN_EXE_kv_block_xfer").unwrap_or_else(|_| {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop();
        path.pop();
        path.push("target");
        path.push("debug");
        path.push("kv-block-xfer");
        path.to_string_lossy().into_owned()
    })
}

fn run_import(frame: &[u8]) -> std::process::Output {
    let mut child = Command::new(xfer_bin())
        .arg("import")
        .arg(FP_HEX)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kv-block-xfer");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(frame)
        .expect("write frame");
    child.wait_with_output().expect("wait")
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}
