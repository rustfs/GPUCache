# Scaling the CPU/TCP contract from 1 to 3 to 10 services

GPUCache currently provides independent file-backed TCP services and a Rust
`ShardedClient`. A caller supplies a complete routing snapshot and coordinates
ownership changes. Starting more services does not automatically form a cluster.
There is no `--join`, seed discovery, membership daemon, replicated metadata
service, remote fence RPC, payload migration, or durability replica placement.

The executable example below verifies 1 → 3 → 10 services on one host. It uses
one process, separate loopback listeners and separate store directories. It is
CPU correctness evidence, not a multi-host deployment qualification, an inference
engine integration, a throughput result, or a production-readiness claim.

## Run the executable example

From a checkout containing
[`scaling_smoke.rs`](../crates/transport/examples/scaling_smoke.rs):

```bash
cargo run -p gpucache-transport --example scaling_smoke --offline
cargo test -p gpucache-transport --example scaling_smoke --offline
```

`--offline` requires the locked dependencies to be cached already. Omit that flag
for the initial dependency download if necessary. The example's positive test is
also executed by `cargo test --workspace --all-targets`.

The example creates an exclusive temporary root, starts independent `FileStore`
services on OS-selected loopback ports, and removes its temporary data afterward.
Its public test credential and synthetic 32-byte blocks are for this local example
only. It does not connect to a GPU, DPU, inference engine, or RustFS endpoint.

Each phase checks every shard using a full physical `CacheIdentity`:

1. Lookup returns a miss for a new identity.
2. Reserve/Put accepts the payload; Commit publishes its returned generation.
3. Lookup returns that generation, length, and checksum.
4. Get returns the exact payload for that identity.
5. Release/Cancel supplies the Get request's pin owner and `Attestation::Drained`;
   the block remains committed and is not quarantined.

At each transition the example retains one old session and an uncommitted
reservation, completes all Get lifecycles, and stops and joins every old server.
It opens the surviving and new stores at the next common epoch, replaces the
complete client snapshot, and checks these outcomes:

- The old session's Lookup and pending Commit are rejected by its route revision.
- A direct request carrying the previous store epoch is rejected by the restarted
  server, independently of the client's local route fence.
- The pending block is absent from its original restarted store and its current
  routed owner.
- A committed identity whose owner changes remains on its original store but is
  initially absent on its new owner. Explicit simulated recomputation populates
  the new owner. No payload migration took place.

The output includes a line for every verified shard, for example:

```text
status=SCALING_SMOKE_OK
topology=SINGLE_PROCESS_LOCALHOST
stage_shards=1 store_epoch=1 local_revision=1
shard=0 epoch=1 lookup_put_commit_get_drained=OK
stage_shards=3 store_epoch=2 local_revision=2
transition_to_shards=3 stale_session=REJECTED stale_commit=REJECTED old_epoch=REJECTED uncommitted=MISS remapped=MISS_THEN_RECOMPUTE original_copy=RETAINED
shard=0 epoch=2 lookup_put_commit_get_drained=OK
shard=1 epoch=2 lookup_put_commit_get_drained=OK
shard=2 epoch=2 lookup_put_commit_get_drained=OK
stage_shards=10 store_epoch=3 local_revision=3
transition_to_shards=10 stale_session=REJECTED stale_commit=REJECTED old_epoch=REJECTED uncommitted=MISS remapped=MISS_THEN_RECOMPUTE original_copy=RETAINED
```

Ten further `shard=0` through `shard=9` success lines follow at epoch 3. The final
summary is:

```text
shard_counts=1,3,10
verified_store_epochs=1,2,3
per_shard_roundtrips=14
transition_fences=2
stale_sessions=REJECTED
inflight_puts=FENCED_MISS
remapped_blocks=MISS_THEN_RECOMPUTE
automatic_payload_migration=NONE
durability_replication=NONE
engine_blocks=SYNTHETIC
hardware=NOT_RUN_HARDWARE
performance=NOT_A_PERFORMANCE_RESULT
```

The 14 roundtrips count the one-per-shard checks across the three phases. The
transition checks perform additional operations. The output does not report a
performance measurement.

