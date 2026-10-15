// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use sha2::{Digest, Sha256};

pub struct SignInput<'a> {
    pub method: &'a str,
    pub canonical_uri: &'a str,
    pub canonical_query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_hash_hex: &'a str,
    pub amz_date: &'a str,
    pub date_stamp: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    pub access_key: &'a str,
    pub secret: &'a str,
}

pub fn authorization(input: &SignInput<'_>) -> String {
    let canonical = canonical_request(input);
    let scope = format!(
        "{}/{}/{}/aws4_request",
        input.date_stamp, input.region, input.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        input.amz_date,
        scope,
        hex(&sha256(canonical.as_bytes()))
    );
    let key = signing_key(input.secret, input.date_stamp, input.region, input.service);
    let signature = hex(&hmac_sha256(&key, string_to_sign.as_bytes()));
    let signed = signed_headers(input.headers);
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        input.access_key, scope, signed, signature
    )
}

fn canonical_request(input: &SignInput<'_>) -> String {
    let mut headers = String::new();
    for (name, value) in input.headers {
        headers.push_str(name);
        headers.push(':');
        headers.push_str(value.trim());
        headers.push('\n');
    }
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        input.method,
        input.canonical_uri,
        input.canonical_query,
        headers,
        signed_headers(input.headers),
        input.payload_hash_hex
    )
}

fn signed_headers(headers: &[(&str, &str)]) -> String {
    headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";")
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let secret = format!("AWS4{secret}");
    let k_date = hmac_sha256(secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let dig = Sha256::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&dig);
    out
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let hashed = sha256(key);
        k[..32].copy_from_slice(&hashed);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; 64];
    let mut opad = [0u8; 64];
    for i in 0..64 {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }
    let mut inner = Vec::with_capacity(64 + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = sha256(&inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

pub fn format_amz_time(unix_secs: u64) -> (String, String) {
    let days = (unix_secs / 86_400) as i64;
    let tod = unix_secs % 86_400;
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    let (year, month, day) = civil_from_days(days);
    let date = format!("{year:04}{month:02}{day:02}");
    let stamp = format!("{date}T{hour:02}{minute:02}{second:02}Z");
    (date, stamp)
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (mp + if mp < 10 { 3 } else { 0 }) as u32 - if mp < 10 { 0 } else { 9 };
    let year = (if month <= 2 { y + 1 } else { y }) as i32;
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_sigv4_example_signature() {
        let headers = [
            (
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            ),
            ("host", "iam.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let header = authorization(&SignInput {
            method: "GET",
            canonical_uri: "/",
            canonical_query: "Action=ListUsers&Version=2010-05-08",
            headers: &headers,
            payload_hash_hex: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            amz_date: "20150830T123600Z",
            date_stamp: "20150830",
            region: "us-east-1",
            service: "iam",
            access_key: "AKIDEXAMPLE",
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        });
        assert!(header.ends_with(
            "Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        ));
    }

    #[test]
    fn unix_time_formats_the_sigv4_example_instant() {
        let (date, stamp) = format_amz_time(1_440_938_160);
        assert_eq!(date, "20150830");
        assert_eq!(stamp, "20150830T123600Z");
    }
}
