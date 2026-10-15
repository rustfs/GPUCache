# Security (V1 C)

The library calls still require an `AuthContext`. The TCP service does too. It binds `127.0.0.1` unless the operator sets `--allow-remote`, and it refuses to start with an empty tenant policy. A data request with an unknown tenant, a mismatched trust domain, or a wrong fingerprint is `Unauthorized` and returns no payload. Hello carries versions and the store epoch only.

The fingerprint is a hash of a credential, never the credential. Do not pass a raw secret on the command line in a place that records process arguments if you can avoid it; V1 still takes the hex fingerprint as an argument because there is no secret store yet. An all-zero fingerprint is rejected before a request is built.

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
- This tree does not encrypt KV bytes at rest. `MemoryStore` keeps them in process memory. `FileStore` writes them as ordinary files in the directory you name. That directory must not be a production disk or a device node. The open path rejects `/dev`, `/proc`, `/sys`, `/`, and `..`.
- TCP has no mTLS and no replay window beyond idempotency. Do not expose it past a trusted host. `--allow-remote` does not add authentication; the fingerprint check is still the only gate.
- `PrefixHit` bytes cover only `matched_prefix_blocks` of the query chain. A client that ignores that field and decodes them as the full query will mis-align. The server will not do that alignment for the client.

## Fallback

Callers must treat every error and every miss as "recompute in the engine". They must not stitch a partial or quarantined block into a decode. V1 C checks that a wrong tenant, a wrong model, a corrupt file, a short write, and a failed disk write return no foreign payload. The synthetic check is exact token-id equality. See [CORRECTNESS.md](CORRECTNESS.md).
