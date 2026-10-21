# CPU/TCP completion audit

Baseline: `8af53146e47a` (verify the full revision in the regression environment
report). This audit describes software contracts, not production qualification.

| Area | Baseline gap / preserved invariant | Implementation and regression evidence |
| --- | --- | --- |
| A: hot authorization | ACL and rate hooks were not called by TCP | `server::HotSecurity::admit` gates every data operation before storage dispatch; `tests/hot_security.rs` exercises socket-level denial, tenant isolation and rate exhaustion |
| A: lifecycle | Only committed, compatible blocks may hit; unknown completion retains credit | Existing lifecycle/fault tests retained. `MemoryStore::get_one` bounds reader pins and aggregate reply payloads before copying; reader cancel storm tests retain expired pin credit until explicit drain/fence |
| A: prefix | Whole stored blocks only | ADR 0003 remains unchanged; prefix and incompatible valid-length tests run in workspace regression |
| B: client | No routed connection pool or retry ledger | `routing::ShardedClient` bounds sockets, routes and retries; shard sessions preserve six RPC methods and shard-local generations; `tests/sharded_tcp.rs` and routing unit tests exercise lost replies, concurrent restart and stale epochs |
| B: membership | No atomic batch membership update | `ShardDirectory::apply_membership` validates before fencing/publication; `ControlPlane::apply_membership` uses the same atomic path; failed fences preserve fills, credits and notices |
| C: availability | Actual cold read errors could escape as errors | `refill_matching` returns `Missing` for availability failures without store mutation; `tests/failure_contract.rs` covers HEAD, manifest and payload stages |
| C: publication and reclaim | Retention errors could discard queued work or starve healthy jobs | Cold reclaim retains and rotates failed work; renewal cancels stale expiry; identity/digest/v3-only publication and ambiguous-PUT non-deletion tests remain enforced |
| D: simulation | Queue existed without a host message facade or functional TCP pair | `HostSimulation`, message cancellation/fault tests and `cpu_simulation_pair.py` compare synthetic byte checksums; no timing/performance result |
| E: engine spike | No independent expected-ABI import harness | `EngineBlockContract` and `engine-spike` reject incompatible imports; real engine route remains Pending |
| F: observability | Descriptor/staging counts could be mistaken for wire measurements | `ClientIoStats` separates complete RPC body/envelope byte counts; pool/directory stats expose fence, drain and quarantine state; these exclude network headers, retransmits and partial writes |

## Remaining explicit boundaries

- Deployments must authenticate and protect the network boundary. GCK1 carries a
  credential fingerprint, not a TLS session or a nonce. mTLS, replay-proof
  transport authentication and automatic secret rotation are not implemented.
  Empty/restrictive ACL and unlinked-feature tests enforce fail-closed behavior.
- Automatic mutation retries are disabled by default. Opt-in requires an
  operator guarantee that server idempotency retention exceeds request TTL.
  `Get` is never automatically retried because it creates a pin. Tests enforce
  that an ambiguous Get or default mutation sends only one request.
- Routing uses explicit operator-fenced epoch snapshots. This is not distributed
  consensus, automatic discovery or a live control-plane daemon. Prefixes on a
  different shard are a clean miss; the client does not fan out globally.
- Synchronous filesystem/object-store operations cannot be preempted by the
  core trait. Adapters must enforce I/O deadlines; a declared slow cold tier
  short-circuits without I/O. The mock tests do not prove a wall-clock bound on
  arbitrary blocking adapters.
- Automatic orphan discovery/GC is not implemented. Tests ensure a failed or
  ambiguously acknowledged PUT does not cause deletion. Explicit retention
  reclaim only touches enrolled objects and verifies deletion.
- No fleet-size, PB-scale, 10,000-GPU, day-long soak, physical NIC flap or
  hardware drain has been qualified. More complete CPU contracts do not make
  this repository production-ready.

## Reproduction

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test -p gpucache-transport --all-targets --features rdma-nixl
python3 scripts/ci/vm_regression.py --profile cpu --offline --output artifacts/astra-full-cpu
```

Fetch locked dependencies once before an offline run. A new output directory is
required for every retry so failed evidence is retained. `results.json` records
the actual commands and logs; `environment.json` records revision and dirty
state. Command runtimes are never reported as product latency or throughput.