## Four different identifiers

| Value | Meaning and owner |
| --- | --- |
| `ShardEndpoint::shard_id` | A stable placement ID assigned by the caller. A server does not advertise this ID in the current wire handshake. Keep its mapping to a store under operator control. |
| `ShardEndpoint::epoch` | The expected actual epoch of that endpoint's `FileStore`. The TCP handshake checks it before using a routed connection. |
| `ClientStats::revision` | A local client's route snapshot revision. It starts at 1 and advances on replacement. It is not a global consensus term or a store epoch. |
| Generation | A block identifier local to one shard and epoch. Two stores can both allocate generation 1. Preserve shard and epoch with the generation. |

`ShardDirectory` and `ControlPlane` are additional in-process APIs. Their member
incarnations and directory epochs do not configure a remote `FileStore`, and
`gpucache-tcp serve` does not instantiate a membership coordinator.

The full physical identity includes tenant, trust domain, model and layout
compatibility, plus block/prefix identity. Rendezvous placement uses this identity
and stable shard IDs; endpoint order and address changes do not change the hash.
An added shard moves some identities to new owners. There is no promise that
exactly one third or one tenth of a small workload lands on each shard.

Keep `(shard_id, epoch, generation)` with outstanding work. Reopen the original
owner with `client.shard(shard_id)` only while that epoch remains valid. Never
route a Commit, Release, or Invalidate by hashing a generation number. Partition
identity-bearing batches by shard before submission. Request IDs must remain
unique for new logical requests; do not rewrite an old request's epoch to make it
pass a fence.

## Plan the service addresses and ownership

These are example address assignments, not discovery configuration:

| Phase | Stable IDs | Separate hosts on a private network | Same-host fixed-port alternative |
| --- | --- | --- | --- |
| 1 service | `0` | `10.0.0.11:9735` | `127.0.0.1:9735` |
| 3 services | `0..2` | `10.0.0.11:9735` through `10.0.0.13:9735` | ports `9735..9737` |
| 10 services | `0..9` | `10.0.0.11:9735` through `10.0.0.20:9735` | ports `9735..9744` |

Each service owns a distinct data directory. One host may run several services,
but those services do not create independent host failure domains. The runnable
example uses ephemeral ports rather than these fixed ones.

The existing standalone CLI can start a file-backed TCP service:

```bash
cargo run -p gpucache-transport --bin gpucache-tcp --offline -- serve \
  --data-dir ./work/shard-0 \
  --tenant example-tenant --trust example-domain \
  --fingerprint "$GPUCACHE_FINGERPRINT_HEX" \
  --bind 127.0.0.1:9735
```

`GPUCACHE_FINGERPRINT_HEX` must contain the caller-provisioned 64-hex credential;
the CLI does not generate or distribute it. Use a protected trusted network and
`--allow-remote` for a non-loopback bind. The fingerprint is bearer-equivalent
and is not a TLS client certificate. This transport does not provide TLS or
mTLS; see [ADR 0007](adr/0007-trusted-fabric.md).

The CLI has no shard ID, expected epoch, resource-limit, or membership flags.
Launching three CLI processes alone is not the 1 → 3 transition. The caller must
supply real addresses and verified store epochs to `ShardedClient::new`, retain
the client for pool reuse, then use `route(&identity)` to obtain a session. The
example's `start_servers`, `prepare_store`, and `run_scaling_smoke` functions are
the tested library bootstrap and orchestration reference.

## Perform a coordinated transition

The currently supported route replacement requires a coordinated pause. It does
not implement a zero-downtime rolling expansion. Use the same sequence for 1 → 3
and 3 → 10; removing or re-adding a shard also follows the snapshot rule.

1. **Pause admission in every old client**, including Lookup/Get and writes.
   Stopping writes alone leaves read pins and active operations. Retain original
   shard/epoch/generation ownership for outstanding requests.
2. Complete or cancel known operations and finish every known Get drain/release
   lifecycle. Unknown completion remains quarantined and charged; lease expiry
   is not evidence of drain. The synthetic example drains every Get explicitly.
