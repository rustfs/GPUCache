# Benchmarks

No performance run was executed for V1 A.

| Comparison the V1 exit criteria ask for | Result |
| --- | --- |
| Engine-native prefix cache | `NOT_RUN_HARDWARE` |
| Same-box local offload | `NOT_RUN_HARDWARE` |
| GPUCache CPU/TCP | `NOT_RUN` (TCP service is V1 B; no tokens were served) |
| Cold / warm / no reuse / multi-turn / HBM cliff | `NOT_RUN_HARDWARE` |

Memcpy or a synthetic store bandwidth number is not token goodput and is not reported. There is no `FAIL_PERFORMANCE` figure because there is no paired measurement to fail. The CPU contract tests in `cargo test --workspace --all-targets` check correctness only.

Do not add a benchmark that prints a success banner without raw samples, a manifest, and the hardware it ran on.
