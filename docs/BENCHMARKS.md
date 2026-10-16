# Benchmarks

No token-goodput run was executed for V1. The TCP tests check reuse, quotas, checksums, restart, and the fault matrix. They are not a bandwidth result.

Paired baselines use the same prefix-cache idea on both sides when a GPU run exists: the engine's own prefix cache, a same-box local offload, and GPUCache CPU/TCP. Write cost is part of that comparison. This tree does not have a GPU, an engine, or a trace, so every timing row is `NOT_RUN_HARDWARE`. The harness is `cargo run -p gpucache-bench --bin paired-baseline`. It prints no sample count, no TTFT, and no tokens/s.

| Comparison | Result | Notes |
| --- | --- | --- |
| Engine-native prefix cache | `NOT_RUN_HARDWARE` | Needs a pinned vLLM commit, an approved model, and a GPU. ADR 0001 is still Pending |
| Same-box local offload | `NOT_RUN_HARDWARE` | Same GPU, model, trace, and prefix-cache settings as the engine row. Not run |
| GPUCache CPU/TCP token goodput | `NOT_RUN_HARDWARE` | Correctness is `PASS` via `cargo test`, `share-demo`, and `fault-demo`. Goodput was not measured |
| Cold | `NOT_RUN_HARDWARE` | Empty cache, engine recomputes |
| Warm | `NOT_RUN_HARDWARE` | Same prefix hits the shared block |
| No prefix reuse | `NOT_RUN_HARDWARE` | Miss path. Callers recompute and may skip the shared cache |
| Agent multi-turn | `NOT_RUN_HARDWARE` | Not run |
| HBM within capacity | `NOT_RUN_HARDWARE` | Not run |
| HBM cliff | `NOT_RUN_HARDWARE` | Not run |
| Write cost | `NOT_RUN_HARDWARE` | Not recorded. A memcpy figure would not count |
| V2 native NIXL P/D | `NOT_RUN_HARDWARE` | libnixl is not linked. `NixlTransport::open` refuses. Not a DPU result |
| V2 Rust shared DRAM | `NOT_RUN_HARDWARE` | The simulated pool is a correctness stand-in, not a bandwidth run |
| V2 Rust shared NVMe | `NOT_RUN_HARDWARE` | `FileStore` uses ordinary files. No device was formatted |
| V2 RustFS refill | `NOT_RUN_HARDWARE` | Mock refill passes. Live endpoint was not configured |
| V2 TTFT / TPOT / ITL / goodput | `NOT_RUN_HARDWARE` | No paired GPU trace. `staged_bytes` is a copy length, not wire bytes |

There is no `FAIL_PERFORMANCE` figure because there is no paired measurement to fail. Do not add a benchmark that prints a success banner without raw samples, a manifest, and the hardware it ran on. The V2 CPU tests do not fill this gap.

Mooncake's published transfer and serving figures are not rows in this table. `paired-baseline` prints `mooncake_transfer_engine=NOT_LINKED` and `mooncake_published_numbers=NOT_REPRODUCED`. Those lines are labels. They are not a reproduction and they contain no sample. See [MOONCAKE.md](MOONCAKE.md).

## V3 paired gate

The harness is `cargo run -p gpucache-bench --bin v3-paired`.

The comparison it names is **NIC RDMA baseline vs DPU storage queue**, under the same GPU, model, trace, service behavior, capacity, policy, auth semantics, and SLO. TCP is forbidden as that baseline (`tcp_used_as_dpu_baseline=FORBIDDEN`). A weak TCP number would not be a DPU result. Soft-RoCE, if someone later smokes it on a hypervisor, is not a performance result and must not be copied into this table.

| Row | Result |
| --- | --- |
| NIC RDMA baseline | `NOT_RUN_HARDWARE` |
| DPU storage queue | `NOT_RUN_HARDWARE` |
| TTFT / TPOT / ITL / SLO goodput | `NOT_RUN_HARDWARE` |
| Host core-seconds, DPU CPU, DPU DRAM, GPU interference | `NOT_RUN_HARDWARE` |
| Wire bytes, SSD write amplification, wall power | `NOT_RUN_HARDWARE` |
| TCO | `TCO_UNKNOWN` |
| Mooncake RDMA | `NOT_A_PERFORMANCE_RESULT`. Not linked, and not a stand-in for the NIC RDMA baseline |
| Go gate | `NOT_EVALUATED` |
| PB-scale | `NOT_CLAIMED` |
| Production ready | `NOT_CLAIMED` |

Engineering targets, not measurements, and not yet confirmed as the go gate: full cost per million useful output tokens improves by at least ten percent with correctness and SLO held, or an isolation goal that removes a whole x86 box is met without a higher total cost. A twenty percent drop in host core-seconds is only a mechanism signal. It does not by itself prove the cost target. Until a paired run exists the gate stays `NOT_EVALUATED`. A no-go keeps the portable TCP and file backends. This tree does not buy hardware to chase the gate.

Soak of a day or longer, with the fault matrix on real devices, was not run.

Correctness criteria for the CPU path are in [CORRECTNESS.md](CORRECTNESS.md).
