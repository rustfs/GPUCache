# Security (V1 A)

V1 A is a library. It does not listen on a network. Binding it to a public address without authentication would violate the product rule; nothing in this milestone opens a socket.

## What is enforced

- Every store call requires an `AuthContext`: tenant, trust domain, and a 32-byte credential fingerprint. The fingerprint is compared in constant time. An all-zero fingerprint is rejected.
- Empty policy configuration fails closed. Unknown tenants fail closed.
- An identity whose tenant or trust domain does not match the caller is `Unauthorized`. The store does not return that block's bytes.
- Physical keys include the tenant. There is no cross-tenant dedup and no list-all-keys API.
- Logs and errors do not embed payloads, prompts, or credentials. Checksums in errors are not included; the error is `ChecksumMismatch`.
- Only `COMMITTED`, checksum-valid, same-identity blocks are returned.
- Quarantine plus fencing is the rule for buffers whose completion is unknown. A timer is not treated as proof that a copy has stopped.

## What is not enforced

- mTLS, replay protection beyond the idempotency window, secret rotation, and audit shipping are later work (the product note puts production hardening in V3; V1 still requires this minimum gate before any network exposure).
- `Attestation::Drained` is accepted from any caller who already passed the tenant gate. It is not a device measurement. A buggy client inside the tenant can reclaim its own in-flight buffer early.
- The idempotency table forgets entries after `idempotency_ttl_ms` or when it is full (new ids are then rejected until expiry). Reusing an id after expiry can run the operation again.
- This crate does not encrypt KV bytes at rest. MemoryStore keeps them in process memory.

## Fallback

Callers must treat every error and every miss as "recompute in the engine". They must not stitch a partial or quarantined block into a decode.
