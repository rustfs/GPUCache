# Security (V1 C, V3 CPU/TCP)

The library calls still require an `AuthContext`. The TCP service does too. It binds `127.0.0.1` unless the operator sets `--allow-remote`, and it refuses to start with an empty tenant policy. A data request with an unknown tenant, a mismatched trust domain, or a wrong fingerprint is `Unauthorized` and returns no payload. Hello carries versions and the store epoch only.

The fingerprint is derived from a credential, but the current protocol accepts that fingerprint directly: it is a bearer-equivalent secret. Capturing it on plaintext TCP permits impersonation. Do not pass a raw secret on the command line in a place that records process arguments if you can avoid it; V1 still takes the hex fingerprint as an argument because there is no secret store yet. An all-zero fingerprint is rejected before a request is built.

## What is enforced

- Every store call requires an `AuthContext`: tenant, trust domain, and a 32-byte credential fingerprint. The comparison examines all 32 fingerprint bytes. An all-zero fingerprint is rejected. Every hot TCP data operation also passes a mandatory server ACL compiled from these store policies before dispatch. A successful Hello does not authorize a later data frame.
- Empty policy configuration fails closed. Unknown tenants fail closed.
- An identity whose tenant or trust domain does not match the caller is `Unauthorized`. The store does not return that block's bytes.
- Physical keys include the tenant. There is no cross-tenant dedup and no list-all-keys API.
- Logs and errors do not embed payloads, prompts, or credentials. Checksums in errors are not included; the error is `ChecksumMismatch`.
- Only `COMMITTED`, checksum-valid, same-identity blocks are returned.
- Quarantine plus fencing is the rule for buffers whose completion is unknown. A timer is not treated as proof that a copy has stopped.

## V3 admission and library hooks

ACL and rate admission run on the TCP hot path alongside the store gate. The remaining hooks are explicitly in-process. None adds mTLS. See [adr/0007-trusted-fabric.md](adr/0007-trusted-fabric.md) and [adr/0010-hot-auth.md](adr/0010-hot-auth.md).

- `Acl` fails closed. `ServerConfig::acl` restricts the configured store policies; an empty ACL denies every data call. Lookup/Get and reader pin release require read access. ReservePut/Commit/Invalidate and writer cancellation require write access. A mixed pin-release/writer-cancel batch requires write before any mutation. Additional grants cannot override the store credential policy. The mandatory ACL has at most 1,024 principals.
- In-process `ReplayGuard` rejects repeated nonces inside a bounded window and stamps outside the configured skew. It rejects new admissions at capacity instead of evicting live nonces. The authorization helper scopes replay by tenant, trust domain and fingerprint; callers must use a server clock. Expired entries can be forgotten. This is not permanent replay protection and it is not wired into GCK1.
- `seal` / `open_seal` mix a key id and generation into a SHA-256. A mismatch is `ChecksumMismatch` and returns no bytes. This is not an HMAC. Anyone who can write the bytes can recompute it. It is an integrity hook for a trusted fabric, not a claim of cryptographic authenticity against a hostile store.
- In-process `SecretRing` rotates fingerprints. The previous fingerprint stays readable until `retire`. Writes use the current fingerprint only. Audit events store `Rotate`, `Retire`, `Deny`, and `Replay` plus a generation id. They do not store the fingerprint, the payload, or a prompt. Rotation history is bounded at 1,024 generations; capacity exhaustion and attempts to revive retired credentials fail closed.
- TCP `RateGate` atomically charges every batch item against a per-tenant budget shared by all connections. The default is 16,384 items per 1,000 ms, configurable in ServerConfig. Rejected credentials cannot spend the claimed tenant's budget. Active windows are never evicted to bypass the limit; at most 1,024 tenant windows exist. Refusal is `QuotaExceeded { RateLimit }`. ServerHandle exposes denial/rate counters and the bounded 256-entry ACL-denial audit.
- Authorized reader pin releases with `Drained` attestation bypass the rate budget so a Get that consumes the final credit can still release its pin. Tenant, region, pin-owner and storage idempotency limits still apply. A mixed batch containing writer cancellation does not receive this exemption.
- Control-plane loss on the simulated DPU queue hides slots and refuses new submits. It does not return a hit while membership is unknown.

## What is not enforced

- mTLS is not linked. The wire fingerprint is a bearer-equivalent secret on plaintext TCP. `--allow-remote` does not add encryption or cryptographic peer authentication. Treat a non-loopback bind as a trusted fabric, or do not bind it.
- Replay protection beyond the idempotency window and the bounded nonce window is not a permanent cache.
- `Attestation::Drained` is accepted from any caller who already passed the tenant gate. It is not a device measurement. A buggy client inside the tenant can reclaim its own in-flight buffer early.
- The idempotency table forgets entries after `idempotency_ttl_ms`. At capacity, new IDs are rejected until expiry rather than evicting a live entry. Reusing an ID after expiry can run the operation again.
- This tree does not encrypt KV bytes at rest. `MemoryStore` keeps them in process memory. `FileStore` writes them as ordinary files in the directory you name. That directory must not be a production disk or a device node. The open path rejects `/dev`, `/proc`, `/sys`, `/`, and `..`.
- TCP does not call ReplayGuard or SecretRing: GCK1 carries no authenticated nonce, issue time or credential generation. The sender-controlled envelope time is not trusted as proof of freshness. ACL configuration is fixed at startup. Versioned authentication/freshness and live rotation remain deferred; the existing request-ID/deadline contract is unchanged.
- `PrefixHit` bytes cover only `matched_prefix_blocks` of the query chain. A client that ignores that field and decodes them as the full query will mis-align. The server will not do that alignment for the client.

## Shutdown and restart

Dropping ServerHandle closes active sockets and joins their workers before returning. Reopen the FileStore only after this completes. The restart advances the persisted epoch; stale connections cannot keep writing through the old store instance. This does not provide a distributed leader election or a fence against independent processes opening the same directory.

## Fallback

Callers must treat every error and every miss as "recompute in the engine". They must not stitch a partial or quarantined block into a decode. V1 C checks that a wrong tenant, a wrong model, a corrupt file, a short write, and a failed disk write return no foreign payload. The synthetic check is exact token-id equality. See [CORRECTNESS.md](CORRECTNESS.md).
