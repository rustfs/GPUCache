// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpucache_core_types::{content_checksum, CacheError, QuotaKind};

use crate::pack::{contract_smoke, ObjectMeta, ObjectStore};
use crate::sigv4::{authorization, format_amz_time, hex, sha256, SignInput};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeStatus {
    NotRunHardware { reason: &'static str },
    Passed,
    Failed { op: &'static str },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub status: ProbeStatus,
}

/// Live Put/Head/RangeGet/Delete when every required variable is set.
///
/// Missing configuration is `NOT_RUN_HARDWARE`. This function does not invent
/// credentials and does not fall back to a mock success.
pub fn live_probe() -> ProbeReport {
    let Some(cfg) = LiveConfig::from_env() else {
        return ProbeReport {
            status: ProbeStatus::NotRunHardware {
                reason: "no authorized RustFS endpoint",
            },
        };
    };
    if cfg.endpoint.starts_with("https://") {
        return ProbeReport {
            status: ProbeStatus::NotRunHardware {
                reason: "https client is not linked",
            },
        };
    }
    if !cfg.endpoint.starts_with("http://") {
        return ProbeReport {
            status: ProbeStatus::Failed { op: "endpoint" },
        };
    }
    let mut store = match HttpStore::new(&cfg) {
        Ok(store) => store,
        Err(_) => {
            return ProbeReport {
                status: ProbeStatus::Failed { op: "connect" },
            }
        }
    };
    match contract_smoke(&mut store) {
        Ok(()) => ProbeReport {
            status: ProbeStatus::Passed,
        },
        Err(_) => ProbeReport {
            status: ProbeStatus::Failed { op: "smoke" },
        },
    }
}

struct LiveConfig {
    endpoint: String,
    bucket: String,
    prefix: String,
    region: String,
    access_key: String,
    secret: String,
}

impl LiveConfig {
    fn from_env() -> Option<Self> {
        let endpoint = env_nonempty("GPUCACHE_RUSTFS_ENDPOINT")?;
        let bucket = env_nonempty("GPUCACHE_RUSTFS_BUCKET")?;
        let prefix = env_nonempty("GPUCACHE_RUSTFS_PREFIX")?;
        let access_key = env_nonempty("GPUCACHE_RUSTFS_ACCESS_KEY")?;
        let secret = env_nonempty("GPUCACHE_RUSTFS_SECRET_KEY")?;
        let region =
            env_nonempty("GPUCACHE_RUSTFS_REGION").unwrap_or_else(|| "us-east-1".to_string());
        if !key_ok(&bucket) || !key_ok(&prefix) || !key_ok(&region) {
            return None;
        }
        Some(Self {
            endpoint,
            bucket,
            prefix,
            region,
            access_key,
            secret,
        })
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

struct HttpStore {
    host: String,
    port: u16,
    bucket: String,
    region: String,
    access_key: String,
    secret: String,
    prefix: String,
}

impl HttpStore {
    fn new(cfg: &LiveConfig) -> Result<Self, CacheError> {
        let (host, port) = parse_http(&cfg.endpoint)?;
        let prefix = format!(
            "{}/probe-{}-{}",
            cfg.prefix.trim_matches('/'),
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        if !key_ok(&prefix) {
            return Err(CacheError::Malformed { reason: "cold key" });
        }
        Ok(Self {
            host,
            port,
            bucket: cfg.bucket.clone(),
            region: cfg.region.clone(),
            access_key: cfg.access_key.clone(),
            secret: cfg.secret.clone(),
            prefix,
        })
    }

    fn full_key(&self, key: &str) -> Result<String, CacheError> {
        let full = format!("{}/{}", self.prefix, key);
        if !key_ok(&full) {
            return Err(CacheError::Malformed { reason: "cold key" });
        }
        Ok(full)
    }
}

impl ObjectStore for HttpStore {
    fn put(&mut self, key: &str, body: &[u8]) -> Result<(), CacheError> {
        let key = self.full_key(key)?;
        let status = request(self, "PUT", &key, body, None, false)?;
        check_status(status.code)?;
        Ok(())
    }

    fn head(&self, key: &str) -> Result<ObjectMeta, CacheError> {
        let key = self.full_key(key)?;
        let status = request(self, "HEAD", &key, b"", None, false)?;
        if status.code == 404 {
            return Err(CacheError::Storage {
                reason: "object missing",
            });
        }
        check_status(status.code)?;
        let fetched = request(self, "GET", &key, b"", None, true)?;
        check_status(fetched.code)?;
        if let Some(len) = status.content_length {
            if len != fetched.body.len() as u64 {
                return Err(CacheError::LengthMismatch {
                    expected: len,
                    actual: fetched.body.len() as u64,
                });
            }
        }
        Ok(ObjectMeta {
            len: fetched.body.len() as u64,
            checksum: content_checksum(&fetched.body),
        })
    }

    fn range_get(&self, key: &str, offset: u64, length: u64) -> Result<Vec<u8>, CacheError> {
        let key = self.full_key(key)?;
        let end = offset.saturating_add(length).saturating_sub(1);
        let header = format!("bytes={offset}-{end}");
        let status = request(self, "GET", &key, b"", Some(&header), true)?;
        check_status(status.code)?;
        if status.body.len() as u64 != length {
            return Err(CacheError::LengthMismatch {
                expected: length,
                actual: status.body.len() as u64,
            });
        }
        Ok(status.body)
    }

    fn delete(&mut self, key: &str) -> Result<(), CacheError> {
        let key = self.full_key(key)?;
        let status = request(self, "DELETE", &key, b"", None, false)?;
        if status.code == 404 || (200..300).contains(&status.code) {
            return Ok(());
        }
        check_status(status.code)
    }
}

/// Preserve permanent authentication/request failures for sink classification.
fn check_status(code: u16) -> Result<(), CacheError> {
    match code {
        200..=299 => Ok(()),
        401 | 403 => Err(CacheError::Unauthorized),
        404 => Err(CacheError::Storage {
            reason: "object missing",
        }),
        408 => Err(CacheError::TimedOut),
        429 => Err(CacheError::QuotaExceeded {
            kind: QuotaKind::RateLimit,
        }),
        500..=599 => Err(CacheError::Storage {
            reason: "object unavailable",
        }),
        _ => Err(CacheError::Malformed {
            reason: "object response",
        }),
    }
}

struct HttpStatus {
    code: u16,
    content_length: Option<u64>,
    body: Vec<u8>,
}

fn request(
    store: &HttpStore,
    method: &str,
    key: &str,
    body: &[u8],
    range: Option<&str>,
    read_body: bool,
) -> Result<HttpStatus, CacheError> {
    let uri = format!("/{}/{}", store.bucket, key);
    let payload_hash = hex(&sha256(body));
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CacheError::Storage {
            reason: "object store",
        })?
        .as_secs();
    let (date_stamp, amz_date) = format_amz_time(unix);
    let host_header = if store.port == 80 {
        store.host.clone()
    } else {
        format!("{}:{}", store.host, store.port)
    };
    let headers = [
        ("host", host_header.as_str()),
        ("x-amz-content-sha256", payload_hash.as_str()),
        ("x-amz-date", amz_date.as_str()),
    ];
    let auth = authorization(&SignInput {
        method,
        canonical_uri: &uri,
        canonical_query: "",
        headers: &headers,
        payload_hash_hex: &payload_hash,
        amz_date: &amz_date,
        date_stamp: &date_stamp,
        region: &store.region,
        service: "s3",
        access_key: &store.access_key,
        secret: &store.secret,
    });
    let mut text = format!(
        "{method} {uri} HTTP/1.1\r\nHost: {host_header}\r\nx-amz-content-sha256: {payload_hash}\r\nx-amz-date: {amz_date}\r\nAuthorization: {auth}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(range) = range {
        text.push_str("Range: ");
        text.push_str(range);
        text.push_str("\r\n");
    }
    text.push_str("\r\n");
    let mut stream =
        TcpStream::connect((store.host.as_str(), store.port)).map_err(|_| CacheError::Storage {
            reason: "object store",
        })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| CacheError::Storage {
            reason: "object store",
        })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| CacheError::Storage {
            reason: "object store",
        })?;
    stream
        .write_all(text.as_bytes())
        .map_err(|_| CacheError::Storage {
            reason: "object store",
        })?;
    if !body.is_empty() {
        stream.write_all(body).map_err(|_| CacheError::Storage {
            reason: "object store",
        })?;
    }
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if raw.len().saturating_add(n) > 8 * 1024 * 1024 {
                    return Err(CacheError::ObjectTooLarge {
                        limit: 8 * 1024 * 1024,
                        actual: raw.len() as u64,
                    });
                }
                raw.extend_from_slice(&chunk[..n]);
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(CacheError::TimedOut);
            }
            Err(_) => {
                return Err(CacheError::Storage {
                    reason: "object store",
                })
            }
        }
    }
    parse_response(&raw, read_body)
}

