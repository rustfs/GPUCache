# V1 B test matrix

Recorded on the V1 B build host after `cargo test --workspace --all-targets` and `cargo run -p gpucache-transport --bin share-demo`. This is not a GPU, DPU, or token-goodput report.

| Check | Result | Where |
| --- | --- | --- |
| TCP Lookup / Reserve-Put / Commit / Get, two clients, exact hit | PASS | `crates/transport/tests/tcp_path.rs`, `share-demo` |
| Miss, layout mismatch, cross-tenant unauthorized | PASS | `share-demo` prints `unknown_miss=PASS`, `layout_mismatch=MISS`, `cross_tenant=UNAUTHORIZED` |
| Whole-block prefix hit; intra-block slice stays a miss | PASS | `share-demo` prints `partial_prefix=WHOLE_BLOCK_PREFIX` and `intra_block_slice=DEGRADED_MISS` |
| Wrong fingerprint unauthorized; bad checksum hides bytes | PASS | `wrong_fingerprint_is_unauthorized_and_checksum_mismatch_hides_bytes` |
| Non-loopback bind refused | PASS | `non_loopback_bind_is_refused_without_an_explicit_flag` |
| Tenant and global quotas, LRU eviction, pinned block kept | PASS | `crates/storage/tests/file_backend.rs` and the TCP restart/eviction test |
| Corrupt file is not a hit after restart | PASS | `corrupt_file_is_not_a_hit_after_restart` |
| Restart reloads a committed block and bumps the epoch | PASS | `restart_reloads_committed_bytes_and_bumps_epoch`, TCP restart test |
| RustFS mock Put / Head / RangeGet / Delete plus pack and manifest | PASS | `crates/cold` `mock_put_head_range_get_delete` |
| RustFS live endpoint | NOT_RUN_HARDWARE | `missing_endpoint_is_not_run_hardware` when `GPUCACHE_RUSTFS_ENDPOINT` is unset |
| vLLM KV export/import and ADR 0001 route choice | NOT_RUN_HARDWARE | ADR 0001 stays Pending |
| aarch64 on this x86_64 host | NOT_RUN | CI jobs `test-aarch64` and `test-aarch64-qemu` still run the same `cargo test` |
| Token goodput vs engine prefix cache | NOT_RUN_HARDWARE | No GPU and no engine. See `docs/BENCHMARKS.md` |
| DOCA / model license review | NOT_RUN_LICENSE | No DOCA binary and no model weights |
| Paid GPU or DPU | NOT_RUN_BUDGET | No rental and no purchase |

One-command CPU repro:

```bash
cargo test --workspace --all-targets
```

Two-instance demo:

```bash
cargo run -p gpucache-transport --bin share-demo
```
