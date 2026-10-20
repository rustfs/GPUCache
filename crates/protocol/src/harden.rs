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
    content_checksum, ByteWriter, CacheError, Checksum, QuotaKind, TenantId, TrustDomain,
    HARD_BYTES_CAP,
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

#[derive(Clone)]
struct AclEntry {
    tenant: TenantId,
    trust: TrustDomain,
    fingerprint: [u8; 32],
    write: bool,
}

#[derive(Clone)]
pub struct Acl {
    entries: Vec<AclEntry>,
    max: usize,
}

impl std::fmt::Debug for Acl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acl")
            .field("entries", &self.entries.len())
            .field("capacity", &self.max)
            .finish()
    }
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
        ctx.validate()?;
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
        ctx.validate()?;
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
    seen: VecDeque<ReplayEntry>,
    last_now_ms: Option<u64>,
}

struct ReplayEntry {
    scope: [u8; 32],
    nonce: [u8; 16],
    observed_ms: u64,
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
            last_now_ms: None,
        })
    }

    /// Accept `nonce` once inside the skew window.
    ///
    /// Entries are retained for the whole window, even when the guard is full.
    /// Capacity exhaustion rejects new admissions instead of enabling replay.
    /// `now_ms` must come from the server clock, never an untrusted request.
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
        self.observe_scope(tenant_key, nonce, now_ms, issued_ms)
    }

    fn observe_scope(
        &mut self,
        scope: [u8; 32],
        nonce: &[u8; 16],
        now_ms: u64,
        issued_ms: u64,
    ) -> Result<(), CacheError> {
        if *nonce == [0; 16] {
            return Err(CacheError::Malformed { reason: "replay" });
        }
        if self.last_now_ms.is_some_and(|last| now_ms < last)
            || issued_ms > now_ms
            || now_ms - issued_ms > self.skew_ms
        {
            return Err(CacheError::Unauthorized);
        }
        self.last_now_ms = Some(now_ms);
        // Subtraction cannot overflow: the clock is nondecreasing and every
        // retained observation came from an earlier accepted server time.
        self.seen
            .retain(|entry| now_ms - entry.observed_ms <= self.skew_ms);
        if self
            .seen
            .iter()
            .any(|entry| entry.scope == scope && entry.nonce == *nonce)
        {
            return Err(CacheError::IdempotencyConflict);
        }
        if self.seen.len() == self.capacity {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::IdempotencyTable,
            });
        }
        self.seen.push_back(ReplayEntry {
            scope,
            nonce: *nonce,
            observed_ms: now_ms,
        });
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
    if payload.is_empty() || payload.len() > HARD_BYTES_CAP {
        return Err(CacheError::Malformed {
            reason: "integrity",
        });
    }
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

