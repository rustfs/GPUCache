# V1 C test matrix

Recorded for the CPU contract. Synthetic token ids are compared exactly (`tolerance=NONE`). This is not a GPU, DPU, or token-goodput report. aarch64 in CI is a CPU build, not a BlueField test.

| Check | Result | Where |
| --- | --- | --- |
| Unit and property tests, including a flipped model byte | PASS | `cargo test --workspace --all-targets`; `flipped_model_byte_is_never_a_hit` |
| Protocol fuzz: random bytes, truncation, duplicate frame, reordered frame, corrupt export | PASS | `crates/protocol/src/fuzz.rs` |
| Short write abandoned; replay is not success | PASS | `short_write_is_not_durable_and_replay_is_not_success` |
| Truncated `.gblk` is a miss after restart | PASS | `truncated_block_file_is_a_miss_after_restart` |
| Corruption (flipped payload byte) is a miss | PASS | `corrupt_file_is_not_a_hit_after_restart`, `fault-demo` `corrupt_fallback=RECOMPUTE` |
| Duplicate commit replays one block; conflicting id is rejected | PASS | `duplicate_commit_replays_one_block_and_conflict_hides_bytes`, `fault-demo` `duplicate=REPLAY_ONE_BLOCK` |
| Reordered commit-before-reserve and duplicate ids in one batch | PASS | `commit_before_reserve_and_duplicate_batch_ids_fail_closed`, `fault-demo` `reorder=REJECTED` |
| Timeout returns no payload | PASS | `timeout_and_cancel_do_not_return_payload`, `fault-demo` `timeout_fallback=RECOMPUTE` |
| Cancel quarantines and hides bytes | PASS | same tests, `cancel_fallback=MISS_QUARANTINED` |
| Server restart reloads exact tokens; dead listener is a transport fallback | PASS | `fault-demo` `restart=HIT_EXACT`, `restart_down=TRANSPORT_FALLBACK` |
| Disk full (injected) is a miss and releases retained bytes | PASS | `disk_full_abandon_leaves_a_miss`, `disk_full_and_short_write_over_tcp_are_misses` |
| `ENOSPC` / `ErrorKind::StorageFull` classified as disk full | PASS | `storage_full_and_enospc_map_to_disk_full` |
| Host volume actually filled | NOT_RUN | Not done. The injector covers the failure path |
| Unwritable blocks directory (Unix) does not leave a hit | PASS | `unwritable_blocks_directory_does_not_leave_a_hit` |
| Eviction versus pinned read keeps exact tokens | PASS | `pinned_read_keeps_exact_tokens_while_eviction_is_refused` |
| Concurrent TCP readers never observe another block | PASS | `concurrent_readers_never_observe_another_blocks_bytes` |
| Tenant key collision | PASS | `tenant_key_collision_never_returns_the_other_payload`, `fault-demo` `wrong_tenant=UNAUTHORIZED` |
| Model, tokenizer, adapter change | PASS | `model_tokenizer_adapter_and_layout_changes_are_not_hits`, `fault-demo` `MISS_ABSENT` |
| Layout mismatch | PASS | same test, `layout_mismatch=MISS_INCOMPATIBLE` |
| Memory quota exhaustion (no silent eviction in `MemoryStore`) | PASS | `memory_quota_exhaustion_does_not_evict_or_return_foreign_bytes` |
| Pinned file quota fallback | PASS | `fault-demo` `quota_fallback=RECOMPUTE` |
| Exact synthetic token check | PASS | `fault-demo` `criteria=EXACT_TOKEN_IDS`, `tolerance=NONE` |
| Real vLLM output comparison | NOT_RUN_HARDWARE | No GPU. ADR 0001 stays Pending. See [CORRECTNESS.md](CORRECTNESS.md) |
| Engine-native prefix cache, local offload, GPUCache goodput, cold/warm/no-reuse/multi-turn/HBM | NOT_RUN_HARDWARE | `cargo run -p gpucache-bench --bin paired-baseline` |
| RustFS live endpoint | NOT_RUN_HARDWARE | Unchanged from V1 B. Mock still PASS |
| DOCA / model license | NOT_RUN_LICENSE | No DOCA binary and no model weights |
| Paid GPU or DPU | NOT_RUN_BUDGET | No rental and no purchase |
| aarch64 BlueField functional test | NOT_RUN_HARDWARE | An aarch64 build is not a BlueField test |
| x86_64 CPU suite | PASS | `cargo test --workspace --all-targets` on this host (Linux 6.12.94+ x86_64, rustc 1.99.0) |
| aarch64 CPU suite on this x86_64 host | NOT_RUN | `qemu-aarch64-static` and `aarch64-linux-gnu-gcc` are not installed here. CI jobs `test-aarch64` and `test-aarch64-qemu` still run the same `cargo test`. This is not a BlueField test |

One-command CPU repro:

```bash
cargo test --workspace --all-targets
```

Fallback demo:

```bash
cargo run -p gpucache-transport --bin fault-demo
```

No erasure coding was added. V2 and V3 were not started.
