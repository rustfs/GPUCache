// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gpucache_core_types::{Attestation, CacheError, QuotaKind};
use gpucache_protocol::{
    decode_hello, decode_request, encode_hello_ack, encode_reply, negotiate, request_op, Acl,
    AuditEvent, AuthContext, ReplyBody, RequestBody, SecurityLog, OP_HELLO, WIRE_VERSION,
};
use gpucache_storage::FileStore;

use crate::clock::mono_ms;
use crate::frame::{read_envelope, write_envelope};
use crate::pool::Pool;
use crate::rate::RateGate;

const MAX_TENANTS: usize = 1024;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub allow_remote: bool,
    pub max_connections: usize,
    pub max_frame_bytes: usize,
    pub pool_buffers: usize,
    pub io_timeout: Duration,
    /// Optional restriction of the store's policies. `None` grants configured
    /// store principals read/write; `Some(empty_acl)` denies every data call.
    /// An ACL grant never bypasses the store's credential/tenant check.
    pub acl: Option<Acl>,
    /// Maximum admitted data items per tenant per window, across connections.
    pub rate_limit_items: u32,
    pub rate_window_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            allow_remote: false,
            max_connections: 16,
            max_frame_bytes: 1024 * 1024,
            pool_buffers: 16,
            io_timeout: Duration::from_secs(5),
            acl: None,
            rate_limit_items: 16_384,
            rate_window_ms: 1_000,
        }
    }
}

impl ServerConfig {
    pub fn validate(&self) -> Result<(), CacheError> {
        if self.max_connections == 0
            || self.pool_buffers == 0
            || self.max_frame_bytes < 64
            || self.io_timeout.is_zero()
            || self.rate_limit_items == 0
            || self.rate_window_ms == 0
        {
            return Err(CacheError::Malformed {
                reason: "server config",
            });
        }
        if self.max_frame_bytes > 32 * 1024 * 1024 {
            return Err(CacheError::ObjectTooLarge {
                limit: 32 * 1024 * 1024,
                actual: self.max_frame_bytes as u64,
            });
        }
        if !self.allow_remote && !self.bind.ip().is_loopback() {
            return Err(CacheError::Malformed {
                reason: "bind must be loopback",
            });
        }
        Ok(())
    }
}

pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    security: Arc<Mutex<HotSecurity>>,
}

