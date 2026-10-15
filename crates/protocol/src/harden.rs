// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! CPU hooks for tenant authorization, replay windows, integrity seals,
//! and secret rotation.
//!
//! This is not mTLS. The TCP listener still relies on a trusted fabric when
//! it leaves loopback. See docs/adr/0007-trusted-fabric.md. Seals are SHA-256
//! over a length-delimited encoding. They are not an HMAC and not a device MAC.
//! Audit events carry generation ids and kinds, never fingerprints or payloads.

use std::collections::VecDeque;

use gpucache_core_types::{
    content_checksum, CacheError, Checksum, TenantId, TrustDomain, HARD_BYTES_CAP,
};

use crate::auth::{fingerprints_equal, AuthContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditEvent {
    Rotate { generation: u64 },
    Retire { generation: u64 },
    Deny,
    Replay,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityLog {
    events: VecDeque<AuditEvent>,
    cap: usize,
}

impl SecurityLog {
    pub fn new(cap: usize) -> Result<Self, CacheError> {
        if cap == 0 || cap > 1024 {
            return Err(CacheError::Malformed { reason: "audit" });
        }
        Ok(Self {
            events: VecDeque::new(),
            cap,
        })
    }

    pub fn push(&mut self, event: AuditEvent) {
        if self.events.len() == self.cap {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    pub fn events(&self) -> impl Iterator<Item = &AuditEvent> {
        self.events.iter()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

struct AclEntry {
    tenant: TenantId,
    trust: TrustDomain,
    fingerprint: [u8; 32],
    write: bool,
}

pub struct Acl {
    entries: Vec<AclEntry>,
    max: usize,
}

impl Acl {
    pub fn new(max: usize) -> Result<Self, CacheError> {
        if max == 0 || max > 1024 {
            return Err(CacheError::Malformed { reason: "authz" });
        }
        Ok(Self {
            entries: Vec::new(),
            max,
        })
    }

    pub fn grant(&mut self, ctx: AuthContext, write: bool) -> Result<(), CacheError> {
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.tenant == ctx.tenant
                && entry.trust == ctx.trust_domain
                && fingerprints_equal(&entry.fingerprint, &ctx.credential_fingerprint)
        }) {
            entry.write = write;
            return Ok(());
        }
        if self.entries.len() >= self.max {
            return Err(CacheError::QuotaExceeded {
                kind: gpucache_core_types::QuotaKind::InFlightRequests,
            });
        }
        self.entries.push(AclEntry {
            tenant: ctx.tenant,
            trust: ctx.trust_domain,
            fingerprint: ctx.credential_fingerprint,
            write,
        });
        Ok(())
    }

    pub fn check(&self, ctx: &AuthContext, write: bool) -> Result<(), CacheError> {
        let mut allowed = false;
        for entry in &self.entries {
            let same = entry.tenant == ctx.tenant
                && entry.trust == ctx.trust_domain
                && fingerprints_equal(&entry.fingerprint, &ctx.credential_fingerprint);
            if same && (!write || entry.write) {
                allowed = true;
            }
        }
        if allowed {
            Ok(())
        } else {
            Err(CacheError::Unauthorized)
        }
    }
}

pub struct ReplayGuard {
    skew_ms: u64,
    capacity: usize,
    seen: VecDeque<([u8; 32], [u8; 16])>,
}

impl ReplayGuard {
    pub fn new(skew_ms: u64, capacity: usize) -> Result<Self, CacheError> {
        if capacity == 0 || capacity > 4096 {
            return Err(CacheError::Malformed { reason: "replay" });
        }
        Ok(Self {
            skew_ms,
            capacity,
            seen: VecDeque::new(),
        })
    }

    /// Accept `nonce` once inside the skew window.
    ///
    /// The window is bounded. After `capacity` newer nonces, the oldest nonce
    /// can be presented again. That is a window, not a permanent replay cache.
    pub fn observe(
        &mut self,
        tenant: &[u8],
        nonce: &[u8; 16],
        now_ms: u64,
        issued_ms: u64,
    ) -> Result<(), CacheError> {
        if tenant.is_empty() || *nonce == [0u8; 16] {
            return Err(CacheError::Malformed { reason: "replay" });
        }
        if issued_ms > now_ms || now_ms - issued_ms > self.skew_ms {
            return Err(CacheError::Unauthorized);
        }
        let tenant_key = content_checksum(tenant).0;
        if self
            .seen
            .iter()
            .any(|(seen_tenant, seen_nonce)| *seen_tenant == tenant_key && seen_nonce == nonce)
        {
            return Err(CacheError::IdempotencyConflict);
        }
        if self.seen.len() == self.capacity {
            self.seen.pop_front();
        }
        self.seen.push_back((tenant_key, *nonce));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegritySeal {
    pub key_id: u64,
    pub generation: u64,
    pub checksum: Checksum,
}

pub fn seal(key_id: u64, generation: u64, payload: &[u8]) -> Result<IntegritySeal, CacheError> {
    if payload.is_empty() || payload.len() > HARD_BYTES_CAP {
        return Err(CacheError::Malformed {
            reason: "integrity",
        });
    }
    Ok(IntegritySeal {
        key_id,
        generation,
        checksum: tag(key_id, generation, payload),
    })
}

pub fn open_seal(seal: &IntegritySeal, payload: &[u8]) -> Result<(), CacheError> {
    let actual = tag(seal.key_id, seal.generation, payload);
    if actual != seal.checksum {
        return Err(CacheError::ChecksumMismatch);
    }
    Ok(())
}

fn tag(key_id: u64, generation: u64, payload: &[u8]) -> Checksum {
    let mut buf = Vec::with_capacity(20 + payload.len());
    buf.extend_from_slice(&key_id.to_le_bytes());
    buf.extend_from_slice(&generation.to_le_bytes());
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    content_checksum(&buf)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GenState {
    Current,
    Overlap,
    Retired,
}

struct Gen {
    id: u64,
    fingerprint: [u8; 32],
    state: GenState,
}

pub struct SecretRing {
    items: Vec<Gen>,
    next_id: u64,
    log: SecurityLog,
}

impl SecretRing {
    pub fn open(fingerprint: [u8; 32], audit_cap: usize) -> Result<Self, CacheError> {
        if fingerprint == [0u8; 32] {
            return Err(CacheError::Unauthorized);
        }
        let mut ring = Self {
            items: Vec::new(),
            next_id: 1,
            log: SecurityLog::new(audit_cap)?,
        };
        let id = ring.push(fingerprint, GenState::Current);
        ring.log.push(AuditEvent::Rotate { generation: id });
        Ok(ring)
    }

    pub fn rotate(&mut self, fingerprint: [u8; 32]) -> Result<u64, CacheError> {
        if fingerprint == [0u8; 32] {
            return Err(CacheError::Unauthorized);
        }
        if self
            .items
            .iter()
            .any(|item| item.fingerprint == fingerprint && item.state != GenState::Retired)
        {
            return Err(CacheError::IdempotencyConflict);
        }
        for item in &mut self.items {
            if item.state == GenState::Current {
                item.state = GenState::Overlap;
            }
        }
        let id = self.push(fingerprint, GenState::Current);
        self.log.push(AuditEvent::Rotate { generation: id });
        Ok(id)
    }

    pub fn retire(&mut self, id: u64) -> Result<(), CacheError> {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return Err(CacheError::UnknownGeneration);
        };
        if item.state != GenState::Overlap {
            return Err(CacheError::FencingRejected { reason: "secret" });
        }
        item.state = GenState::Retired;
        self.log.push(AuditEvent::Retire { generation: id });
        Ok(())
    }

    pub fn allow_read(&self, fingerprint: &[u8; 32]) -> Result<u64, CacheError> {
        self.allow(fingerprint, false)
    }

    pub fn allow_write(&self, fingerprint: &[u8; 32]) -> Result<u64, CacheError> {
        self.allow(fingerprint, true)
    }

    pub fn log(&self) -> &SecurityLog {
        &self.log
    }

    fn allow(&self, fingerprint: &[u8; 32], write: bool) -> Result<u64, CacheError> {
        for item in &self.items {
            if !fingerprints_equal(&item.fingerprint, fingerprint) {
                continue;
            }
            let ok = match item.state {
                GenState::Current => true,
                GenState::Overlap => !write,
                GenState::Retired => false,
            };
            if ok {
                return Ok(item.id);
            }
        }
        Err(CacheError::Unauthorized)
    }

    fn push(&mut self, fingerprint: [u8; 32], state: GenState) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.items.push(Gen {
            id,
            fingerprint,
            state,
        });
        id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestStamp {
    pub write: bool,
    pub nonce: [u8; 16],
    pub now_ms: u64,
    pub issued_ms: u64,
}

pub fn authorize_request(
    acl: &Acl,
    replay: &mut ReplayGuard,
    log: &mut SecurityLog,
    ctx: &AuthContext,
    stamp: RequestStamp,
) -> Result<(), CacheError> {
    if let Err(err) = acl.check(ctx, stamp.write) {
        log.push(AuditEvent::Deny);
        return Err(err);
    }
    if let Err(err) = replay.observe(
        ctx.tenant.as_bytes(),
        &stamp.nonce,
        stamp.now_ms,
        stamp.issued_ms,
    ) {
        if err != (CacheError::Malformed { reason: "replay" }) {
            log.push(AuditEvent::Replay);
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpucache_core_types::TenantId;

    fn ctx(tenant: &[u8], fingerprint: u8) -> AuthContext {
        AuthContext::new(
            TenantId::new(tenant.to_vec()).unwrap(),
            TrustDomain::new(b"td-local".to_vec()).unwrap(),
            [fingerprint; 32],
        )
        .unwrap()
    }

    #[test]
    fn acl_fails_closed_and_read_does_not_grant_write() {
        let mut acl = Acl::new(4).unwrap();
        assert_eq!(
            acl.check(&ctx(b"tenant-a", 1), false).unwrap_err(),
            CacheError::Unauthorized
        );
        acl.grant(ctx(b"tenant-a", 1), false).unwrap();
        acl.check(&ctx(b"tenant-a", 1), false).unwrap();
        assert_eq!(
            acl.check(&ctx(b"tenant-a", 1), true).unwrap_err(),
            CacheError::Unauthorized
        );
        assert_eq!(
            acl.check(&ctx(b"tenant-b", 1), false).unwrap_err(),
            CacheError::Unauthorized
        );
        acl.grant(ctx(b"tenant-a", 1), true).unwrap();
        acl.check(&ctx(b"tenant-a", 1), true).unwrap();
    }

    #[test]
    fn replay_rejects_a_second_nonce_and_a_stale_stamp() {
        let mut guard = ReplayGuard::new(1000, 2).unwrap();
        let nonce = [2u8; 16];
        guard.observe(b"tenant-a", &nonce, 5_000, 4_500).unwrap();
        assert_eq!(
            guard
                .observe(b"tenant-a", &nonce, 5_100, 4_600)
                .unwrap_err(),
            CacheError::IdempotencyConflict
        );
        guard.observe(b"tenant-b", &nonce, 5_100, 4_600).unwrap();
        assert_eq!(
            guard
                .observe(b"tenant-a", &[3u8; 16], 9_000, 1_000)
                .unwrap_err(),
            CacheError::Unauthorized
        );
    }

    #[test]
    fn seal_detects_a_bit_flip_and_returns_no_match_on_success_path() {
        let payload = b"kv-bytes";
        let seal = seal(7, 3, payload).unwrap();
        open_seal(&seal, payload).unwrap();
        let mut bad = *payload;
        bad[0] ^= 1;
        assert_eq!(
            open_seal(&seal, &bad).unwrap_err(),
            CacheError::ChecksumMismatch
        );
    }

    #[test]
    fn rotation_keeps_overlap_reads_and_audits_without_the_fingerprint() {
        let mut ring = SecretRing::open([4u8; 32], 8).unwrap();
        let first = ring.allow_write(&[4u8; 32]).unwrap();
        let second = ring.rotate([5u8; 32]).unwrap();
        assert!(ring.allow_read(&[4u8; 32]).is_ok());
        assert_eq!(
            ring.allow_write(&[4u8; 32]).unwrap_err(),
            CacheError::Unauthorized
        );
        ring.allow_write(&[5u8; 32]).unwrap();
        ring.retire(first).unwrap();
        assert_eq!(
            ring.allow_read(&[4u8; 32]).unwrap_err(),
            CacheError::Unauthorized
        );
        assert_eq!(
            ring.retire(second).unwrap_err(),
            CacheError::FencingRejected { reason: "secret" }
        );
        let text = format!("{:?}", ring.log().events().collect::<Vec<_>>());
        assert!(text.contains("Rotate"));
        assert!(text.contains("Retire"));
        assert!(!text.contains("4, 4, 4"));
        assert!(!text.contains("5, 5, 5"));
    }

    #[test]
    fn authorize_request_audits_deny_and_replay() {
        let mut acl = Acl::new(4).unwrap();
        acl.grant(ctx(b"tenant-a", 9), true).unwrap();
        let mut replay = ReplayGuard::new(50, 4).unwrap();
        let mut log = SecurityLog::new(8).unwrap();
        let stamp = RequestStamp {
            write: true,
            nonce: [9u8; 16],
            now_ms: 100,
            issued_ms: 90,
        };
        authorize_request(&acl, &mut replay, &mut log, &ctx(b"tenant-a", 9), stamp).unwrap();
        authorize_request(&acl, &mut replay, &mut log, &ctx(b"tenant-a", 9), stamp).unwrap_err();
        let denied = RequestStamp {
            write: false,
            nonce: [1u8; 16],
            now_ms: 100,
            issued_ms: 90,
        };
        authorize_request(&acl, &mut replay, &mut log, &ctx(b"tenant-a", 8), denied).unwrap_err();
        let events: Vec<_> = log.events().copied().collect();
        assert_eq!(events, vec![AuditEvent::Replay, AuditEvent::Deny]);
    }
}
