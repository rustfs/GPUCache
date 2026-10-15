# Third-party notices

GPUCache V1 A, V1 B, and V1 C are Apache-2.0. See [LICENSE](LICENSE). V1 C adds no direct dependency.

Direct dependencies:

| Crate | Version | License | Role |
| --- | --- | --- | --- |
| sha2 | 0.10.8 | Apache-2.0 OR MIT | Payload and fingerprint SHA-256 |
| proptest | 1.4.0 | Apache-2.0 OR MIT | Dev-only property tests |

Transitive crates are pinned in [Cargo.lock](Cargo.lock). They are build or test dependencies (including `rand`, `tempfile`, `regex-syntax`, and cryptographic helpers used by `sha2`). No CUDA, DOCA, NIXL, or other accelerator binary is vendored. This milestone does not publish a release and does not attach a generated SBOM; `cargo metadata --format-version 1` on this lockfile is the input for one when a release is authorized.
