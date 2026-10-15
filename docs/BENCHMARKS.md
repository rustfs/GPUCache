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

There is no `FAIL_PERFORMANCE` figure because there is no paired measurement to fail. Do not add a benchmark that prints a success banner without raw samples, a manifest, and the hardware it ran on.

Correctness criteria for the CPU path are in [CORRECTNESS.md](CORRECTNESS.md).