impl ServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Bounded audit snapshot; no fingerprints, request identities or payloads.
    pub fn security_events(&self) -> Vec<AuditEvent> {
        self.security
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .log
            .events()
            .copied()
            .collect()
    }

    pub fn denied_requests(&self) -> u64 {
        self.security
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .denied
    }

    pub fn rate_limited_requests(&self) -> u64 {
        self.security
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .rate_limited
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let mut wake_addr = self.addr;
        if wake_addr.ip().is_unspecified() {
            wake_addr.set_ip(if wake_addr.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        let _ = TcpStream::connect(wake_addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Shared {
    store: Mutex<FileStore>,
    pool: Arc<Pool>,
    cfg: ServerConfig,
    shutdown: Arc<AtomicBool>,
    connections: AtomicUsize,
    security: Arc<Mutex<HotSecurity>>,
}

struct HotSecurity {
    principals: Acl,
    restriction: Option<Acl>,
    rate: RateGate,
    log: SecurityLog,
    denied: u64,
    rate_limited: u64,
}

impl HotSecurity {
    fn open(store: &FileStore, cfg: &ServerConfig) -> Result<Self, CacheError> {
        let mut principals = Acl::new(MAX_TENANTS)?;
        for policy in store.tenant_policies() {
            principals.grant(
                AuthContext::new(
                    policy.tenant,
                    policy.trust_domain,
                    policy.credential_fingerprint,
                )?,
                true,
            )?;
        }
        Ok(Self {
            principals,
            restriction: cfg.acl.clone(),
            rate: RateGate::new(cfg.rate_limit_items, cfg.rate_window_ms, MAX_TENANTS)?,
            log: SecurityLog::new(256)?,
            denied: 0,
            rate_limited: 0,
        })
    }

    fn admit(
        &mut self,
        auth: &AuthContext,
        request: &RequestBody,
        now: u64,
    ) -> Result<(), CacheError> {
        // Reader pin releases must remain available to a read-only principal.
        // A mixed batch that includes a writer cancellation requires write.
        let write = match request {
            RequestBody::Lookup(_) | RequestBody::Get(_) => false,
            RequestBody::ReleaseCancel(items) => items.iter().any(|item| item.pin_owner.is_none()),
            RequestBody::ReservePut(_) | RequestBody::Commit(_) | RequestBody::Invalidate(_) => {
                true
            }
        };
        let authorized = self.principals.check(auth, write).and_then(|()| {
            self.restriction
                .as_ref()
                .map_or(Ok(()), |acl| acl.check(auth, write))
        });
        if let Err(error) = authorized {
            self.denied = self.denied.saturating_add(1);
            self.log.push(AuditEvent::Deny);
            return Err(error);
        }
        if request.is_empty() {
            return Err(CacheError::Malformed {
                reason: "empty batch",
            });
        }
        // A full admission window must not strand an existing reader pin.
        // Storage still checks tenant, region and pin owner, and its bounded
        // idempotency table applies. Unknown completion does not bypass rate
        // admission because it cannot release the held reference.
        if matches!(request, RequestBody::ReleaseCancel(items) if items.iter().all(|item| {
            item.pin_owner.is_some()
                && item.region_id.is_some()
                && item.attestation == Attestation::Drained
        })) {
            return Ok(());
        }
        let result = self
            .rate
            .admit_batch(auth.tenant.as_bytes(), now, request.len());
        if matches!(
            result,
            Err(CacheError::QuotaExceeded {
                kind: QuotaKind::RateLimit
            })
        ) {
            self.rate_limited = self.rate_limited.saturating_add(1);
        }
        result
    }
}

/// Bind a loopback TCP cache service. `store` must already have tenant policies.
pub fn serve(store: FileStore, cfg: ServerConfig) -> Result<ServerHandle, CacheError> {
    cfg.validate()?;
    let security = Arc::new(Mutex::new(HotSecurity::open(&store, &cfg)?));
    let listener =
        TcpListener::bind(cfg.bind).map_err(|_| CacheError::Storage { reason: "bind" })?;
    let addr = listener
        .local_addr()
        .map_err(|_| CacheError::Storage { reason: "bind" })?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        store: Mutex::new(store),
        pool: Arc::new(Pool::new(cfg.pool_buffers)),
        cfg,
        shutdown: Arc::clone(&shutdown),
        connections: AtomicUsize::new(0),
        security: Arc::clone(&security),
    });
    let thread_shared = Arc::clone(&shared);
    let thread = std::thread::Builder::new()
        .name("gpucache-tcp".to_string())
        .spawn(move || accept_loop(listener, thread_shared))
        .map_err(|_| CacheError::Storage { reason: "bind" })?;
    Ok(ServerHandle {
        addr,
        shutdown,
        thread: Some(thread),
        security,
    })
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    // Keep bounded ownership of workers. Drop must interrupt and join them
    // before the file-backed directory can safely be reopened after restart.
    let mut workers: Vec<(JoinHandle<()>, TcpStream)> = Vec::new();
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let Ok((stream, _)) = listener.accept() else {
            break;
        };
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let mut index = 0;
        while index < workers.len() {
            if workers[index].0.is_finished() {
                let (worker, _) = workers.swap_remove(index);
                let _ = worker.join();
            } else {
                index += 1;
            }
        }
        let Ok(shutdown_stream) = stream.try_clone() else {
            continue;
        };
        let previous = shared.connections.fetch_add(1, Ordering::Relaxed);
        if previous >= shared.cfg.max_connections || workers.len() >= shared.cfg.max_connections {
            shared.connections.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let shared_for_thread = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("gpucache-conn".to_string())
            .spawn(move || {
                struct Guard(Arc<Shared>);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        self.0.connections.fetch_sub(1, Ordering::Relaxed);
                    }
                }
                let _guard = Guard(Arc::clone(&shared_for_thread));
                connection(stream, shared_for_thread);
            });
        match spawned {
            Ok(worker) => workers.push((worker, shutdown_stream)),
            Err(_) => {
                shared.connections.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
    for (_, stream) in &workers {
        let _ = stream.shutdown(Shutdown::Both);
    }
    for (worker, _) in workers {
        let _ = worker.join();
    }
}

fn connection(mut stream: TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_read_timeout(Some(shared.cfg.io_timeout));
    let _ = stream.set_write_timeout(Some(shared.cfg.io_timeout));
    let _ = stream.set_nodelay(true);
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let pooled = match shared.pool.checkout() {
            Ok(buf) => buf,
            Err(_) => break,
        };
        let envelope = match read_envelope(&mut stream, pooled, shared.cfg.max_frame_bytes) {
            Ok(envelope) => envelope,
            Err(_) => break,
        };
        let reply = handle_frame(&shared, envelope.body());
        // The decoder owns the request; return the read buffer before writing.
        drop(envelope);
        if write_envelope(&mut stream, mono_ms(), &reply, shared.cfg.max_frame_bytes).is_err() {
            break;
        }
    }
}

fn handle_frame(shared: &Shared, body: &[u8]) -> Vec<u8> {
    let store = &shared.store;
    if body.len() >= 8 && &body[..4] == b"GCK1" {
        let op = u16::from_le_bytes([body[6], body[7]]);
        if op == OP_HELLO {
            let negotiated =
                decode_hello(body).and_then(|versions| negotiate(&[WIRE_VERSION], &versions));
            let epoch = store.lock().unwrap_or_else(|err| err.into_inner()).epoch();
            return match negotiated {
                Ok(version) => encode_hello_ack(version, epoch),
                Err(err) => fail(OP_HELLO, epoch, &err),
            };
        }
    }
    let decoded = decode_request(body);
    let reply = {
        let mut store = store.lock().unwrap_or_else(|err| err.into_inner());
        // Time is checked after waiting for the store, not before contention.
        match decoded {
            Ok((auth, request)) => {
                let now = mono_ms();
                let admission = shared
                    .security
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .admit(&auth, &request, now);
                match admission {
                    Ok(()) => dispatch(&mut store, &auth, request, now),
                    Err(error) => ReplyBody::Failed {
                        op: request_op(&request),
                        store_epoch: store.epoch(),
                        error,
                    },
                }
            }
            Err(error) => ReplyBody::Failed {
                op: 0,
                store_epoch: store.epoch(),
                error,
            },
        }
    };
    encode_reply(&reply).unwrap_or_else(|error| {
        let epoch = store.lock().unwrap_or_else(|err| err.into_inner()).epoch();
        fail(0, epoch, &error)
    })
}

fn dispatch(
    store: &mut FileStore,
    auth: &gpucache_protocol::AuthContext,
    request: RequestBody,
    now: u64,
) -> ReplyBody {
    let op = request_op(&request);
    let result = match request {
        RequestBody::Lookup(items) => store.lookup(auth, now, items).map(ReplyBody::Lookup),
        RequestBody::ReservePut(items) => {
            store.reserve_put(auth, now, items).map(ReplyBody::Reserve)
        }
        RequestBody::Commit(items) => store.commit(auth, now, items).map(ReplyBody::Commit),
        RequestBody::Get(items) => store.get(auth, now, items).map(ReplyBody::Get),
        RequestBody::ReleaseCancel(items) => store
            .release_cancel(auth, now, items)
            .map(ReplyBody::Release),
        RequestBody::Invalidate(items) => store
            .invalidate(auth, now, items)
            .map(ReplyBody::Invalidate),
    };
    result.unwrap_or_else(|error| ReplyBody::Failed {
        op,
        store_epoch: store.epoch(),
        error,
    })
}

fn fail(op: u16, epoch: u64, err: &CacheError) -> Vec<u8> {
    encode_reply(&ReplyBody::Failed {
        op,
        store_epoch: epoch,
        error: err.clone(),
    })
    .unwrap_or_else(|_| encode_hello_ack(WIRE_VERSION, epoch))
}
