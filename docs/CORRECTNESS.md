# Correctness criteria (V1 C)

GPUCache V1 C checks the CPU contract. It does not check a GPU kernel.

## What is compared

Synthetic blocks use `SKV1` version 1: a little-endian count and a list of `u32` token ids. The demo and the fault tests encode those ids, store the bytes, and decode them on every hit.

The pass rule is exact equality of the token-id list and of the raw payload bytes. `tolerance=NONE`. There is no absolute or relative numeric slack, because this path has no floating-point kernel.

On a miss, timeout, cancel, checksum failure, corrupt file, short write, full disk, quota refusal, or a stopped server, the caller recomputes with `deterministic_recompute`. That function is a pure encoding of the token ids the caller already has. It does not read another tenant's block. The cache result is not used.

## What is never a hit

A lookup or get is a failure of this milestone if it returns bytes for any of these:

- a different tenant or trust domain
- a different model revision or model-config digest
- a different tokenizer revision
- a different adapter or LoRA revision
- a different layout ABI, dtype, quant, shard, or RoPE
- a block that is not `COMMITTED`, including reserved, writing, evicting, and quarantined
- a checksum mismatch, a truncated file, or a byte flipped on disk
- a commit the service reported as a failed durable write

Model, tokenizer, and adapter changes are different logical keys. The lookup reason is `MISS_ABSENT`. A layout change keeps the logical key and is `MISS_INCOMPATIBLE`. `Get` of the stored generation with the wrong identity is `Incompatible` and returns no payload. A wrong tenant is `Unauthorized` and returns no payload.

## Limits

- These token ids are not vLLM output. A real export, import, and engine-output comparison is `NOT_RUN_HARDWARE` until it runs on a GPU. ADR 0001 stays Pending.
- Non-deterministic GPU kernels are not given a tolerance in this tree. When a GPU run exists, that run must publish its own criterion. It must not loosen the rule that a wrong tenant or a wrong model is a miss.
- `deterministic_recompute` does not approximate attention. It only proves the cache did not substitute a different token list.
- Protocol fuzz is `proptest` over the decoders inside `cargo test`. It is not a multi-hour libFuzzer campaign. Decoders must not panic. A frame that decodes must round-trip. Truncation, duplication, and a rotated frame must fail closed.
- Filling the host disk is `NOT_RUN`. Disk-full behavior is the `PersistFault::DiskFull` path plus the `ENOSPC` / `ErrorKind::StorageFull` classifier. A chmod-unwritable directory covers a real failed write on Unix.
- aarch64 `cargo test` is a CPU build. It is not a BlueField test.

## Workloads that should bypass the shared cache

A request with no prefix overlap is a miss. The engine recomputes. Callers can skip the shared cache for that traffic. The CPU tests show the miss. They do not show a token-goodput win. See [BENCHMARKS.md](BENCHMARKS.md).
