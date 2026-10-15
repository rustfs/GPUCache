# Third-party notices

GPUCache is Apache-2.0. See [LICENSE](LICENSE). The V2 first slice adds no direct dependency. NIXL (`nixl-sys`, Apache-2.0) and Mooncake (Apache-2.0) were audited and not linked. See [docs/adr/0004-nixl-transport.md](docs/adr/0004-nixl-transport.md).

Direct dependencies:

| Crate | Version | License | Role |
| --- | --- | --- | --- |
| sha2 | 0.10.8 | Apache-2.0 OR MIT | Payload and fingerprint SHA-256 |
| proptest | 1.4.0 | Apache-2.0 OR MIT | Dev-only property tests |

Transitive crates are pinned in [Cargo.lock](Cargo.lock). They are build or test dependencies (including `rand`, `tempfile`, `regex-syntax`, and cryptographic helpers used by `sha2`). No CUDA, DOCA, NIXL, or other accelerator binary is vendored. This milestone does not publish a release and does not attach a generated SBOM; `cargo metadata --format-version 1` on this lockfile is the input for one when a release is authorized.
