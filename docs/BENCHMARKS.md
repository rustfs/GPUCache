# Benchmarks

No token-goodput run was executed for V1 A or V1 B. The TCP tests check reuse, quotas, checksums, and restart. They are not a bandwidth result.

| Comparison the V1 exit criteria ask for | Result |
| --- | --- |
| Engine-native prefix cache | `NOT_RUN_HARDWARE` |
| Same-box local offload | `NOT_RUN_HARDWARE` |
| GPUCache CPU/TCP | Correctness `PASS` via `cargo test` and `share-demo`. Token goodput `NOT_RUN_HARDWARE` (no engine, no GPU) |
| Cold / warm / no reuse / multi-turn / HBM cliff | `NOT_RUN_HARDWARE` |

Memcpy or a synthetic store bandwidth number is not token goodput and is not reported. There is no `FAIL_PERFORMANCE` figure because there is no paired measurement to fail. The CPU contract tests in `cargo test --workspace --all-targets` check correctness only.

Do not add a benchmark that prints a success banner without raw samples, a manifest, and the hardware it ran on.
