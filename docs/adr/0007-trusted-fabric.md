# ADR 0007: Trusted fabric, not a linked mTLS stack

- Status: **Accepted**
- Date: 2026-10-15
- Milestone: V3

## Context

V1 already requires a tenant, a trust domain, and a credential fingerprint on every data call. The TCP listener binds loopback unless the operator sets `--allow-remote`. V3 asks for tenant authorization, mTLS or an explicit trusted-fabric boundary, integrity, anti-replay, quota and rate limits, noisy-neighbor tests, secret rotation, and audit events.

Linking a TLS stack (for example rustls) would add a new dependency and would still not be a measured mutual-auth deployment. This tree has no certificate authority, no production network, and no operator policy for which peers are allowed. Pretending the socket is mTLS would be a false claim.

## Decision

The V3 boundary is a **trusted fabric**, documented here, plus CPU hooks. mTLS is not linked.

Operators:

- Leave the listener on `127.0.0.1` unless the host network is already restricted to cache peers.
- `--allow-remote` does not add authentication and does not start TLS. The fingerprint and the V3 ACL still apply.
- Do not place this TCP service on an open network and call that mTLS.

In-process hooks, all caller-clocked and bounded:

- `Acl` maps a fingerprint to read or write. An empty ACL fails closed. A read grant is not a write grant. The fingerprint compare scans every entry.
- `ReplayGuard` accepts a nonce once inside a caller-supplied skew. The window is bounded. Evicting the oldest nonce means that nonce can appear again later. This is not an infinite replay cache.
- `seal` / `open_seal` bind a key id and a generation into a SHA-256 of a length-delimited buffer. A mismatch is `ChecksumMismatch` and returns no bytes. This is not an HMAC and not a transport MAC. On a trusted fabric it catches accidental corruption. It does not stop a caller who can recompute SHA-256.
- `RateGate` limits admits per tenant per window. A noisy tenant spends only its own budget.
- `SecretRing::rotate` moves the previous fingerprint to overlap (read, not write) and audits the generation id. `retire` applies to overlap only. The audit log stores event kinds and generation ids, not fingerprints, payloads, or prompts.

Shard membership, epoch fencing, rebalance, rolling upgrade, and fault-domain isolation extend the V2 directory. They fence in-flight fills. They do not copy payloads and they are not replicas. See [0005-shard-fencing.md](0005-shard-fencing.md).

## Consequences

- `cargo test --workspace --all-targets` covers these hooks without certificates.
- A later milestone may add mTLS behind a feature flag. Until that lands, security docs must keep saying the fabric is the boundary.
- Wrong-tenant and replay failures are not hits. Callers recompute.