const MAX_SECRET_GENERATIONS: usize = 1024;

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
        let id = ring.push(fingerprint, GenState::Current)?;
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
            .any(|item| fingerprints_equal(&item.fingerprint, &fingerprint))
        {
            return Err(CacheError::IdempotencyConflict);
        }
        // Preflight before demoting the current credential: a rejected rotation
        // must leave the previous write authority and audit history unchanged.
        self.check_capacity()?;
        for item in &mut self.items {
            if item.state == GenState::Current {
                item.state = GenState::Overlap;
            }
        }
        let id = self.push(fingerprint, GenState::Current)?;
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

    fn check_capacity(&self) -> Result<(), CacheError> {
        if self.items.len() >= MAX_SECRET_GENERATIONS {
            return Err(CacheError::QuotaExceeded {
                kind: QuotaKind::IdempotencyTable,
            });
        }
        self.next_id
            .checked_add(1)
            .ok_or(CacheError::FencingRejected {
                reason: "secret generation exhausted",
            })?;
        Ok(())
    }

    fn push(&mut self, fingerprint: [u8; 32], state: GenState) -> Result<u64, CacheError> {
        self.check_capacity()?;
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Gen {
            id,
            fingerprint,
            state,
        });
        Ok(id)
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
    // Scope replay by the complete authenticated principal, with unambiguous
    // lengths. The same tenant label in another trust domain is independent.
    let mut scope = ByteWriter::new();
    scope.raw(b"gpucache-replay-v1");
    scope.bytes(ctx.tenant.as_bytes())?;
    scope.bytes(ctx.trust_domain.as_bytes())?;
    scope.raw(&ctx.credential_fingerprint);
    if let Err(err) = replay.observe_scope(
        content_checksum(&scope.finish()).0,
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

    #[test]
    fn cloned_acl_keeps_authority_without_exposing_fingerprints_in_debug() {
        let ctx = AuthContext::new(
            TenantId::new(b"tenant".to_vec()).unwrap(),
            TrustDomain::new(b"trust".to_vec()).unwrap(),
            [173; 32],
        )
        .unwrap();
        let mut acl = Acl::new(1).unwrap();
        acl.grant(ctx.clone(), false).unwrap();
        let copy = acl.clone();
        assert_eq!(copy.check(&ctx, false), Ok(()));
        assert_eq!(copy.check(&ctx, true), Err(CacheError::Unauthorized));
        let debug = format!("{copy:?}");
        assert!(debug.contains("entries: 1"));
        assert!(!debug.contains("173"));
        assert!(!debug.contains("tenant"));
    }

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

    #[test]
    fn full_replay_window_never_evicts_a_live_nonce() {
        let mut guard = ReplayGuard::new(100, 1).unwrap();
        guard.observe(b"tenant-a", &[1; 16], 10, 10).unwrap();
        assert_eq!(
            guard.observe(b"tenant-a", &[2; 16], 11, 11),
            Err(CacheError::QuotaExceeded {
                kind: QuotaKind::IdempotencyTable,
            })
        );
        assert_eq!(
            guard.observe(b"tenant-a", &[1; 16], 110, 10),
            Err(CacheError::IdempotencyConflict)
        );
        guard.observe(b"tenant-a", &[2; 16], 111, 111).unwrap();
        assert_eq!(guard.seen.len(), 1);
        assert_eq!(
            guard.observe(b"tenant-a", &[1; 16], 111, 10),
            Err(CacheError::Unauthorized)
        );
    }

    #[test]
    fn replay_window_rejects_clock_rollback_without_losing_entries() {
        let mut guard = ReplayGuard::new(10, 2).unwrap();
        guard.observe(b"tenant-a", &[1; 16], 100, 100).unwrap();
        assert_eq!(
            guard.observe(b"tenant-a", &[2; 16], 99, 99),
            Err(CacheError::Unauthorized)
        );
        assert_eq!(
            guard.observe(b"tenant-a", &[1; 16], 100, 100),
            Err(CacheError::IdempotencyConflict)
        );
    }

    #[test]
    fn replay_window_does_not_overflow_near_the_clock_limit() {
        let mut guard = ReplayGuard::new(10, 1).unwrap();
        guard
            .observe(b"tenant-a", &[1; 16], u64::MAX - 1, u64::MAX - 1)
            .unwrap();
        assert_eq!(
            guard.observe(b"tenant-a", &[1; 16], u64::MAX, u64::MAX - 1),
            Err(CacheError::IdempotencyConflict)
        );
    }

    #[test]
    fn authenticated_replay_scopes_include_trust_and_credential() {
        let a = ctx(b"tenant-a", 1);
        let mut b = a.clone();
        b.trust_domain = TrustDomain::new(b"other-trust".to_vec()).unwrap();
        let c = ctx(b"tenant-a", 2);
        let mut acl = Acl::new(3).unwrap();
        for principal in [&a, &b, &c] {
            acl.grant(principal.clone(), true).unwrap();
        }
        let mut replay = ReplayGuard::new(20, 3).unwrap();
        let mut log = SecurityLog::new(8).unwrap();
        let stamp = RequestStamp {
            write: true,
            nonce: [1; 16],
            now_ms: 10,
            issued_ms: 10,
        };
        for principal in [&a, &b, &c] {
            authorize_request(&acl, &mut replay, &mut log, principal, stamp).unwrap();
        }
        assert_eq!(
            authorize_request(&acl, &mut replay, &mut log, &a, stamp),
            Err(CacheError::IdempotencyConflict)
        );
    }

    #[test]
    fn zero_credentials_cannot_be_granted_via_public_fields() {
        let mut zero = ctx(b"tenant-a", 1);
        zero.credential_fingerprint = [0; 32];
        let mut acl = Acl::new(1).unwrap();
        assert_eq!(acl.grant(zero.clone(), true), Err(CacheError::Unauthorized));
        assert_eq!(acl.check(&zero, false), Err(CacheError::Unauthorized));
    }

    #[test]
    fn failed_rotation_preserves_current_write_authority() {
        let mut ring = SecretRing::open([1; 32], 8).unwrap();
        ring.next_id = u64::MAX;
        let before_log = ring.log.len();
        assert_eq!(
            ring.rotate([2; 32]),
            Err(CacheError::FencingRejected {
                reason: "secret generation exhausted",
            })
        );
        assert_eq!(ring.allow_write(&[1; 32]), Ok(1));
        assert_eq!(ring.allow_read(&[2; 32]), Err(CacheError::Unauthorized));
        assert_eq!(ring.log.len(), before_log);
    }

    #[test]
    fn retired_credentials_cannot_be_resurrected_by_rotation() {
        let mut ring = SecretRing::open([1; 32], 8).unwrap();
        ring.rotate([2; 32]).unwrap();
        ring.retire(1).unwrap();
        assert_eq!(ring.rotate([1; 32]), Err(CacheError::IdempotencyConflict));
        assert_eq!(ring.allow_read(&[1; 32]), Err(CacheError::Unauthorized));
        assert_eq!(ring.allow_write(&[2; 32]), Ok(2));
    }

    #[test]
    fn secret_history_is_bounded_and_full_rotation_is_atomic() {
        let mut ring = SecretRing::open([1; 32], 8).unwrap();
        ring.items.resize_with(MAX_SECRET_GENERATIONS, || Gen {
            id: 0,
            fingerprint: [3; 32],
            state: GenState::Retired,
        });
        assert_eq!(
            ring.rotate([2; 32]),
            Err(CacheError::QuotaExceeded {
                kind: QuotaKind::IdempotencyTable,
            })
        );
        assert_eq!(ring.allow_write(&[1; 32]), Ok(1));
        assert_eq!(ring.items.len(), MAX_SECRET_GENERATIONS);
    }

    #[test]
    fn seal_verification_rejects_empty_payloads_even_with_matching_tags() {
        let forged = IntegritySeal {
            key_id: 1,
            generation: 1,
            checksum: tag(1, 1, &[]),
        };
        assert_eq!(
            open_seal(&forged, &[]),
            Err(CacheError::Malformed {
                reason: "integrity",
            })
        );
    }
}