fn parse_response(raw: &[u8], read_body: bool) -> Result<HttpStatus, CacheError> {
    let malformed = || CacheError::Malformed {
        reason: "object response",
    };
    // KV packs contain arbitrary bytes; only the HTTP header is UTF-8 text.
    let boundary = raw
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or_else(malformed)?;
    let text = std::str::from_utf8(&raw[..boundary]).map_err(|_| malformed())?;
    let mut lines = text.split("\r\n");
    let status = lines.next().ok_or_else(malformed)?;
    let mut fields = status.split_whitespace();
    if !matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(malformed());
    }
    let code = fields
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|code| (100..=599).contains(code))
        .ok_or_else(malformed)?;
    let mut content_length = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(malformed)?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            // The dependency-free smoke client does not decode chunked bodies.
            return Err(malformed());
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(malformed());
            }
            content_length = Some(value.trim().parse::<u64>().map_err(|_| malformed())?);
        }
    }
    let received = &raw[boundary + 4..];
    let body = if read_body && (200..300).contains(&code) {
        // An incomplete response, including a read timeout after headers, must
        // never yield a truncated success or consume bytes past the declared body.
        let expected = content_length.ok_or_else(malformed)?;
        if received.len() as u64 != expected {
            return Err(CacheError::LengthMismatch {
                expected,
                actual: received.len() as u64,
            });
        }
        received.to_vec()
    } else {
        Vec::new()
    };
    Ok(HttpStatus {
        code,
        content_length,
        body,
    })
}

