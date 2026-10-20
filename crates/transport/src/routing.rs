// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Bounded CPU/TCP shard routing. A session keeps the original six RPC methods.
//! Generations remain shard-local: retain the session's shard alongside a generation.
//! No directory, payload, or retry queue is shared across tenants.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use gpucache_core_types::{
    content_checksum, encode_physical_key, CacheError, CacheIdentity, IdentityLimits, QuotaKind,
    ResourceLimits,
};
use gpucache_protocol::{
    encode_request, AuthContext, BatchReply, CommitItem, CommitOutcome, GetHit, GetItem,
    InvalidateItem, InvalidateOutcome, ItemHeader, LookupItem, LookupOutcome, ReleaseCancelItem,
    ReleaseOutcome, RequestBody, ReserveOutcome, ReservePutItem,
};

use crate::TcpClient;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardEndpoint {
    pub shard_id: u64,
    pub address: SocketAddr,
    /// Expected store epoch, obtained from the deployment's fenced membership.
    pub epoch: u64,
}

#[derive(Clone, Debug)]
pub struct ClientLimits {
    pub max_shards: usize,
    pub max_connections: usize,
    pub max_connections_per_shard: usize,
    pub max_retry_entries: usize,
    /// Total attempts for one exact request, including its first attempt.
    pub max_attempts: u8,
    pub max_frame_bytes: usize,
    pub io_timeout: Duration,
    pub max_request_ttl_ms: u64,
    /// Explicit deployment contract, not negotiated over GCK1. None disables
    /// mutation retries. Must not exceed any server's configured replay retention.
    pub mutation_retry_retention_ms: Option<u64>,
}

impl Default for ClientLimits {
    fn default() -> Self {
        Self {
            max_shards: 64,
            max_connections: 64,
            max_connections_per_shard: 4,
            max_retry_entries: 4096,
            max_attempts: 2,
            max_frame_bytes: 1024 * 1024,
            io_timeout: Duration::from_secs(5),
            max_request_ttl_ms: 30_000,
            mutation_retry_retention_ms: None,
        }
    }
}

