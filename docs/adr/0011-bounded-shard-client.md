# ADR 0011: Bounded TCP shard sessions and explicit retry contracts

- Status: **Accepted for the CPU/TCP contract**
- Date: 2026-10-20
- Milestone: V3

## Context

`TcpClient` already checked reply operation, ordered request IDs, and store epoch,
and poisoned an incomplete stream. It had no shared connection pool or shard
placement. The directory bounded active fills and notices but exposed membership
changes one at a time. The V3 control plane's membership was not configurable
through a public batch operation. A distributed client must not turn an ambiguous
write into a write on another owner, reuse a poisoned stream, or duplicate a
reader pin after losing a Get reply.

## Decision

The GCK1/GCKR bytes and `CacheIdentity` remain unchanged. `TcpClient` remains
available. `ShardedClient` adds a tenant-scoped CPU/TCP connection pool and
rendezvous routing over the validated full physical identity. Endpoint order and
address changes do not change the placement hash. `route(identity)` checks the
client's tenant and trust domain before opening a connection and returns a
`ShardSession` with the same six RPC methods as `TcpClient`.

Generations remain local to a shard. Retain `session.shard_id()` with a generation
and use `shard(shard_id)` to reopen that owner. Do not hash generation numbers or
move a Commit/Release/Invalidate to another shard: independent stores can both
allocate generation 1. Identity-bearing batches must belong to one session's
shard. Cross-shard batches fail before sending; callers partition them explicitly.
No generation bits, identity fields, or wire version are repurposed for routing.

### Admission and retries

- `ClientLimits` bounds shards, global and per-shard sockets, retry metadata,
  frame size, request lifetime, and total attempts. Upper validation caps are
  4,096 shards/sockets, 1,048,576 retry entries, 32 MiB per frame, four attempts,
  and 60 seconds request lifetime. Deployment defaults are smaller. There is no
  waiting or retry queue: exhaustion returns a quota error for recomputation.
- Active and idle sockets share the connection ceiling. Checkout can close an
  idle socket on another shard so it does not monopolize the global ceiling.
  Poisoned sockets are closed. Dropping a healthy lease returns its socket.
- The retry table retains only checksums and bounded request metadata, not KV
  payloads or replies. Admission validates the entire batch before inserting any
  entry. Duplicate request IDs, a changed batch fingerprint, or a concurrently
  active same-ID request fail. Live entries are not evicted to admit new work.
  Entries expire at the original deadline; active entries survive until their
  call finishes. Request IDs must remain unique across new logical requests.
- Every retry keeps the exact batch, authentication, request IDs, epochs, and
  deadlines. Retry only follows an ambiguous I/O/timeout result; item-level
  failures and epoch/identity/authorization failures are returned unchanged.
  The original call and automatic or explicit replays share the attempt ceiling.
- Lookup can retry because it does not create a pin or mutation. **Get never
  retries**, including when mutation retries are enabled. A lost Get reply means
  recompute and later explicit pin drain according to the storage lease contract.
- Mutation retries default to disabled. Opt-in requires
  `mutation_retry_retention_ms` to declare the deployment's minimum server
  idempotency retention, and it must be strictly greater than the client's
  maximum request lifetime. Operators must configure every server to retain
  idempotency entries for at least that declared duration. GCK1 does not negotiate
  this setting. If this contract is unknown, keep mutation retries disabled.
  Reconnecting checks the expected epoch before resending; restart cannot replay
  an old mutation into a new store.

### Membership and fences

`ShardDirectory::apply_membership` and `ControlPlane::apply_membership` apply a
bounded complete member snapshot. Duplicate IDs, zero/stale incarnations, empty
membership, capacity exhaustion, and epoch exhaustion fail before changing
membership, fault domains, credits, or notices. Reordering an unchanged set is a
no-op. A valid changed snapshot fences once. There is no membership update queue
in which a rejected update can block later independent snapshots. This is a
synchronous admission rule, not a distributed scheduler fairness guarantee.

`ShardedClient::replace_routes` checks the expected local revision and preflights
the full snapshot. Every new endpoint must have an epoch above the highest epoch
in the previous snapshot, including newly added and re-added shard IDs. This
coordinated replacement rule keeps a scalar epoch high-water mark instead of an
unbounded retired-node history. It closes idle sockets, retires old sessions,
and suppresses replies that arrive after publication. Checked-out old leases
remain charged until dropped.

**Publishing a client route does not fence a remote server.** The caller must
stop/drain old ownership and advance every destination server epoch before
publishing. The client refuses a handshake from an unexpected epoch, but the
unchanged wire format cannot prove that a disconnected old server has stopped
serving other clients. This tree has no distributed quorum, authenticated
membership distribution, or cross-host ownership authority. A local directory
fence is not a remote data-plane fence.

