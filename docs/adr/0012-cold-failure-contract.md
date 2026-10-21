# ADR 0012: Bounded cold failure handling and explicit retention

- Status: **Accepted**
- Date: 2026-10-20
- Milestone: V3 CPU contract completion

## Context

The G4 mock already binds v3 manifests to complete canonical identities, derives
publication keys from content, and caps pending plus failed sink jobs. A PUT
failure can mean that publication succeeded but its acknowledgement was lost.
Deleting its object would corrupt an already published manifest.

The remaining gaps were failure handling: actual backend read outages escaped as
errors even though predeclared Slow/Unavailable returned Missing; all storage
write errors were retried, including corruption; retention HEAD/DELETE errors
could discard scheduled work, and repeated delete-verification failure could
block the next key. Replacing a retention record also left an old expiration
queued. The optional HTTP probe treated binary response bodies as UTF-8 and
mapped authorization failures to transient storage errors.

## Decision

1. Keep GPMF manifest version 3 and existing digest keys unchanged. Versions 1,
   2, and unknown future versions are clean misses without payload reads. No
   CacheIdentity or GCK1/GCKR wire semantics change.
2. ObjectStore adapters classify timeouts as `TimedOut`, temporary backend I/O
   as `Storage` with reason `object store` or `object unavailable`, and temporary
   backpressure as `QuotaExceeded`. The missing-object label is `object missing`.
   Authorization, malformed input, corruption, and unknown storage labels are
   permanent write errors. Unknown errors therefore cannot silently retry.
3. At each refill read stage (HEAD, manifest GET, payload GET), missing objects
   and temporary failures return `Missing`. There is no inline retry or store
   mutation. Predeclared Slow/Unavailable performs no store calls. Validation
   and authorization failures remain distinguishable errors with no payload;
   upstream still recomputes. This synchronous API cannot interrupt a backend
   that never returns: adapters must enforce their own deadlines.
4. The sink attempts each job at most once per bounded drain round, rotating
   temporary failures to the tail and exhausting them after three total
   attempts. Permanent errors go directly to the failed queue. Pending and
   failed jobs share the configured byte/job limits and the 4096-job ceiling.
   Retrieval or explicit cancellation releases host credits only.
5. **Orphan garbage collection is not automatic.** Neither sink drain,
   `take_failed`, cancellation, drop, nor ColdOps scans for orphan objects or
   deletes one because PUT failed. ColdOps only acts on records explicitly
   supplied by a trusted caller and explicitly scheduled for expiration. It is
   a retention primitive, not a proof that an object is unreferenced.
6. Retention errors preserve the record and rotate its queued key to the tail.
   A checksum mismatch refuses DELETE. Renewing a record cancels any queued
   expiration for that key; the renewed timestamp must expire and be scheduled
   again. Only a confirmed missing object completes reclaim.
7. Cold key metadata has hard ceilings of 1024 tenants and 4096 generation
   records, including retired generations. Rotation checks history capacity and
   generation overflow before demoting the current key. Retention holds at most
   4096 object records and at most one queued reclaim per record. Constructors
   permit lower limits. Replacing a record at capacity remains allowed; a failed
   reclaim retains its admission credit. Saturation returns `QuotaExceeded`
   using the existing `InFlightRequests` metadata label, without a wire change.
8. The optional HTTP smoke adapter preserves permanent status errors, parses
   binary bodies without UTF-8 conversion, and rejects truncated, overlong,
   ambiguous-length, and unsupported chunked success responses. These are CPU
   parser checks, not evidence of live RustFS compatibility.

## Operator cleanup policy

A failed publication may retain an orphan indefinitely. Preserve the failed job
or an external inventory before releasing its host credits. After producer
quiescence and an operator-chosen grace period, an external authority must check
ownership and every manifest reference, including in-flight publications, before
a delete is authorized. This crate has no reference index, bucket scanner,
crash-persistent inventory, or automatic orphan collector. A negative HEAD alone
is not a proof of safe deletion. Do not register unknown orphan keys in ColdOps
as a shortcut around those checks.

ColdOps records and cold key-rotation history remain in-memory metadata despite
these hard ceilings. Retired key history is never silently evicted to create
space: capacity exhaustion requires operator action and must not revive old
credentials. The bounds are not a claim of bounded overall process RSS or a
crash-safe inventory. A future integrated collector needs its own ADR and
reference-safe protocol.

## Evidence and limitations

`cargo test -p gpucache-cold --all-targets` covers:

- v1/v2 misses, complete identity mismatch, wrong-tenant rejection, content-key
  substitution, and combined manifest/payload budgets (`refill_safety.rs`);
- every refill entry point and read stage under actual timeout, unavailable,
  missing, and backpressure errors, plus no-call degraded wrappers
  (`failure_contract.rs`);
- permanent/transient classification, fair drain, retry exhaustion, retained
  failed-job credits, and a saturated-queue storm that performs no automatic
  orphan collection (`sink_tests.rs`);
- retention HEAD, DELETE, and verification faults, fair progress, unexpected
  content preservation, renewed-record expiration, metadata capacity failure
  atomicity, and key-generation overflow (`reclaim_tests.rs`);
- HTTP status classification and binary/framing parser failures (`http.rs`).

Mock ObjectStore remains the default. Live RustFS, GDS, RDMA and BF3 remain
`NOT_RUN_HARDWARE`. No credentials, hardware, performance results, or new
external dependencies were introduced. The cold crate stays off the hot path.
