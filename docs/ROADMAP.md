# GPUCache roadmap

This roadmap separates the current V3 CPU scope from proposed follow-up work. A proposed stage is a design direction, not an implemented feature or a release commitment. [AGENTS.md](../AGENTS.md), the accepted ADRs, and the [compatibility manifest](../COMPATIBILITY.md) define the current implementation boundary.

GPUCache is the G3.5 shared KV tier. RustFS is the colder G4 backend, off the prefill-to-decode hot path. A lost hot block is a miss followed by engine recomputation. GPUCache will not add hot-tier erasure coding or durability replicas. See [ADR 0002](adr/0002-no-erasure-coding.md).

## Current evidence

| Area | Current scope | Evidence boundary |
| --- | --- | --- |
| V1 A/B/C | Identity, lifecycle, protocol, memory/file stores, loopback TCP reuse, synthetic correctness and fault tests | CPU contracts. Synthetic bytes are not real engine KV tensors |
| V2 first slice | Transport trait, simulated registered buffers, shard-local directory, fencing, single-flight, credits, mock G4 packs and refill | One process is one shard. No distributed metadata service or quorum |
| V3 | Simulated storage-side DRAM slab and storage queue, quarantine/drain and tenant invalidation/reclaim contracts; CPU security/control hooks; mock cold operations | DOCA, SPDK, RDMA, CUDA, GDS and a BlueField device are not linked |
| Engine integration | Native vLLM connector versus LMCache bridge remains undecided | [ADR 0001](adr/0001-integration-route.md) is Pending; real round trip is `NOT_RUN_HARDWARE` |
| DPU decision | Storage queue is the sole first offload candidate | Hypothesis only. Paired NIC-RDMA versus DPU measurements are `NOT_RUN_HARDWARE`; `TCO_UNKNOWN` |

The V3 hooks are not complete service enforcement. The TCP service does not call the V3 ACL, replay guard, rate gate or secret ring, and it does not terminate mTLS. Existing payload and writer limits are not a complete process-RSS bound. See [security](SECURITY.md) and [protocol](PROTOCOL.md).

## V3 completion work

Current V3 work remains CPU-only. Preserve the crate dependency restrictions and `unsafe` prohibition in AGENTS.md.

1. Make the existing CPU contracts reproducible from a clean Linux VM, with structured results, raw command logs and explicit unavailable hardware rows. See [VM regression](VM_REGRESSION.md).
2. Keep every V1/V2 test passing while correcting documented V3 contract defects. A successful simulation must continue to report `offload_active=false` and `NOT_RUN_HARDWARE`.
3. Preserve the failure boundary: hiding bytes requests recompute but retains slab capacity until the explicit CPU drain/invalidation/reclaim rules permit release. Quarantined tasks retain credits and cannot publish a late completion; the simulation caps task and slot counts at 4,096. A CPU attestation is not evidence of a hardware drain. Never free a future DMA region merely because its lease expired.
4. Make follow-up work reviewable as small English PRs with their own acceptance evidence. See [PR plan](PR_PLAN.md).

V3 completion does not close the pending engine route, certify a physical fault campaign, or certify service operation on an untrusted network.

## Proposed follow-up stages

All stages below are **PROPOSED / NOT IMPLEMENTED**. Progress through their technical prerequisites in order. Hardware linkage requires revisiting the current dependency and safety restrictions; this roadmap does not override them.

