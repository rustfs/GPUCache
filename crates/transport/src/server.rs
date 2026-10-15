// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gpucache_core_types::CacheError;
use gpucache_protocol::{
    decode_hello, decode_request, encode_hello_ack, encode_reply, negotiate, request_op, ReplyBody,
    RequestBody, OP_HELLO, WIRE_VERSION,
};
use gpucache_storage::FileStore;

use crate::clock::mono_ms;
use crate::frame::{read_envelope, write_envelope};
use crate::pool::Pool;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub allow_remote: bool,
    pub max_connections: usize,
    pub max_frame_bytes: usize,
    pub pool_buffers: usize,
    pub io_timeout: Duration,
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
        }
    }
}

impl ServerConfig {
    pub fn validate(&self) -> Result<(), CacheError> {
        if self.max_connections == 0
            || self.pool_buffers == 0
            || self.max_frame_bytes < 64
            || self.io_timeout.is_zero()
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
}

impl ServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
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
}

/// Bind a loopback TCP cache service. `store` must already have tenant policies.
pub fn serve(store: FileStore, cfg: ServerConfig) -> Result<ServerHandle, CacheError> {
    cfg.validate()?;
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
    })
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
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
        let previous = shared.connections.fetch_add(1, Ordering::Relaxed);
        if previous >= shared.cfg.max_connections {
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
        if spawned.is_err() {
            shared.connections.fetch_sub(1, Ordering::Relaxed);
        }
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
        let reply = handle_frame(&shared.store, envelope.body());
        // The decoder owns the request; return the read buffer before writing.
        drop(envelope);
        if write_envelope(&mut stream, mono_ms(), &reply, shared.cfg.max_frame_bytes).is_err() {
            break;
        }
    }
}

fn handle_frame(store: &Mutex<FileStore>, body: &[u8]) -> Vec<u8> {
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
            Ok((auth, request)) => dispatch(&mut store, &auth, request, mono_ms()),
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
