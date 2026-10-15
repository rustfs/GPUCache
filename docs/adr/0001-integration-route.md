# ADR 0001: Inference integration route

- Status: **Pending**
- Date: 2026-10-15
- Milestone: V1 A

## Context

GPUCache has to sit beside one real inference engine. The engine for the product is vLLM. Two integration routes are plausible:

1. **Native vLLM KV connector.** Use the connector hook that the pinned vLLM revision already exposes, and move KV blocks through that API. The cache then sees the engine's real block layout, dtype, and TP/PP sharding.
2. **LMCache bridge.** Reuse LMCache's existing vLLM integration and treat GPUCache as a storage backend behind that bridge. This avoids writing a second connector, at the cost of another project's release cadence, license, and layout assumptions.

V1 must pick one primary path. Maintaining both half-implemented is how layout bugs ship. The choice depends on a spike that exports one real KV block from vLLM, moves it, imports it, and checks the engine output. A compile of either project, or a fake tensor, is not that spike.

## Decision

**No route is selected.** The spike was not run.

This environment has no GPU, no approved model weights, and no budget for a rented accelerator. The spike is `NOT_RUN_HARDWARE`. V1 A stopped at the CPU contract. V1 B adds TCP and a file backend and still does not select a route. Neither vLLM nor LMCache is a crate dependency. Status stays **Pending** until the spike exists.

When a GPU is authorized, the spike should:

- pin one vLLM commit and one model revision whose license is already approved;
- export one real KV block for a fixed TP/PP layout and dtype;
- round-trip it through the V1 identity and checksum rules;
- import it and compare engine output under the engine's own determinism rules;
- record which hook already exists in that vLLM revision.

Prefer the hook that already exists in the pinned revision. Switch only if that spike fails. Do not start the second path in the same change.

## V1 C

The fault-matrix work did not run the GPU spike. This ADR stays **Pending**. No engine adapter was added.

## Consequences

- Callers must not assume GPUCache can read an arbitrary engine's KV bytes.
- Resharding, a different quant format, or a different layout ABI is a miss, not a conversion.
- A later ADR will replace this status with Accepted and name the one route. Until then, engine adapters stay out of the tree.
