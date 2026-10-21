// Copyright 2024-2026 RustFS, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use gpucache_core_types::{CacheError, RequestId};
use gpucache_protocol::{
    decode_hello_ack, decode_reply, encode_hello, encode_request, request_op, AuthContext,
    BatchReply, CommitItem, CommitOutcome, GetHit, GetItem, InvalidateItem, InvalidateOutcome,
    LookupItem, LookupOutcome, ReleaseCancelItem, ReleaseOutcome, ReplyBody, RequestBody,
    ReserveOutcome, ReservePutItem, WIRE_VERSION,
};

use crate::clock::Clock;
use crate::frame::{read_envelope, write_envelope};
use crate::pool::Pool;

/// Counts complete application envelopes, including the hello exchange.
/// A framed envelope adds 12 bytes. TCP/IP headers, retransmission, and partial
/// failed writes/reads are excluded; these counters are not NIC wire telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientIoStats {
    pub rpc_body_bytes_sent: u64,
    pub rpc_body_bytes_received: u64,
    pub framed_bytes_sent: u64,
    pub framed_bytes_received: u64,
}

pub struct TcpClient {
    stream: TcpStream,
    auth: AuthContext,
    pool: Arc<Pool>,
    max_frame: usize,
    epoch: u64,
    clock: Clock,
    usable: bool,
    io_stats: ClientIoStats,
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
            io_stats: ClientIoStats {
                rpc_body_bytes_sent: hello.len() as u64,
                rpc_body_bytes_received: envelope.body().len() as u64,
                framed_bytes_sent: hello.len() as u64 + 12,
                framed_bytes_received: envelope.body().len() as u64 + 12,
            },
        })
    }

    pub fn io_stats(&self) -> ClientIoStats {
        self.io_stats
    }

    pub(crate) fn is_usable(&self) -> bool {
        self.usable
    }

    pub(crate) fn server_now(&self) -> u64 {
        self.clock.server_now()
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
        map: impl FnOnce(ReplyBody) -> Result<BatchReply<T>, CacheError>,
    ) -> Result<BatchReply<T>, CacheError> {
        if !self.usable {
            return Err(CacheError::Storage {
                reason: "transport io",
            });
        }
        let bytes = encode_request(&self.auth, &request)?;
        let expected_op = request_op(&request);
        let expected_ids = request_ids(&request);
        // No request correlation exists at the envelope layer. After an
        // incomplete exchange, a late reply must never answer a later call.
        self.usable = false;
        write_envelope(&mut self.stream, 0, &bytes, self.max_frame)?;
        self.io_stats.rpc_body_bytes_sent = self
            .io_stats
            .rpc_body_bytes_sent
            .saturating_add(bytes.len() as u64);
        self.io_stats.framed_bytes_sent = self
            .io_stats
            .framed_bytes_sent
            .saturating_add(bytes.len() as u64 + 12);
        let envelope = read_envelope(&mut self.stream, self.pool.checkout()?, self.max_frame)?;
        self.io_stats.rpc_body_bytes_received = self
            .io_stats
            .rpc_body_bytes_received
            .saturating_add(envelope.body().len() as u64);
        self.io_stats.framed_bytes_received = self
            .io_stats
            .framed_bytes_received
            .saturating_add(envelope.body().len() as u64 + 12);
        match decode_reply(envelope.body())? {
            ReplyBody::Failed {
                op,
                store_epoch,
                error,
            } => {
                self.check_epoch(store_epoch)?;
                if op == expected_op {
                    self.clock.observe(envelope.server_now);
                    self.usable = true;
                } else if op != 0 {
                    return Err(CacheError::Malformed { reason: "reply op" });
                }
                // Op zero is a server-side decode failure with no reliable
                // request association. Preserve its error but keep the stream
                // poisoned so it cannot answer a subsequent request.
                Err(error)
            }
            other => {
                let batch = map(other)?;
                self.check_epoch(batch.store_epoch)?;
                if batch.items.len() != expected_ids.len() {
                    return Err(CacheError::Malformed {
                        reason: "reply batch length",
                    });
                }
                if batch
                    .items
                    .iter()
                    .zip(&expected_ids)
                    .any(|(item, expected)| item.request_id != *expected)
                {
                    return Err(CacheError::Malformed {
                        reason: "reply request id",
                    });
                }
                self.clock.observe(envelope.server_now);
                self.usable = true;
                Ok(batch)
            }
        }
    }

    fn check_epoch(&self, received: u64) -> Result<(), CacheError> {
        if received != self.epoch {
            return Err(CacheError::EpochMismatch {
                store: received,
                request: self.epoch,
            });
        }
        Ok(())
    }
}