## CPU restart and rebalance checklist

1. Stop admitting fills on the old shard; retain generation/epoch/shard scope.
2. Drain known completions. Keep unknown completions quarantined and charged.
3. Stop the old TCP service and wait for its workers to exit before reopening
   the same file store. Reopening advances the persisted store epoch and drops
   uncommitted state. Coordinate independent hosts externally.
4. Start and verify all snapshot endpoints at epochs above the previous maximum.
   Do not publish a snapshot if any endpoint or directory fence fails.
5. Apply directory/control-plane membership snapshots and publish client routes
   with the expected revision. Drop retired sessions. Never rewrite an old
   request's epoch or retry it on the new owner.
6. Treat absent uncommitted blocks, unavailable shards, rejected old generations,
   and any unverified completion as recompute. Committed file blocks may remain
   readable under the new epoch; they are not durability replicas.

## Observability

`ClientStats` exposes active-plus-idle connection count, idle connections, retry
entries, reuse/retry counters, route revision, and suppressed replies.
`DirectoryStats` reports flights, request entries, notices, charged tenants,
members, fences, and fenced fills. `RegisteredPool::stats` reports quarantined
slots, held bytes, fences, drained slots, and reclaimed slots. Lease expiry never
increments the drained/reclaimed counters.

`TcpClient::io_stats` separates complete RPC body bytes from complete framed
bytes, including hello. Each complete application envelope adds 12 framing bytes.
These are application counters: TCP/IP headers, kernel retransmissions, and
partial failed I/O are excluded. `TransferDesc::bytes.len()` and TCP staging receipts
continue to describe submitted/staged payload size, not measured NIC
wire traffic. None of these values is a bandwidth benchmark.

## Enforced limitations and evidence

| Contract | CPU regression |
| --- | --- |
| A longer prefix routed away from a stored shorter prefix is a clean miss with no fanout | `prefix_on_another_shard_is_a_clean_miss_without_global_fanout` |
| Two servers can both allocate generation 1 without routing a wrong identity or tenant hit | `two_shards_keep_generation_scope_tenant_identity_and_pool_reuse` |
| Global socket capacity cannot be monopolized by idle connections on another shard | `idle_connection_on_one_shard_does_not_starve_another_shard` |
| Socket/retry tables stay bounded under a cancel storm; no hidden queue | `checked_out_connections_and_cancel_retry_entries_have_hard_bounds` |
| Retry batch admission has no partial metadata mutation; Get cannot be replayed | `retry_admission_is_atomic_and_get_is_never_replayed` |
| Lost mutation acknowledgement replays identical bytes with one allocation | `ambiguous_put_retries_identical_bytes_once_without_duplicate_allocation` |
| Get and default mutations do not retry an ambiguous loss | `default_mutations_and_all_gets_never_retry_after_lost_reply` |
| Reconnect to a new epoch never sends an old mutation | `retry_reconnect_epoch_change_never_sends_the_old_mutation` |
| Late replies cannot cross a route revision | `late_reply_after_route_publication_is_suppressed_and_socket_closed` |
| Re-added shard cannot reset the epoch high-water mark | `removing_then_readding_shard_cannot_reset_epoch_high_water_mark` |
| Concurrent Lookup/Get/Put racing two FileStore restarts, plus stale pending Commit, fail safely | `two_shard_restart_fences_concurrent_lookup_get_and_put_and_inflight_reservation` |
| Invalid membership batches leave notices, credits and members untouched | `membership_batch_rejects_every_invalid_snapshot_before_fencing` |
| Cancellation/restart storms keep directory metadata bounded | `cancel_restart_and_membership_storm_keep_every_table_bounded` |
| Registered cancellation requires explicit drain/fence and reports bounded held bytes | `cancellation_storm_requires_drain_and_reports_fence_reclaim_counts` |

Whole-block prefix lookup remains correct within a shard. Full-identity routing
can place a longer prefix on a different shard, so discovery of shorter prefixes
across shards is deferred and returns a miss; no cross-shard search or intra-block
slicing is added. Generations are not globally unique. Independently incremental
route replacement is deferred in favor of the coordinated snapshot fence rule.
Client restart does not recover a retry ledger; only the server's current-epoch
idempotency table can recognize a still-valid exact request. No retry survives a
server epoch change. Error callers must recompute; I/O errors are not fabricated
as cache hits or successful mutations.

Run the complete CPU reproduction with
`python3 scripts/ci/vm_regression.py --profile cpu --offline --output artifacts/astra-full-cpu`.
The runner discovers these tests through workspace tests. Live RDMA, NIC/DPU
completion, GDS, vLLM KV import/export and multi-host partitions remain
`NOT_RUN_HARDWARE`. This is CPU/TCP fault evidence, not PB-scale verification.