3. Stop all old service owners and wait for them and their connection workers to
   exit. The library example drops every `ServerHandle`, whose destructor stops
   and joins its workers. With independently managed hosts, the operator must
   provide the equivalent stop-and-wait barrier. A disconnected host is not
   proven stopped. Do not reopen a directory or transfer ownership while an old
   owner may still serve it.
4. Exclusively open each surviving and new store and verify its actual epoch.
   Choose a target greater than the highest epoch in the old snapshot. **Every
   endpoint in the new snapshot must exceed that old maximum**, including newly
   added endpoints. Do not assume a newly created directory has the right epoch.
5. Start the new service set using those verified store instances. If using local
   directory/control-plane membership, validate and apply its complete bounded
   snapshot while external admission is still paused. `ControlPlane` must be
   `Serving`: its `apply_membership` rejects the software-upgrade `Draining`
   state. This API is not a remote fence.
6. Distribute the same complete endpoint snapshot to all callers, with authenticated
   distribution supplied by the deployment. For each existing client call
   `replace_routes(client.stats().revision, endpoints)` and check success. Each
   client has its own expected revision; distribution is not atomic across
   processes. Resume only after the required callers acknowledge the new view.
7. Drop retired sessions and recompute any absent, failed, or ambiguously completed
   block as new work. A new owner can miss even when the previous store still
   contains committed bytes. Repopulate explicitly and resume admission.

`replace_routes` closes idle connections and retires old checked-out sessions;
it also suppresses replies that arrive after publication. Those checked-out
connections remain charged until dropped. Publishing routes by itself cannot
stop a remote old server or fence another client's stale view.

### How the example reaches epochs 1, 2, and 3

`FileStore::open` creates a fresh directory at epoch 1. Every later exclusive open
of that store increments its persisted manifest epoch. There is no public
`FileStore::set_epoch` API or CLI `--epoch` flag.

| Target snapshot | Surviving stores | Fresh stores |
| --- | --- | --- |
| 1 service, epoch 1 | none | Open shard 0 once. |
| 3 services, epoch 2 | After stop/join, reopen shard 0 once. | Open, close, and reopen shards 1 and 2. |
| 10 services, epoch 3 | After stop/join, reopen shards 0 through 2 once. | Open and close twice, then open shards 3 through 9 a third time. |

The example's preparation helper is bounded to three opens and targets 1 through
3. It returns the final open store directly to `serve`, so starting the service
does not open it again. A CLI invocation opens its own store and therefore would
increment an existing epoch again. Never edit `MANIFEST`, delete an old root to
reset its epoch, or run overlapping owners to bypass the rule. The caller must
guarantee exclusive directory ownership during preparation.

This helper is deliberately limited to a fresh disposable demonstration. An
existing deployment at epochs 7, 7, and 9 needs every replacement endpoint above
9, not simply epoch 3 because it has ten services. The repository does not yet
provide a general fleet epoch-administration tool. Any failure or unexpected
epoch requires stopping the transition and reconciling the actual store states
before publishing a snapshot; do not silently lower the target.

## Limits and expected misses

Client defaults allow 64 shards, 64 total connections, and 4 connections per
shard. A server defaults to 16 accepted connections across all clients. Multiple
client processes each have their own pool, so increasing the shard count does
not establish a capacity plan. Configure the library's `ClientLimits`,
`ServerConfig`, and store `ResourceLimits` for the actual workload and handle
quota rejection with recomputation. A 10-service smoke test is not evidence for
10 physical hosts or PB scale.

Mutation retries remain disabled by default. Enabling them requires an explicit
server idempotency-retention contract longer than the maximum client request
lifetime. Get is never replayed automatically because it creates a pin. Errors,
timeouts, and epoch failures are not hits. See [ADR 0011](adr/0011-bounded-shard-client.md)
for the bounded pool, replay, and membership contracts.

Whole-block prefix reuse is shard-local. A longer identity can route away from a
stored shorter prefix; that lookup is a clean miss. There is no cross-shard prefix
search or intra-block slicing. Membership changes do not copy blocks. Old local
copies may remain until eviction, but no replica protocol maintains them and
no durability promise follows. A lost hot block is recomputed; cold durability
belongs to RustFS G4, outside this hot-path example.
