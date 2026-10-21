# ADR 0010: Enforce ACL and bounded admission on the hot TCP path

- Status: **Accepted**
- Date: 2026-10-20
- Milestone: V3 CPU/TCP

## Context

The FileStore already checked tenant, trust domain, fingerprint and the identity
of each requested block. The V3 ACL and rate gate existed only as library hooks.
A TCP deployment could not restrict a configured tenant to read access, and
independent client connections did not share an admission window.

GCK1 has no authenticated nonce, secret generation, or issued-time field.
The envelope timestamp is supplied by an untrusted sender. Treating it as proof
of freshness would make replay protection misleading and would break legitimate
idempotent retries. The wire and CacheIdentity semantics must remain unchanged.

## Decision

Every decoded TCP data request passes a server-owned ACL before any store call.
The mandatory ACL is compiled from the FileStore policies, with a maximum of
1,024 entries. `ServerConfig::acl` optionally restricts these grants. `None`
preserves read/write access for the configured store principals; an explicit
empty ACL denies all data calls. Additional ACL entries cannot authorize a
credential absent from the store policies.

Lookup and Get require read access. ReservePut, Commit and Invalidate require
write access. ReleaseCancel requires write if **any** item cancels a writer
(`pin_owner` absent). A batch of reader pin releases requires read, so read-only
clients can drain their own references. The storage tenant and pin-owner checks
still apply. Denying a mixed batch happens before releasing any pin.

After authorization, each batch atomically charges its item count against a
server-owned per-tenant RateGate. Connections share this budget. Defaults are
16,384 items per 1,000 ms, configurable through ServerConfig. A failed batch
admission does not partially consume the budget. Unauthorized credentials cannot
spend the claimed tenant's budget. Tables are limited to 1,024 tenants; active
windows are never evicted to admit another tenant. Expired windows release table
capacity. An oversized first batch does not occupy a tenant window. Authorized
reader pin releases with `Drained` attestation bypass this admission budget, so a
Get that uses the final credit can still release its reference. Storage still
checks the tenant, region and pin owner and bounds its idempotency table.
Time comes from the server's monotonic clock.

ACL denials increment a saturating counter and a 256-entry audit ring.
Rate-limit refusals increment a separate counter. ServerHandle exposes snapshots;
they contain no credentials, payloads or identities. These are admission counters,
not a claim to count every per-item storage error.

Server shutdown closes the bounded set of active TCP sockets and joins every
worker before returning. An old worker cannot continue modifying the FileStore
after its directory is reopened by the supported stop-then-start sequence.

## Reader and reply admission

`MemoryStore` now applies `max_in_flight_requests` as an independent global
reader-pin ceiling as well as the existing writer ceiling. Expired or unknown
completion pins remain charged. Each Get batch may copy at most
`max_object_bytes` total payload bytes, checked before allocation/pin admission.
Later over-budget items receive existing quota errors, preserving item-scoped
batch semantics. `StoreStats::reader_pins` exposes the charged count. No new
wire tag, identity field or configuration schema is introduced. Direct fenced
reclaim authorizes before running timer maintenance.

TCP additionally passes its `max_frame_bytes` reply-body ceiling to the store
before Get admission. The protocol-owned budget reserves the exact 20-byte GCKR
batch header and 85 metadata bytes per possible successful item, then allows
payloads only within the remaining bytes and the codec's absolute field limit.
Every item error is smaller than a successful item's metadata reservation;
failed items conservatively keep that reservation for the rest of the batch.
If metadata alone does not fit, the batch fails with the existing quota error,
whose 21-byte failed reply fits even the minimum server frame limit. Otherwise
over-budget items fail without copying payloads or creating pins. Successfully
admitted items and their pin handles always fit the configured frame before
encoding begins. The outer TCP envelope is separate from this body limit.
Network failures after admission still retain unknown-completion pins under
the existing drain/fence contract.

## Wire compatibility and deferred authentication

GCK1 requests, GCKR replies and CacheIdentity are unchanged. Hello remains an
unauthenticated version/epoch exchange with no data access. A data frame still
requires its own complete AuthContext; a successful Hello does not authenticate
the connection.

The bearer-equivalent fingerprint travels on plaintext TCP. This is authorization
on a trusted fabric, **not mTLS** or cryptographic peer authentication. The ACL
is fixed at server start. The SecretRing and ReplayGuard remain in-process hooks;
neither is advertised as enforced by TCP. Request IDs and deadlines retain the
existing bounded idempotency contract, including replay after expiry. Adding
authenticated freshness or live credential rotation needs a separate versioned
protocol/authentication design. SHA-256 integrity seals are not transport MACs.

## Evidence and limits

`crates/transport/tests/hot_security.rs` exercises all six operations on actual
loopback TCP: empty-ACL denial, read-only denial and pin drain, conflicting ACL
grants versus store policies, cross-tenant generations, cross-model/layout
reads, batch rate limits across connections, a bounded denial audit, and restart
with an old connection still open. Unit tests cover the bounded rate table and
credential-safe ACL debugging.

`crates/transport/tests/reply_budget.rs` uses 2 KiB frames and one/two-pin ceilings
to cover oversized Gets, exact-size single and multi-item replies, mixed success
and quota errors, pin drain, and subsequent requests on the same TCP connection.
Storage tests assert zero pins after metadata/payload rejection. Codec tests
check the budget against actual GCKR encodings and the absolute field cap.

These tests use synthetic CPU payloads. They do not qualify encryption, hardware
DMA drain, a distributed control-plane authority or any GPU/DPU transport.
Hardware paths remain `NOT_RUN_HARDWARE`.