fn parse_http(endpoint: &str) -> Result<(String, u16), CacheError> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or(CacheError::Malformed { reason: "cold key" })?;
    if rest.contains('/') || rest.is_empty() {
        return Err(CacheError::Malformed { reason: "cold key" });
    }
    if let Some((host, port)) = rest.rsplit_once(':') {
        let port = port
            .parse::<u16>()
            .map_err(|_| CacheError::Malformed { reason: "cold key" })?;
        if host.is_empty() {
            return Err(CacheError::Malformed { reason: "cold key" });
        }
        Ok((host.to_string(), port))
    } else {
        Ok((rest.to_string(), 80))
    }
}

fn key_ok(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 512
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b'/')
        && !key.contains("//")
        && !key.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_endpoint_is_not_run_hardware() {
        if std::env::var_os("GPUCACHE_RUSTFS_ENDPOINT").is_some() {
            return;
        }
        let report = live_probe();
        assert_eq!(
            report.status,
            ProbeStatus::NotRunHardware {
                reason: "no authorized RustFS endpoint",
            }
        );
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    #[test]
    fn http_status_preserves_permanent_and_transient_backend_failures() {
        for code in [401, 403] {
            assert_eq!(check_status(code), Err(CacheError::Unauthorized));
        }
        for code in [400, 301, 409, 413, 416] {
            let error = check_status(code).unwrap_err();
            assert!(!crate::failure::transient(&error));
        }
        for code in [408, 429, 500, 502, 503, 504] {
            let error = check_status(code).unwrap_err();
            assert!(crate::failure::transient(&error));
            assert!(crate::failure::refill_miss(&error));
        }
        assert!(crate::failure::refill_miss(&check_status(404).unwrap_err()));
        assert_eq!(check_status(204), Ok(()));
        // Error status classification does not require a success payload body.
        let status = parse_response(b"HTTP/1.1 503 Unavailable\r\n\r\n", true).unwrap();
        assert_eq!(
            check_status(status.code),
            Err(CacheError::Storage {
                reason: "object unavailable"
            })
        );
    }

    #[test]
    fn response_parser_preserves_binary_bodies_and_requires_complete_framing() {
        let mut response = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\r\n".to_vec();
        response.extend_from_slice(&[0, 255, 128, 42]);
        let parsed = parse_response(&response, true).unwrap();
        assert_eq!(parsed.code, 206);
        assert_eq!(parsed.body, [0, 255, 128, 42]);
        for malformed in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 1".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nxx",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n\r\nx",
            b"HTTP/1.1 200 OK\r\nContent-Length: garbage\r\n\r\nx",
        ] {
            assert!(parse_response(malformed, true).is_err());
        }
        // HEAD describes object length but contains no body.
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 900\r\n\r\n", false).is_ok());
    }
}