| Proposed stage | Deliverable | Acceptance gate |
| --- | --- | --- |
| R1: Rust client SDK | A typed client facade over the existing identity, lifecycle and batch protocol; documented deadlines, cancellation, pins, bounded retries and compatible-version negotiation | Memory/file/TCP conformance; wrong tenant/model/layout returns no payload; every successful read pin is released; request and auxiliary-state admission is explicitly bounded and tested |
| R2: One real engine route | Run the vLLM spike, accept ADR 0001, then implement only the selected native connector or LMCache route | Pin engine/model revisions, layout, dtype and TP/PP; export/import a real GPU KV block; compare output under a declared determinism criterion; exercise miss and failure fallback |
| R3: Multi-node service | Node discovery and shard routing, bounded stale metadata, epoch fencing, admission credits, drain and controlled refill | Multi-process/VM campaign for simultaneous node loss, partition, restart, rolling upgrade and hot-prefix storms; correct surviving hits or bounded misses; no claim of seamless active-decode recovery |
| R4: Native NIXL/RDMA path | Optional transport integration with actual registration, completion, cancellation/drain and quarantine | Real RDMA hosts; path/device evidence; tenant access bounds; NIC failure and unknown completion cannot expose or prematurely recycle buffers; a simulated pool is insufficient |
| R5: Live cold backend and optional GDS | Live RustFS pack/manifest operations, cost-bounded refill, then a separately qualified GDS path | Authorized endpoint and integrity/delete/budget tests; GDS topology, cuFile/filesystem and direct-versus-fallback evidence; cold writes/refill do not become mandatory PD hops |
| R6: Storage-side BF3 experiment | The single storage-queue offload in ADR 0006, only after a real NIC-RDMA baseline | Same behavior, capacity, policy, auth, GPU/model/trace and SLO on both sides; raw repeated samples; mechanism, power and full cost evidence; a no-go retains portable backends |
| R7: Second engine and fleet qualification | SGLang/HiCache evaluation after the primary interface is stable; staged fleet and fault qualification | Accept ADR 0008 only after its prerequisites; qualify each engine/layout separately; publish simulated, VM and physical GPU scale as separate results |

R4, R5 and R6 are distinct evidence gates. A GDS pass does not certify RDMA or DPU offload. Starting a daemon on Arm does not satisfy R6. An installed SDK, device name, compile or successful stub command does not satisfy a hardware gate.

## Stable boundaries for the proposed SDK

| Component | Owns | Must not assume |
| --- | --- | --- |
| Engine adapter | Engine block tables, GPU execution ordering, tensor export/import, layout identity and recompute decisions | Arbitrary KV formats are interchangeable; a token hash alone identifies physical bytes |
| Rust SDK | Protocol versions, typed hit/miss/error outcomes, deadlines, request IDs, cancellation and retry classification | Every operation is freely retryable; repeated Get is pin-free; an auth failure is a valid hit |
| Transport | Transfer of approved byte ranges and trustworthy completion reporting | Completion publishes a block; lease expiry proves DMA stopped; a CPU pointer is a GPU region |
| Cache service | Commit visibility, checksums, admission, eviction, isolation and epoch fencing | A transport library supplies membership, tenant policy or persistence automatically |
| Cold backend | RustFS object/manifest operations, retention and bounded refill | Cold storage is an obligatory P-to-D hop or repairs a lost hot block automatically |

Keep GPU/DPU types and dependencies out of the portable core. A proposed foreign-language binding should be narrow and versioned; it is not a second implementation of identity or lifecycle rules. Compatibility mismatch remains a miss unless a future, independently tested conversion is explicitly accepted.

## Performance and reliability gates

Predeclare workload, sample count, cache state, concurrency, prompt/output lengths, reuse ratio, SLO and error policy before a paired run. Include population/write cost, cold misses, warm hits, no-reuse traffic, HBM pressure, noisy neighbors and failures. Report TTFT and TPOT/ITL distributions, SLO goodput, useful output tokens, host/DPU resources, network/PCIe traffic and total cost where measurable.

PD separation is a scheduling option, not a universal throughput improvement. Native prefix caching, local offload and native PD transport remain comparison configurations. A DPU decision requires the NIC-RDMA baseline in [benchmarks](BENCHMARKS.md); TCP is not that baseline. Existing cost targets are hypotheses until the paired go gate is confirmed.

The 10,000-GPU objective requires physical fleet evidence and a stated fault envelope, spare inference capacity and SLO. A 10,000-client simulator only tests its simulated workload. Cache node loss may correctly cause misses, extra prefill work and higher latency. Interrupted generation needs engine-level recovery/replay; preserving a cache API is not proof of uninterrupted decoding.

## Evidence labels

- `PASS` means the named executed check passed in its recorded environment.
- `SIMULATED` describes an executed CPU stand-in; it does not upgrade a hardware row.
- `NOT_RUN_HARDWARE` means required devices or a real path were not exercised.
- `NOT_RUN_LICENSE` and `NOT_RUN_BUDGET` preserve their existing meanings.
- `TCO_UNKNOWN`, `NOT_EVALUATED` and `NOT_CLAIMED` remain until supporting evidence exists.

Do not rewrite unavailable results as zero timings, zero cost, an implicit pass, PB-scale verification or production readiness. [Replacement strategy](REPLACEMENT_STRATEGY.md) describes adoption without those claims.