fn request_ids(request: &RequestBody) -> Vec<RequestId> {
    match request {
        RequestBody::Lookup(items) => items.iter().map(|item| item.header.request_id).collect(),
        RequestBody::ReservePut(items) => items.iter().map(|item| item.header.request_id).collect(),
        RequestBody::Commit(items) => items.iter().map(|item| item.header.request_id).collect(),
        RequestBody::Get(items) => items.iter().map(|item| item.header.request_id).collect(),
        RequestBody::ReleaseCancel(items) => {
            items.iter().map(|item| item.header.request_id).collect()
        }
        RequestBody::Invalidate(items) => items.iter().map(|item| item.header.request_id).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpucache_core_types::{synthetic_identity, RequestId};
    use gpucache_protocol::{encode_hello_ack, encode_reply, ItemHeader, ItemResult};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn lookup_item(id: u128) -> LookupItem {
        LookupItem {
            header: ItemHeader {
                request_id: RequestId::from_counter(id).unwrap(),
                epoch: 1,
                deadline_ms: 1000,
            },
            identity: synthetic_identity(1, 1),
        }
    }

    fn lookup_reply(epoch: u64, ids: &[u128]) -> ReplyBody {
        ReplyBody::Lookup(BatchReply {
            store_epoch: epoch,
            items: ids
                .iter()
                .map(|id| ItemResult {
                    request_id: RequestId::from_counter(*id).unwrap(),
                    body: Ok(LookupOutcome::Miss {
                        reason: gpucache_protocol::MissReason::Absent,
                    }),
                })
                .collect(),
        })
    }

    fn fake_server(replies: Vec<Vec<u8>>) -> (TcpClient, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let pool = Arc::new(Pool::new(1));
            drop(read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap());
            write_envelope(&mut stream, 1, &encode_hello_ack(WIRE_VERSION, 1), 4096).unwrap();
            for reply in replies {
                drop(read_envelope(&mut stream, pool.checkout().unwrap(), 4096).unwrap());
                write_envelope(&mut stream, 2, &reply, 4096).unwrap();
            }
        });
        let identity = synthetic_identity(1, 1);
        let auth = AuthContext::new(identity.tenant, identity.trust_domain, [9; 32]).unwrap();
        let client = TcpClient::connect(addr, auth, Duration::from_secs(5), 4096).unwrap();
        (client, server)
    }

    fn assert_poisoned_reply(reply: Vec<u8>, ids: &[u128], expected: CacheError) {
        let (mut client, server) = fake_server(vec![reply]);
        let items = ids.iter().map(|id| lookup_item(*id)).collect();
        assert_eq!(client.lookup(items), Err(expected));
        assert!(!client.usable);
        assert_eq!(
            client.lookup(vec![lookup_item(99)]),
            Err(CacheError::Storage {
                reason: "transport io"
            })
        );
        server.join().unwrap();
    }

    #[test]
    fn wrong_request_id_poisons_the_connection() {
        assert_poisoned_reply(
            encode_reply(&lookup_reply(1, &[9])).unwrap(),
            &[1],
            CacheError::Malformed {
                reason: "reply request id",
            },
        );
    }

    #[test]
    fn reordered_batch_items_poison_the_connection() {
        assert_poisoned_reply(
            encode_reply(&lookup_reply(1, &[2, 1])).unwrap(),
            &[1, 2],
            CacheError::Malformed {
                reason: "reply request id",
            },
        );
    }

    #[test]
    fn missing_and_extra_batch_items_poison_the_connection() {
        for (reply_ids, request_ids) in [(vec![1], vec![1, 2]), (vec![1, 2], vec![1])] {
            assert_poisoned_reply(
                encode_reply(&lookup_reply(1, &reply_ids)).unwrap(),
                &request_ids,
                CacheError::Malformed {
                    reason: "reply batch length",
                },
            );
        }
    }

    #[test]
    fn truncated_reply_body_poisons_the_connection() {
        let mut reply = encode_reply(&lookup_reply(1, &[1])).unwrap();
        reply.pop();
        assert_poisoned_reply(
            reply,
            &[1],
            CacheError::Malformed {
                reason: "truncated buffer",
            },
        );
    }

    #[test]
    fn wrong_epoch_poisons_success_and_failed_replies() {
        let replies = [
            lookup_reply(2, &[1]),
            ReplyBody::Failed {
                op: gpucache_protocol::OP_LOOKUP,
                store_epoch: 2,
                error: CacheError::Unauthorized,
            },
        ];
        for reply in replies {
            assert_poisoned_reply(
                encode_reply(&reply).unwrap(),
                &[1],
                CacheError::EpochMismatch {
                    store: 2,
                    request: 1,
                },
            );
        }
    }

    #[test]
    fn wrong_operation_poisons_success_and_failed_replies() {
        let replies = [
            ReplyBody::Reserve(BatchReply {
                store_epoch: 1,
                items: Vec::new(),
            }),
            ReplyBody::Failed {
                op: gpucache_protocol::OP_GET,
                store_epoch: 1,
                error: CacheError::Unauthorized,
            },
        ];
        for reply in replies {
            assert_poisoned_reply(
                encode_reply(&reply).unwrap(),
                &[1],
                CacheError::Malformed { reason: "reply op" },
            );
        }
    }

    #[test]
    fn unassociated_server_decode_error_is_returned_without_reusing_the_stream() {
        assert_poisoned_reply(
            encode_reply(&ReplyBody::Failed {
                op: 0,
                store_epoch: 1,
                error: CacheError::Unauthorized,
            })
            .unwrap(),
            &[1],
            CacheError::Unauthorized,
        );
    }

    #[test]
    fn correlated_batch_and_item_errors_keep_the_stream_synchronized() {
        let failed = ReplyBody::Failed {
            op: gpucache_protocol::OP_LOOKUP,
            store_epoch: 1,
            error: CacheError::Unauthorized,
        };
        let item_error = ReplyBody::Lookup(BatchReply {
            store_epoch: 1,
            items: vec![ItemResult {
                request_id: RequestId::from_counter(2).unwrap(),
                body: Err(CacheError::TimedOut),
            }],
        });
        let replies = [failed, item_error, lookup_reply(1, &[3])]
            .iter()
            .map(|reply| encode_reply(reply).unwrap())
            .collect();
        let (mut client, server) = fake_server(replies);
        assert_eq!(
            client.lookup(vec![lookup_item(1)]),
            Err(CacheError::Unauthorized)
        );
        assert!(client.usable);
        assert_eq!(
            client.lookup(vec![lookup_item(2)]).unwrap().items[0].body,
            Err(CacheError::TimedOut)
        );
        assert!(client.usable);
        assert!(matches!(
            client.lookup(vec![lookup_item(3)]).unwrap().items[0].body,
            Ok(LookupOutcome::Miss { .. })
        ));
        server.join().unwrap();
    }

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

    #[test]
    fn io_counters_separate_complete_rpc_body_and_framed_bytes() {
        let (mut client, server) = fake_server(vec![encode_reply(&lookup_reply(1, &[1])).unwrap()]);
        let before = client.io_stats();
        assert_eq!(before.framed_bytes_sent - before.rpc_body_bytes_sent, 12);
        assert_eq!(
            before.framed_bytes_received - before.rpc_body_bytes_received,
            12
        );
        client.lookup(vec![lookup_item(1)]).unwrap();
        let after = client.io_stats();
        assert_eq!(after.framed_bytes_sent - after.rpc_body_bytes_sent, 24);
        assert_eq!(
            after.framed_bytes_received - after.rpc_body_bytes_received,
            24
        );
        assert!(after.rpc_body_bytes_sent > before.rpc_body_bytes_sent);
        assert!(after.rpc_body_bytes_received > before.rpc_body_bytes_received);
        server.join().unwrap();
    }
}
