# ADR 0008: SGLang / HiCache stays an evaluation note

- Status: **Pending**
- Date: 2026-10-15
- Milestone: V3

## Context

V3 asks for a second engine after the primary interface is stable, with SGLang / HiCache as the preferred evaluation. Each layout has to be tested on its own. A KV block is not a universal tensor.

The primary route is still undecided. [0001-integration-route.md](0001-integration-route.md) is **Pending** because the vLLM spike was not run (`NOT_RUN_HARDWARE`). Adding a second engine now would be a second half-built path.

## Decision

Do not add SGLang, HiCache, or any other engine adapter in this change.

When ADR 0001 is Accepted and one vLLM path round-trips a real block, a follow-up may evaluate SGLang / HiCache against that same identity and checksum contract. The evaluation has to use that engine's layout, dtype, and shard placement. A miss on mismatch stays a miss. This note is not an integration and not a claim that formats are interchangeable.

## Consequences

- No new crate, dependency, or Python package was added for a second engine.
- Docs must not say GPUCache speaks SGLang or HiCache today.
