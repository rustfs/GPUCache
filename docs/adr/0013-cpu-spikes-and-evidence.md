# ADR 0013: CPU spike contracts and evidence boundaries

- Status: Accepted for CPU contracts; engine and hardware integration Pending
- Date: 2026-10-20
- Milestone: V3

## Decision

`EngineBlockContract` is a Rust-side ABI spike for opaque whole blocks. It
validates an independently supplied full identity, exact payload length,
nonzero source generation and checksum before an import. Its ABI version is
independent of the unchanged GCK1/GCKR/GEXP formats. It performs no layout
conversion, tensor slicing, authentication or model computation.

`engine-spike` uses visibly synthetic bytes. Its success label is
`FAKE_BLOCK_CONTRACT_OK`; real export/import is `NOT_RUN_HARDWARE` and the
GPU/model campaign is `NOT_RUN_BUDGET`. ADR 0001 stays Pending. This spike does
not select native vLLM versus LMCache and is not an engine adapter or FFI.

`HostSimulation` dispatches synchronous typed storage-queue messages on the
host without another queue or inbox. Unknown cancellation keeps credits until
an explicit simulated drain. Fault replies contain recompute decisions, not
payloads. It is not a network/device message format.

The CPU runner compares independently checked DPU-simulation and localhost
TCP payload digests and executes both failure paths. This is a functional
pair, labeled `NOT_A_PERFORMANCE_RESULT`. It is not the NIC-RDMA versus
BlueField comparison required for TCO. Device opens continue to fail closed
with empty feature dependencies; aarch64 remains CPU evidence only.

## Verification and limitations

Workspace tests exercise cross-identity imports, corrupt/truncated exports,
cancel storms, explicit drain and simulated fault decisions. The bounded
export property suite generates valid payloads and checks truncation, trailing
bytes and payload bit flips. The runner includes `engine-spike` and
`cpu-simulation-pair` as required CPU gates.

No CUDA/DOCA/SPDK/NIXL/Mooncake dependency is added. Real vLLM, RDMA, GDS,
BF3 and live RustFS remain `NOT_RUN_HARDWARE`. No model was downloaded and no
license evaluation was run. `TCO_UNKNOWN` and `go_gate=NOT_EVALUATED` stay in
force. No hot erasure coding, durability replica or performance sample exists.
