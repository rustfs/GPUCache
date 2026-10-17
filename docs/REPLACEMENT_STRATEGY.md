# Adoption and replacement strategy

Status: **PROPOSED / NOT IMPLEMENTED**. This document describes future integration choices. It does not add an engine adapter, a Rust SDK crate, a native transport or a hardware dependency. Current implementation follows the V3 CPU boundary. [ADR 0001](adr/0001-integration-route.md) and [ADR 0008](adr/0008-second-engine.md) remain Pending.

GPUCache should replace a measured cache-tier capability in a supported configuration. It should preserve the inference engine, its fastest local cache and a working transport where appropriate. A claim that GPUCache replaces a whole inference stack would exceed its data-tier scope.

## Adoption sequence

1. **Conformance first.** Publish the Rust client contract and prove existing identity, commit, checksum, tenant, pin and miss behavior through portable backends. This SDK is proposed, not currently a workspace crate.
2. **Choose one primary integration.** On authorized GPU hardware, pin vLLM and model revisions and run the real block round trip specified in ADR 0001. Prefer the existing hook in that revision. Select the native connector or LMCache bridge; do not ship two half-built routes.
3. **Start as an optional shared tier.** Preserve GPU-local prefix caching. Cache unavailability should trigger bounded fallback/recompute, with admission and deadlines that prevent refill and prefill storms from consuming the fleet.
4. **Measure against the deployment being replaced.** Keep policy, capacity, auth semantics, trace, model, GPU layout and SLO constant. Include write/population costs and no-reuse traffic. Publish regressions as well as wins.
5. **Expand only after evidence.** Additional engines, distributed production routing, RDMA, GDS and BF3 require the distinct gates in the [roadmap](ROADMAP.md). They are not implied by a storage backend adapter.

## Capability-level migration

| Existing deployment | Proposed first integration | Replacement claim allowed after acceptance |
| --- | --- | --- |
| vLLM APC | Retain GPU-local APC; select a supported external connector route after ADR 0001 | A tested external shared-cache path for the pinned engine/model/layout; not replacement of vLLM scheduling or attention |
| LMCache | If selected, implement GPUCache behind the supported storage interface and preserve the existing engine bridge | The tested backend capability and modes; not automatic equivalence to CacheBlend, compression or every LMCache backend |
| SGLang HiCache | Evaluate an L3 backend after the primary route works; retain RadixAttention and private local tiers | The qualified L3 behavior for that SGLang revision; not a universal KV format or an already implemented adapter |
| Mooncake Store | Evaluate GPUCache as an alternate shared store while preserving a supported TE where useful | Equivalent tested object/commit/failure behavior through the selected engine integration; no undocumented wire-compatibility promise |
| NIXL | Use a qualified NIXL transport integration where appropriate | A compatible cache tier using a transport library; GPUCache need not replace NIXL |
| NVIDIA Dynamo | Expose reuse through a supported engine integration and, later, versioned KV lifecycle events | Verified cache-tier integration and router visibility; not replacement of orchestration, autoscaling or inference workers |
| File, Redis or object-store offload | Add a compatibility probe and explicit migration tool only when their layouts are understood | Migration of compatible bytes, or safe regeneration for incompatible entries; not arbitrary cross-model KV reuse |

The table identifies proposed integration boundaries rather than promising feature parity with any project. Validate upstream interfaces against pinned official revisions. Product names and plugin interfaces alone are not a compatibility test.

## Compatibility contract

A reusable block must match its authenticated tenant and trust domain, model revision/configuration, tokenizer, adapter, multimodal identity where relevant, position/RoPE configuration, dtype/quantization, layout ABI, TP/PP shard and valid lengths. Preserve the distinction between logical prefix identity and physical byte identity.

Only a checksum-valid committed block is readable. A whole-block `PrefixHit` requires the documented prefix identity and matched length; it is not an instruction to slice arbitrary KV tensors or decode the shorter block as the entire request. See [protocol](PROTOCOL.md) and [partial-prefix ADR](adr/0003-partial-prefix.md).

Non-prefix KV composition, resharding and format conversion are separate inference features. Storing or transporting bytes does not implement them. A model/layout change is a miss until an independently validated conversion is accepted.

## Side-by-side evaluation and rollback

The first proposed deployment should support a reversible baseline/candidate selection. Use an isolated namespace and bounded cache budget. Do not silently merge tenants or deduplicate private prefixes across tenants. Shadow writes may test population and integrity, but cannot establish real restore correctness by themselves.

Before moving traffic, demonstrate real engine restore, declared output equivalence, error/fallback behavior and paired SLO goodput. After moving a bounded cohort, monitor lookup latency, saved prefill work, admission denials, pin lifetime, quarantine, fallback rate and useful output tokens. Cache hit ratio alone is not the decision metric.

Rollback should stop new candidate traffic, cancel or drain outstanding work under the lifecycle contract, and preserve the baseline engine path. Unknown completion remains quarantined until verified drain/fencing. Leaving a candidate cache namespace behind is preferable to freeing a region that might still receive DMA. Cold-data retention/deletion follows its explicit policy; rollback is not permission to delete user objects.

## Reliability and isolation boundaries

GPUCache does not maintain hot durability replicas, parity or replica repair. Rebalance and fault-domain hooks currently fence fills rather than copy payloads. A lost hot block is a miss and may be recomputed/repopulated. RustFS supplies the colder durability boundary. See [ADR 0002](adr/0002-no-erasure-coding.md).

Multi-node availability must be tested within a defined failure envelope. More cache misses after node loss can increase prefill pressure; reserve capacity, per-tenant admission, refill budgets and bounded retries are necessary before promising an SLO. A failed decode worker still needs engine-level request recovery. Never claim arbitrary simultaneous node loss has no effect.

Current TCP uses a bearer-equivalent credential fingerprint on a trusted fabric. mTLS is not linked; V3 ACL/replay/rate/rotation hooks are not wired into the listener. These are migration constraints, not completed network security. See [security](SECURITY.md) and [trusted-fabric ADR](adr/0007-trusted-fabric.md).

## Evidence required in a replacement report

- Exact baseline/candidate commits and configurations, model revision, hardware/topology and active data path.
- Supported engine operations and modes, plus explicit unsupported combinations.
- Correctness criterion, raw samples, command logs, warm/cold/no-reuse tests and failure results.
- Population cost, TTFT/TPOT/ITL distributions, SLO goodput and capacity/resource costs.
- Rollback procedure and observed behavior under cache outage, noisy tenants and partial transfer failure.
- A scoped conclusion such as “replaces the tested remote KV backend for configuration X.”

Until those results exist, use “proposed integration” or `NOT_RUN_HARDWARE`. Hardware acceleration and TCO remain unmeasured. See [benchmarks](BENCHMARKS.md).