impl ClientLimits {
    fn validate(&self) -> Result<(), CacheError> {
        if self.max_shards == 0
            || self.max_shards > 4096
            || self.max_connections == 0
            || self.max_connections > 4096
            || self.max_connections_per_shard == 0
            || self.max_connections_per_shard > self.max_connections
            || self.max_retry_entries == 0
            || self.max_retry_entries > 1_048_576
            || self.max_attempts == 0
            || self.max_attempts > 4
            || self.max_frame_bytes < 64
            || self.max_frame_bytes > 32 * 1024 * 1024
            || self.io_timeout.is_zero()
            || self.max_request_ttl_ms == 0
            || self.max_request_ttl_ms > 60_000
            || self
                .mutation_retry_retention_ms
                .is_some_and(|ttl| ttl <= self.max_request_ttl_ms)
        {
            return Err(CacheError::Malformed {
                reason: "shard client config",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientStats {
    pub revision: u64,
    pub shards: usize,
    pub connections: usize,
    pub idle_connections: usize,
    pub retry_entries: usize,
    pub connection_reuses: u64,
    pub retries: u64,
    pub fenced_replies: u64,
}

#[derive(Clone)]
pub struct ShardedClient {
    inner: Arc<Inner>,
}

struct Inner {
    auth: AuthContext,
    limits: ClientLimits,
    state: Mutex<State>,
}

struct State {
    revision: u64,
    endpoints: Vec<ShardEndpoint>,
    idle: BTreeMap<u64, Vec<TcpClient>>,
    connections: usize,
    by_shard: BTreeMap<u64, usize>,
    retries: BTreeMap<(u64, u64, [u8; 16]), RetryEntry>,
    connection_reuses: u64,
    retry_attempts: u64,
    fenced_replies: u64,
}

struct RetryEntry {
    fingerprint: [u8; 32],
    attempts: u8,
    active: bool,
    expires: Instant,
}

/// Exclusive connection lease. Drop returns a synchronized connection to the
/// pool; poisoned or retired connections are closed. No blocking checkout queue.
pub struct ShardSession {
    owner: Arc<Inner>,
    endpoint: ShardEndpoint,
    revision: u64,
    client: Option<TcpClient>,
}

impl ShardedClient {
    pub fn new(
        auth: AuthContext,
        endpoints: Vec<ShardEndpoint>,
        limits: ClientLimits,
    ) -> Result<Self, CacheError> {
        auth.validate()?;
        limits.validate()?;
        let endpoints = validate_endpoints(endpoints, limits.max_shards)?;
        Ok(Self {
            inner: Arc::new(Inner {
                auth,
                limits,
                state: Mutex::new(State {
                    revision: 1,
                    endpoints,
                    idle: BTreeMap::new(),
                    connections: 0,
                    by_shard: BTreeMap::new(),
                    retries: BTreeMap::new(),
                    connection_reuses: 0,
                    retry_attempts: 0,
                    fenced_replies: 0,
                }),
            }),
        })
    }

    pub fn stats(&self) -> ClientStats {
        let mut state = self.inner.lock();
        state.expire();
        ClientStats {
            revision: state.revision,
            shards: state.endpoints.len(),
            connections: state.connections,
            idle_connections: state.idle.values().map(Vec::len).sum(),
            retry_entries: state.retries.len(),
            connection_reuses: state.connection_reuses,
            retries: state.retry_attempts,
            fenced_replies: state.fenced_replies,
        }
    }

    pub fn shard_for(&self, identity: &CacheIdentity) -> Result<u64, CacheError> {
        let hash = self.inner.identity_hash(identity)?;
        Ok(select_endpoint(&self.inner.lock().endpoints, &hash).shard_id)
    }

    /// Route an identity and obtain an exclusive bounded connection lease.
    pub fn route(&self, identity: &CacheIdentity) -> Result<ShardSession, CacheError> {
        let hash = self.inner.identity_hash(identity)?;
        let state = self.inner.lock();
        let endpoint = select_endpoint(&state.endpoints, &hash);
        self.checkout(state, endpoint)
    }

    /// Reopen the original shard for a stored generation. Never hash generation
    /// numbers: two independent stores can both allocate generation 1.
    pub fn shard(&self, shard_id: u64) -> Result<ShardSession, CacheError> {
        let state = self.inner.lock();
        let endpoint = state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.shard_id == shard_id)
            .copied()
            .ok_or(CacheError::UnknownGeneration)?;
        self.checkout(state, endpoint)
    }

    fn checkout(
        &self,
        mut state: MutexGuard<'_, State>,
        endpoint: ShardEndpoint,
    ) -> Result<ShardSession, CacheError> {
        let revision = state.revision;
        if let Some(client) = state.idle.get_mut(&endpoint.shard_id).and_then(Vec::pop) {
            state.connection_reuses = state.connection_reuses.saturating_add(1);
            return Ok(ShardSession {
                owner: Arc::clone(&self.inner),
                endpoint,
                revision,
                client: Some(client),
            });
        }
        if state.by_shard.get(&endpoint.shard_id).copied().unwrap_or(0)
            >= self.inner.limits.max_connections_per_shard
        {
            return Err(quota(QuotaKind::InFlightRequests));
        }
        // An idle socket on another shard must not monopolize the global
        // ceiling. Retire one idle socket before refusing unrelated work.
        if state.connections >= self.inner.limits.max_connections {
            let victim = state.idle.iter().find_map(|(shard, clients)| {
                (*shard != endpoint.shard_id && !clients.is_empty()).then_some(*shard)
            });
            if let Some(shard) = victim {
                state.idle.get_mut(&shard).and_then(Vec::pop);
                state.connections -= 1;
                if let Some(count) = state.by_shard.get_mut(&shard) {
                    *count -= 1;
                    if *count == 0 {
                        state.by_shard.remove(&shard);
                    }
                }
            }
        }
        if state.connections >= self.inner.limits.max_connections {
            return Err(quota(QuotaKind::InFlightRequests));
        }
        state.connections += 1;
        *state.by_shard.entry(endpoint.shard_id).or_default() += 1;
        drop(state);
        let mut session = ShardSession {
            owner: Arc::clone(&self.inner),
            endpoint,
            revision,
            client: None,
        };
        session.reconnect()?;
        Ok(session)
    }

    /// Publish a fully validated snapshot after servers have been fenced.
    /// Every shard must exceed the previous snapshot's highest epoch. Publication
    /// invalidates all old leases; late replies are suppressed. This does not
    /// fence a remote server and is not a distributed membership consensus.
    pub fn replace_routes(
        &self,
        expected_revision: u64,
        endpoints: Vec<ShardEndpoint>,
    ) -> Result<u64, CacheError> {
        let endpoints = validate_endpoints(endpoints, self.inner.limits.max_shards)?;
        let mut state = self.inner.lock();
        if state.revision != expected_revision {
            return Err(CacheError::FencingRejected {
                reason: "route revision",
            });
        }
        // A scalar high-water mark also fences removal followed by re-addition
        // without retaining an unbounded table of retired shard IDs.
        let epoch_floor = state
            .endpoints
            .iter()
            .map(|endpoint| endpoint.epoch)
            .max()
            .unwrap_or(0);
        if endpoints
            .iter()
            .any(|endpoint| endpoint.epoch <= epoch_floor)
        {
            return Err(CacheError::FencingRejected {
                reason: "route epoch",
            });
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| quota(QuotaKind::InFlightRequests))?;
        let idle = std::mem::take(&mut state.idle);
        for (shard, clients) in idle {
            state.connections -= clients.len();
            if let Some(count) = state.by_shard.get_mut(&shard) {
                *count -= clients.len();
                if *count == 0 {
                    state.by_shard.remove(&shard);
                }
            }
        }
        state.revision = revision;
        state.endpoints = endpoints;
        Ok(revision)
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn identity_hash(&self, identity: &CacheIdentity) -> Result<[u8; 32], CacheError> {
        if identity.tenant != self.auth.tenant || identity.trust_domain != self.auth.trust_domain {
            return Err(CacheError::Unauthorized);
        }
        let bytes = encode_physical_key(
            identity,
            &IdentityLimits::from_resources(&ResourceLimits::dev_defaults()),
        )?;
        Ok(content_checksum(&bytes).0)
    }
}

impl State {
    fn expire(&mut self) {
        let now = Instant::now();
        self.retries
            .retain(|_, entry| entry.active || entry.expires > now);
    }
}

impl ShardSession {
    pub fn shard_id(&self) -> u64 {
        self.endpoint.shard_id
    }
    pub fn epoch(&self) -> u64 {
        self.endpoint.epoch
    }
    pub fn deadline(&self, ttl_ms: u64) -> u64 {
        self.client
            .as_ref()
            .map_or(0, |client| client.deadline(ttl_ms))
    }

    pub fn lookup(
        &mut self,
        items: Vec<LookupItem>,
    ) -> Result<BatchReply<LookupOutcome>, CacheError> {
        self.call(RequestBody::Lookup(items), |client, body| {
            let RequestBody::Lookup(items) = body else {
                unreachable!()
            };
            client.lookup(items)
        })
    }
    pub fn reserve_put(
        &mut self,
        items: Vec<ReservePutItem>,
    ) -> Result<BatchReply<ReserveOutcome>, CacheError> {
        self.call(RequestBody::ReservePut(items), |client, body| {
            let RequestBody::ReservePut(items) = body else {
                unreachable!()
            };
            client.reserve_put(items)
        })
    }
    pub fn commit(
        &mut self,
        items: Vec<CommitItem>,
    ) -> Result<BatchReply<CommitOutcome>, CacheError> {
        self.call(RequestBody::Commit(items), |client, body| {
            let RequestBody::Commit(items) = body else {
                unreachable!()
            };
            client.commit(items)
        })
    }
    pub fn get(&mut self, items: Vec<GetItem>) -> Result<BatchReply<GetHit>, CacheError> {
        self.call(RequestBody::Get(items), |client, body| {
            let RequestBody::Get(items) = body else {
                unreachable!()
            };
            client.get(items)
        })
    }
    pub fn release_cancel(
        &mut self,
        items: Vec<ReleaseCancelItem>,
    ) -> Result<BatchReply<ReleaseOutcome>, CacheError> {
        self.call(RequestBody::ReleaseCancel(items), |client, body| {
            let RequestBody::ReleaseCancel(items) = body else {
                unreachable!()
            };
            client.release_cancel(items)
        })
    }
    pub fn invalidate(
        &mut self,
        items: Vec<InvalidateItem>,
    ) -> Result<BatchReply<InvalidateOutcome>, CacheError> {
        self.call(RequestBody::Invalidate(items), |client, body| {
            let RequestBody::Invalidate(items) = body else {
                unreachable!()
            };
            client.invalidate(items)
        })
    }

    fn current(&self) -> Result<(), CacheError> {
        if self.owner.lock().revision != self.revision {
            return Err(CacheError::FencingRejected {
                reason: "route revision",
            });
        }
        Ok(())
    }

    fn reconnect(&mut self) -> Result<(), CacheError> {
        self.current()?;
        self.client = None;
        let client = TcpClient::connect(
            self.endpoint.address,
            self.owner.auth.clone(),
            self.owner.limits.io_timeout,
            self.owner.limits.max_frame_bytes,
        )?;
        if client.epoch() != self.endpoint.epoch {
            return Err(CacheError::EpochMismatch {
                store: client.epoch(),
                request: self.endpoint.epoch,
            });
        }
        self.current()?;
        self.client = Some(client);
        Ok(())
    }

    fn call<T>(
        &mut self,
        request: RequestBody,
        invoke: impl Fn(&mut TcpClient, RequestBody) -> Result<BatchReply<T>, CacheError>,
    ) -> Result<BatchReply<T>, CacheError> {
        self.current()?;
        if request.is_empty() || request.len() > 1024 {
            return Err(quota(QuotaKind::BatchItems));
        }
        let request_headers = headers(&request);
        let mut unique = BTreeSet::new();
        for header in &request_headers {
            if !unique.insert(header.request_id.0) {
                return Err(CacheError::IdempotencyConflict);
            }
            if header.epoch != self.epoch() {
                return Err(CacheError::EpochMismatch {
                    store: self.epoch(),
                    request: header.epoch,
                });
            }
        }
        for identity in identities(&request) {
            let hash = self.owner.identity_hash(identity)?;
            let state = self.owner.lock();
            if select_endpoint(&state.endpoints, &hash).shard_id != self.shard_id() {
                return Err(CacheError::FencingRejected {
                    reason: "wrong shard",
                });
            }
        }
        if let RequestBody::ReservePut(items) = &request {
            let payload_bytes = items.iter().try_fold(0usize, |n, item| {
                n.checked_add(item.payload.as_ref().map_or(0, Vec::len))
            });
            if payload_bytes.is_none_or(|n| n > self.owner.limits.max_frame_bytes) {
                return Err(quota(QuotaKind::InFlightBytes));
            }
        }
        let encoded = encode_request(&self.owner.auth, &request)?;
        if encoded.len() > self.owner.limits.max_frame_bytes {
            return Err(CacheError::ObjectTooLarge {
                limit: self.owner.limits.max_frame_bytes as u64,
                actual: encoded.len() as u64,
            });
        }
        let fingerprint = content_checksum(&encoded).0;
        drop(encoded);
        if self
            .client
            .as_ref()
            .is_none_or(|client| !client.is_usable())
        {
            self.reconnect()?;
        }
        let server_now = self
            .client
            .as_ref()
            .ok_or(CacheError::Cancelled)?
            .server_now();
        let now = Instant::now();
        for header in &request_headers {
            if header.deadline_ms <= server_now {
                return Err(CacheError::TimedOut);
            }
            if header.deadline_ms - server_now > self.owner.limits.max_request_ttl_ms {
                return Err(CacheError::Malformed {
                    reason: "retry deadline",
                });
            }
        }
        // Reads without side effects are retryable. Get creates a reader pin
        // and cannot be retried until the server offers pin idempotency.
        let retryable = matches!(request, RequestBody::Lookup(_))
            || (!matches!(request, RequestBody::Get(_))
                && self.owner.limits.mutation_retry_retention_ms.is_some());
        let attempt_limit = if retryable {
            self.owner.limits.max_attempts
        } else {
            1
        };
        let keys: Vec<_> = request_headers
            .iter()
            .map(|header| (self.shard_id(), self.epoch(), header.request_id.0))
            .collect();
        let mut state = self.owner.lock();
        state.expire();
        let mut new_entries = 0;
        for key in &keys {
            if let Some(entry) = state.retries.get(key) {
                if entry.fingerprint != fingerprint || entry.active {
                    return Err(CacheError::IdempotencyConflict);
                }
                if entry.attempts >= attempt_limit {
                    return Err(quota(QuotaKind::IdempotencyTable));
                }
            } else {
                new_entries += 1;
            }
        }
        if state.retries.len().saturating_add(new_entries) > self.owner.limits.max_retry_entries {
            return Err(quota(QuotaKind::IdempotencyTable));
        }
        for (key, header) in keys.iter().zip(request_headers) {
            state
                .retries
                .entry(*key)
                .or_insert(RetryEntry {
                    fingerprint,
                    attempts: 0,
                    active: false,
                    expires: now + Duration::from_millis(header.deadline_ms - server_now),
                })
                .active = true;
        }
        drop(state);
        let _guard = RetryGuard {
            owner: Arc::clone(&self.owner),
            keys: keys.clone(),
        };
        loop {
            self.current()?;
            if self
                .client
                .as_ref()
                .is_none_or(|client| !client.is_usable())
            {
                self.reconnect()?;
            }
            let server_now = self
                .client
                .as_ref()
                .ok_or(CacheError::Cancelled)?
                .server_now();
            if headers(&request)
                .iter()
                .any(|header| header.deadline_ms <= server_now)
            {
                return Err(CacheError::TimedOut);
            }
            let mut state = self.owner.lock();
            if keys
                .iter()
                .any(|key| state.retries[key].attempts >= attempt_limit)
            {
                return Err(quota(QuotaKind::IdempotencyTable));
            }
            for key in &keys {
                let entry = state.retries.get_mut(key).ok_or(CacheError::Cancelled)?;
                entry.attempts += 1;
            }
            drop(state);
            let result = invoke(
                self.client.as_mut().ok_or(CacheError::Cancelled)?,
                request.clone(),
            );
            if let Err(error) = self.current() {
                let mut state = self.owner.lock();
                state.fenced_replies = state.fenced_replies.saturating_add(1);
                return Err(error);
            }
            let retry = retryable
                && matches!(
                    &result,
                    Err(CacheError::TimedOut
                        | CacheError::Storage {
                            reason: "transport io"
                        })
                );
            if !retry {
                return result;
            }
            let mut state = self.owner.lock();
            if keys
                .iter()
                .any(|key| state.retries[key].attempts >= attempt_limit)
            {
                return result;
            }
            state.retry_attempts = state.retry_attempts.saturating_add(1);
            drop(state);
            // Closing the old socket precedes reconnect; no queued request and
            // no retry on another shard, address, revision, or epoch.
            self.client = None;
        }
    }
}

impl Drop for ShardSession {
    fn drop(&mut self) {
        let mut state = self.owner.lock();
        if state.revision == self.revision && self.client.as_ref().is_some_and(TcpClient::is_usable)
        {
            if let Some(client) = self.client.take() {
                state.idle.entry(self.shard_id()).or_default().push(client);
                return;
            }
        }
        state.connections -= 1;
        if let Some(count) = state.by_shard.get_mut(&self.shard_id()) {
            *count -= 1;
            if *count == 0 {
                state.by_shard.remove(&self.shard_id());
            }
        }
    }
}

struct RetryGuard {
    owner: Arc<Inner>,
    keys: Vec<(u64, u64, [u8; 16])>,
}
impl Drop for RetryGuard {
    fn drop(&mut self) {
        let mut state = self.owner.lock();
        for key in &self.keys {
            if let Some(entry) = state.retries.get_mut(key) {
                entry.active = false;
            }
        }
        state.expire();
    }
}

fn headers(request: &RequestBody) -> Vec<&ItemHeader> {
    match request {
        RequestBody::Lookup(items) => items.iter().map(|item| &item.header).collect(),
        RequestBody::ReservePut(items) => items.iter().map(|item| &item.header).collect(),
        RequestBody::Commit(items) => items.iter().map(|item| &item.header).collect(),
        RequestBody::Get(items) => items.iter().map(|item| &item.header).collect(),
        RequestBody::ReleaseCancel(items) => items.iter().map(|item| &item.header).collect(),
        RequestBody::Invalidate(items) => items.iter().map(|item| &item.header).collect(),
    }
}
fn identities(request: &RequestBody) -> Vec<&CacheIdentity> {
    match request {
        RequestBody::Lookup(items) => items.iter().map(|item| &item.identity).collect(),
        RequestBody::ReservePut(items) => items.iter().map(|item| &item.identity).collect(),
        RequestBody::Get(items) => items.iter().map(|item| &item.identity).collect(),
        _ => Vec::new(),
    }
}
fn quota(kind: QuotaKind) -> CacheError {
    CacheError::QuotaExceeded { kind }
}
fn validate_endpoints(
    mut endpoints: Vec<ShardEndpoint>,
    max: usize,
) -> Result<Vec<ShardEndpoint>, CacheError> {
    if endpoints.is_empty() || endpoints.len() > max {
        return Err(quota(QuotaKind::InFlightRequests));
    }
    endpoints.sort_by_key(|endpoint| endpoint.shard_id);
    let mut addresses = BTreeSet::new();
    for (index, endpoint) in endpoints.iter().enumerate() {
        if endpoint.epoch == 0
            || !addresses.insert(endpoint.address)
            || index > 0 && endpoints[index - 1].shard_id == endpoint.shard_id
        {
            return Err(CacheError::Malformed {
                reason: "shard endpoints",
            });
        }
    }
    Ok(endpoints)
}
fn select_endpoint(endpoints: &[ShardEndpoint], hash: &[u8; 32]) -> ShardEndpoint {
    // Rendezvous placement: stable for endpoint ordering and address changes.
    *endpoints
        .iter()
        .max_by_key(|endpoint| {
            let mut key = [0u8; 40];
            key[..32].copy_from_slice(hash);
            key[32..].copy_from_slice(&endpoint.shard_id.to_le_bytes());
            content_checksum(&key).0
        })
        .expect("validated nonempty endpoints")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_envelope, write_envelope};
    use crate::pool::Pool;
    use gpucache_core_types::{synthetic_identity, RequestId};
    use gpucache_protocol::{
        decode_request, encode_hello_ack, encode_reply, ItemResult, MissReason, ReplyBody,
        TenantPolicy, WIRE_VERSION,
    };
    use gpucache_storage::MemoryStore;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    fn auth() -> AuthContext {
        let id = synthetic_identity(1, 1);
        AuthContext::new(id.tenant, id.trust_domain, [9; 32]).unwrap()
    }
    fn accept(listener: &TcpListener, epoch: u64) -> (TcpStream, Arc<Pool>) {
        listener.set_nonblocking(true).unwrap();
        let start = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && start.elapsed() < Duration::from_secs(3) =>
                {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let pool = Arc::new(Pool::new(1));
        drop(read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap());
        write_envelope(&mut stream, 1, &encode_hello_ack(WIRE_VERSION, epoch), 4096).unwrap();
        (stream, pool)
    }
    fn client(listener: &TcpListener, mutation_retry_retention_ms: Option<u64>) -> ShardedClient {
        ShardedClient::new(
            auth(),
            vec![ShardEndpoint {
                shard_id: 0,
                address: listener.local_addr().unwrap(),
                epoch: 1,
            }],
            ClientLimits {
                max_frame_bytes: 4096,
                io_timeout: Duration::from_secs(2),
                mutation_retry_retention_ms,
                ..ClientLimits::default()
            },
        )
        .unwrap()
    }
    fn reserve(session: &ShardSession) -> ReservePutItem {
        ReservePutItem {
            header: ItemHeader {
                request_id: RequestId::from_counter(1).unwrap(),
                epoch: session.epoch(),
                deadline_ms: session.deadline(1000),
            },
            identity: synthetic_identity(1, 1),
            generation: 0,
            length: 4,
            checksum: content_checksum(&[1, 2, 3, 4]),
            payload: Some(vec![1, 2, 3, 4]),
            lease_ttl_ms: 1000,
        }
    }

    #[test]
    fn ambiguous_put_retries_identical_bytes_once_without_duplicate_allocation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = client(&listener, Some(60_000));
        let worker = std::thread::spawn(move || {
            let auth = auth();
            let policy = TenantPolicy::new(auth.tenant, auth.trust_domain, [9; 32]).unwrap();
            let mut store = MemoryStore::new(ResourceLimits::dev_defaults(), vec![policy]).unwrap();
            let mut original = Vec::new();
            for attempt in 0..2 {
                let (mut stream, pool) = accept(&listener, 1);
                let envelope = read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap();
                let bytes = envelope.body().to_vec();
                if attempt == 0 {
                    original = bytes.clone();
                } else {
                    assert_eq!(bytes, original);
                }
                let (auth, RequestBody::ReservePut(items)) = decode_request(&bytes).unwrap() else {
                    panic!("reserve")
                };
                let reply = store.reserve_put(&auth, 2, items).unwrap();
                assert_eq!(reply.items[0].body.as_ref().unwrap().generation, 1);
                if attempt == 1 {
                    write_envelope(
                        &mut stream,
                        2,
                        &encode_reply(&ReplyBody::Reserve(reply)).unwrap(),
                        4096,
                    )
                    .unwrap();
                }
                // First acknowledgement is deliberately lost after the mutation.
            }
            assert_eq!(store.stats().retained_bytes, 4);
            assert_eq!(store.stats().in_flight_requests, 1);
            assert_eq!(store.stats().idempotency_entries, 1);
        });
        let mut session = client.shard(0).unwrap();
        let item = reserve(&session);
        assert_eq!(
            session.reserve_put(vec![item.clone()]).unwrap().items[0]
                .body
                .as_ref()
                .unwrap()
                .generation,
            1
        );
        assert_eq!(client.stats().retries, 1);
        assert_eq!(client.stats().retry_entries, 1);
        assert_eq!(
            session.reserve_put(vec![item]),
            Err(quota(QuotaKind::IdempotencyTable))
        );
        worker.join().unwrap();
    }

    #[test]
    fn retry_reconnect_epoch_change_never_sends_the_old_mutation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = client(&listener, Some(60_000));
        let worker = std::thread::spawn(move || {
            let (mut stream, pool) = accept(&listener, 1);
            drop(read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap());
            drop(stream);
            let (mut stream, pool) = accept(&listener, 2);
            // The old request never reaches the new epoch after its hello.
            assert!(read_envelope(&mut stream, pool.checkout().unwrap(), 4096).is_err());
        });
        let mut session = client.shard(0).unwrap();
        let item = reserve(&session);
        assert_eq!(
            session.reserve_put(vec![item]),
            Err(CacheError::EpochMismatch {
                store: 2,
                request: 1
            })
        );
        worker.join().unwrap();
        drop(session);
        assert_eq!(client.stats().connections, 0);
    }

    #[test]
    fn default_mutations_and_all_gets_never_retry_after_lost_reply() {
        for get in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = client(&listener, if get { Some(60_000) } else { None });
            let worker = std::thread::spawn(move || {
                let (mut stream, pool) = accept(&listener, 1);
                drop(read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap());
                // Drop both stream and listener. Any accidental retry is visible
                // in the client's retry counter even though reconnect fails.
            });
            let mut session = client.shard(0).unwrap();
            let item = reserve(&session);
            let result = if get {
                session
                    .get(vec![GetItem {
                        header: item.header,
                        generation: 1,
                        identity: item.identity,
                        pin_ttl_ms: 1000,
                    }])
                    .map(|_| ())
            } else {
                session.reserve_put(vec![item]).map(|_| ())
            };
            assert!(result.is_err());
            assert_eq!(client.stats().retries, 0);
            worker.join().unwrap();
        }
    }

    #[test]
    fn late_reply_after_route_publication_is_suppressed_and_socket_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = client(&listener, None);
        let address = listener.local_addr().unwrap();
        let (arrived, wait_arrived) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (mut stream, pool) = accept(&listener, 1);
            let envelope = read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap();
            let (_, RequestBody::Lookup(items)) = decode_request(envelope.body()).unwrap() else {
                panic!("lookup")
            };
            arrived.send(()).unwrap();
            wait_release.recv().unwrap();
            write_envelope(
                &mut stream,
                2,
                &encode_reply(&ReplyBody::Lookup(BatchReply {
                    store_epoch: 1,
                    items: vec![ItemResult {
                        request_id: items[0].header.request_id,
                        body: Ok(LookupOutcome::Miss {
                            reason: MissReason::Absent,
                        }),
                    }],
                }))
                .unwrap(),
                4096,
            )
            .unwrap();
        });
        let mut session = client.shard(0).unwrap();
        let call = std::thread::spawn(move || {
            let item = LookupItem {
                header: reserve(&session).header,
                identity: synthetic_identity(1, 1),
            };
            session.lookup(vec![item])
        });
        wait_arrived.recv_timeout(Duration::from_secs(3)).unwrap();
        client
            .replace_routes(
                1,
                vec![ShardEndpoint {
                    shard_id: 0,
                    address,
                    epoch: 2,
                }],
            )
            .unwrap();
        release.send(()).unwrap();
        assert_eq!(
            call.join().unwrap(),
            Err(CacheError::FencingRejected {
                reason: "route revision"
            })
        );
        worker.join().unwrap();
        assert_eq!(client.stats().fenced_replies, 1);
        assert_eq!(client.stats().connections, 0);
    }

    #[test]
    fn removing_then_readding_shard_cannot_reset_epoch_high_water_mark() {
        let first: SocketAddr = "127.0.0.1:10001".parse().unwrap();
        let second: SocketAddr = "127.0.0.1:10002".parse().unwrap();
        let client = ShardedClient::new(
            auth(),
            vec![ShardEndpoint {
                shard_id: 1,
                address: first,
                epoch: 8,
            }],
            ClientLimits::default(),
        )
        .unwrap();
        client
            .replace_routes(
                1,
                vec![ShardEndpoint {
                    shard_id: 2,
                    address: second,
                    epoch: 9,
                }],
            )
            .unwrap();
        assert_eq!(
            client.replace_routes(
                2,
                vec![ShardEndpoint {
                    shard_id: 1,
                    address: first,
                    epoch: 8
                }]
            ),
            Err(CacheError::FencingRejected {
                reason: "route epoch"
            })
        );
        assert_eq!(client.stats().revision, 2);
        assert_eq!(client.stats().shards, 1);
    }
}
