# Operations (V1 A)

V1 A is a library plus one import helper. There is no daemon, no package, and no production config.

## Reproduce the CPU tests

```bash
cargo test --workspace --all-targets
```

Rust stable is selected by `rust-toolchain.toml`. The declared MSRV is 1.85. CI also runs `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`.

## Import one frame

`kv-block-xfer` is built by the test suite. It reads a `GEXP` frame on stdin:

```bash
cargo run -p gpucache-storage --bin kv-block-xfer -- import <64-hex-char fingerprint>
```

The fingerprint is the tenant credential hash, not a prompt. The tool creates a one-tenant `MemoryStore`, commits the block, and prints `status=IMPORTED` only after lookup and get succeed. A checksum failure exits 2 and prints `error=` on stderr. This path uses synthetic or caller-supplied bytes. It does not contact vLLM.

## Not operated here

No NVMe format, no DPU mode change, no firmware, no IOMMU or BIOS change, no production network change, no RustFS bucket. Those need a separate authorization. There is no website deploy and no crates.io publish in this milestone.
