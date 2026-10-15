// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use gpucache_core_types::CacheError;
use gpucache_protocol::{
    decode_hello_ack, decode_reply, encode_hello, encode_request, AuthContext, BatchReply,
    CommitItem, CommitOutcome, GetHit, GetItem, InvalidateItem, InvalidateOutcome, LookupItem,
    LookupOutcome, ReleaseCancelItem, ReleaseOutcome, ReplyBody, RequestBody, ReserveOutcome,
    ReservePutItem, WIRE_VERSION,
};

use crate::clock::Clock;
use crate::frame::{read_envelope, write_envelope};
use crate::pool::Pool;

pub struct TcpClient {
    stream: TcpStream,
    auth: AuthContext,
    pool: Arc<Pool>,
    max_frame: usize,
    epoch: u64,
    clock: Clock,
    usable: bool,
}

impl TcpClient {
    pub fn connect(
        addr: SocketAddr,
        auth: AuthContext,
        timeout: Duration,
        max_frame: usize,
    ) -> Result<Self, CacheError> {
        if max_frame < 64 {
            return Err(CacheError::Malformed {
                reason: "server config",
            });
        }
        let mut stream =
            TcpStream::connect_timeout(&addr, timeout).map_err(|_| CacheError::Storage {
                reason: "transport io",
            })?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| CacheError::Storage {
                reason: "transport io",
            })?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|_| CacheError::Storage {
                reason: "transport io",
            })?;
        let _ = stream.set_nodelay(true);
        let pool = Arc::new(Pool::new(1));
        let hello = encode_hello(&[WIRE_VERSION])?;
        write_envelope(&mut stream, 0, &hello, max_frame)?;
        let envelope = read_envelope(&mut stream, pool.checkout()?, max_frame)?;
        let (version, epoch) = decode_hello_ack(envelope.body())?;
        if version != WIRE_VERSION {
            return Err(CacheError::WireVersionUnsupported {
                local: WIRE_VERSION,
                remote: version,
            });
        }
        Ok(Self {
            stream,
            auth,
            pool,
            max_frame,
            epoch,
            clock: Clock::new(envelope.server_now),
            usable: true,
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn deadline(&self, ttl_ms: u64) -> u64 {
        self.clock.server_now().saturating_add(ttl_ms).max(1)
    }

    pub fn lookup(
        &mut self,
        items: Vec<LookupItem>,
    ) -> Result<BatchReply<LookupOutcome>, CacheError> {
        self.call(RequestBody::Lookup(items), |body| match body {
            ReplyBody::Lookup(batch) => Ok(batch),
            _ => Err(CacheError::Malformed { reason: "reply op" }),
        })
    }

    pub fn reserve_put(
        &mut self,
        items: Vec<ReservePutItem>,
    ) -> Result<BatchReply<ReserveOutcome>, CacheError> {
        self.call(RequestBody::ReservePut(items), |body| match body {
            ReplyBody::Reserve(batch) => Ok(batch),
            _ => Err(CacheError::Malformed { reason: "reply op" }),
        })
    }

    pub fn commit(
        &mut self,
        items: Vec<CommitItem>,
    ) -> Result<BatchReply<CommitOutcome>, CacheError> {
        self.call(RequestBody::Commit(items), |body| match body {
            ReplyBody::Commit(batch) => Ok(batch),
            _ => Err(CacheError::Malformed { reason: "reply op" }),
        })
    }

    pub fn get(&mut self, items: Vec<GetItem>) -> Result<BatchReply<GetHit>, CacheError> {
        self.call(RequestBody::Get(items), |body| match body {
            ReplyBody::Get(batch) => Ok(batch),
            _ => Err(CacheError::Malformed { reason: "reply op" }),
        })
    }

    pub fn release_cancel(
        &mut self,
        items: Vec<ReleaseCancelItem>,
    ) -> Result<BatchReply<ReleaseOutcome>, CacheError> {
        self.call(RequestBody::ReleaseCancel(items), |body| match body {
            ReplyBody::Release(batch) => Ok(batch),
            _ => Err(CacheError::Malformed { reason: "reply op" }),
        })
    }

    pub fn invalidate(
        &mut self,
        items: Vec<InvalidateItem>,
    ) -> Result<BatchReply<InvalidateOutcome>, CacheError> {
        self.call(RequestBody::Invalidate(items), |body| match body {
            ReplyBody::Invalidate(batch) => Ok(batch),
            _ => Err(CacheError::Malformed { reason: "reply op" }),
        })
    }

    fn call<T>(
        &mut self,
        request: RequestBody,
        map: impl FnOnce(ReplyBody) -> Result<T, CacheError>,
    ) -> Result<T, CacheError> {
        if !self.usable {
            return Err(CacheError::Storage {
                reason: "transport io",
            });
        }
        let bytes = encode_request(&self.auth, &request)?;
        // No request correlation exists at the envelope layer. After an
        // incomplete exchange, a late reply must never answer a later call.
        self.usable = false;
        write_envelope(&mut self.stream, 0, &bytes, self.max_frame)?;
        let envelope = read_envelope(&mut self.stream, self.pool.checkout()?, self.max_frame)?;
        self.clock.observe(envelope.server_now);
        match decode_reply(envelope.body())? {
            ReplyBody::Failed { error, .. } => {
                self.usable = true;
                Err(error)
            }
            other => {
                let result = map(other);
                if result.is_ok() {
                    self.usable = true;
                }
                result
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpucache_core_types::{synthetic_identity, RequestId};
    use gpucache_protocol::{encode_hello_ack, encode_reply, ItemHeader, ItemResult};
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn timed_out_connection_cannot_consume_a_late_reply_for_another_call() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release, wait) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let pool = Arc::new(Pool::new(1));
            drop(read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap());
            write_envelope(&mut stream, 1, &encode_hello_ack(WIRE_VERSION, 1), 4096).unwrap();
            drop(read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap());
            wait.recv().unwrap();
            let body = encode_reply(&ReplyBody::Lookup(BatchReply {
                store_epoch: 1,
                items: vec![ItemResult {
                    request_id: RequestId::from_counter(1).unwrap(),
                    body: Ok(LookupOutcome::Miss {
                        reason: gpucache_protocol::MissReason::Absent,
                    }),
                }],
            }))
            .unwrap();
            write_envelope(&mut stream, 1, &body, 4096).unwrap();
        });
        let identity = synthetic_identity(1, 1);
        let auth = AuthContext::new(
            identity.tenant.clone(),
            identity.trust_domain.clone(),
            [9; 32],
        )
        .unwrap();
        let mut client = TcpClient::connect(addr, auth, Duration::from_secs(5), 4096).unwrap();
        client
            .stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let item = LookupItem {
            header: ItemHeader {
                request_id: RequestId::from_counter(1).unwrap(),
                epoch: 1,
                deadline_ms: 1000,
            },
            identity,
        };
        assert_eq!(client.lookup(vec![item.clone()]), Err(CacheError::TimedOut));
        release.send(()).unwrap();
        server.join().unwrap();
        let mut next = item;
        next.header.request_id = RequestId::from_counter(2).unwrap();
        assert!(matches!(
            client.lookup(vec![next]),
            Err(CacheError::Storage {
                reason: "transport io"
            })
        ));
    }
}
