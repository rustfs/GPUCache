# V2 test matrix

CPU contract for the first V2 slice. Live RDMA, live RustFS, GPU export, and token goodput stay `NOT_RUN_HARDWARE`. This file is not a performance report.

| Check | Result | Where |
| --- | --- | --- |
| Default workspace tests, including V1 A/B/C | PASS | `cargo test --workspace --all-targets` |
| TCP transport trait: one host copy, backpressure, `dma_posted=false` | PASS | `iface::tests` |
| `NixlTransport::open` refuses without a device. `submit_preferring_rdma` labels the TCP fallback `RdmaNotLinked` and does not post | PASS | `iface::tests`, `tests/v2_control.rs` |
| Feature `rdma-nixl` still refuses `open` | PASS | `cargo test -p gpucache-transport --all-targets --features rdma-nixl` |
| Registration reuse, prefer an idle binding, atomic checkout, quarantine, lease hold, fence reclaim, epoch overflow | PASS | `registered::tests` |
| Failed TCP stage quarantines the simulated slots | PASS | `stage::tests` |
| Single-node directory is one shard; join charges credits once | PASS | `directory::tests` |
| Two shard values do not share flights | PASS | `directory::tests::two_shard_values_do_not_share_flights` |
| Stale epoch fences the fill and releases credits. Finish, failure, and fence are distinct notices with no payload. Epoch overflow keeps the fill | PASS | `directory::tests` |
| Other tenant cannot join; quota does not drop the leader | PASS | `directory::tests` |
| `shard-demo` prints `SHARD_OK` and `rdma=NOT_RUN_HARDWARE` | PASS | `cargo run -p gpucache-transport --bin shard-demo` |
| Cold mock Put/Head/RangeGet/Delete, pack, manifest | PASS | `contract_smoke` |
| Async sink drain, cancel writes nothing, queue backpressure | PASS | `sink::tests` |
| Refill over budget does not download the payload (or the manifest, when the manifest itself is over budget). A wrong tenant does not download the payload. A failed drain keeps the job | PASS | `sink::tests` |
| `cold-demo` prints `COLD_CONTRACT_OK` and `gds=NOT_RUN_HARDWARE` | PASS | `cargo run -p gpucache-cold --bin cold-demo` |
| Live RustFS Put/Head/RangeGet/Delete | NOT_RUN_HARDWARE | No `GPUCACHE_RUSTFS_*` endpoint |
| Live NIXL / Soft-RoCE / hardware RDMA | NOT_RUN_HARDWARE | `libnixl` is not linked. See [adr/0004-nixl-transport.md](adr/0004-nixl-transport.md) |
| GPU KV export, NIXL P/D, TTFT, TPOT, goodput | NOT_RUN_HARDWARE | `cargo run -p gpucache-bench --bin paired-baseline` |
| BlueField, DOCA, GDS, SPDK | NOT_RUN_HARDWARE | Not started. Not an erasure-coding milestone |
