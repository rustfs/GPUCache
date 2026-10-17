# Staged PR plan

This is an English review plan. Current implementation scope remains V3 CPU work. Every follow-up below is **PROPOSED / NOT IMPLEMENTED**, not a completed milestone. Crate publication, releases, hardware purchases, deployment and website changes remain outside this implementation plan.

The accepted [no-EC decision](adr/0002-no-erasure-coding.md), [integration decision](adr/0001-integration-route.md), [storage-queue candidate](adr/0006-storage-queue-offload.md) and crate dependency rules in [AGENTS.md](../AGENTS.md) take precedence over optional future designs.

## Current V3 review slices

| Proposed PR title | Scope | Required evidence |
| --- | --- | --- |
| `docs: define delivery gates and scoped replacement strategy` | Roadmap, compatibility boundaries, replacement gates and VM runbook | Links resolve; future work is labeled proposed; no simulated/hardware claims are conflated |
| `ci: add dependency-free VM regression reports` | Standard-library Python CPU runner, environment/capability/results reports, command logs and strict unavailable hardware profiles | Clean-checkout CPU execution; invalid arguments, missing toolchain, failed command and timeout fail predictably; unavailable hardware cannot silently pass |
| `fix: close reproduced V3 CPU contract defects` | Only defects established by regression evidence; retain V1/V2 behavior | Focused regression for each defect, full required Rust checks, updated limitations where behavior remains incomplete |

These slices may be reviewed independently when their prerequisites are present. A documentation PR must not describe a planned runner, SDK or integration as implemented before its code is reviewed and executable.

## Proposed follow-up PR sequence

| Order and proposed title | Prerequisite | Reviewable result and acceptance |
| --- | --- | --- |
| 1. `feat(sdk): add a typed Rust client and backend conformance` | Existing protocol read and preserved | Narrow client facade, examples, bounded retries and pin/cancel ownership; shared conformance for memory/file/TCP; no GPU dependency in the core |
| 2. `docs(adr): select the primary vLLM integration route` | Authorized GPU, approved/pinned model and a real spike | Commit/model/layout manifest, actual block export/import, output comparison and native hook evidence; accept one route in ADR 0001 |
| 3. `feat(adapter): implement the selected engine cache route` | ADR 0001 Accepted | One native vLLM connector or LMCache bridge; real restore and failure fallback; narrow binding and compatibility matrix |
| 4. `feat(cluster): add bounded multi-node cache routing` | Service/metadata design recorded | Actual discovery/routing, stale ownership and drain behavior; multi-process fault tests; cache misses after loss remain valid; no hot replicas |
| 5. `feat(transport): add qualified NIXL RDMA transfers` | Dependency/safety ADRs revisited; licenses reviewed; hardware available | Actual registration/completion and fencing, device/path evidence, fault tests and native transport baseline; TCP stays usable |
| 6. `feat(cold): validate live RustFS refill and optional GDS` | Authorized endpoint; independent GDS prerequisites | Live object/manifest integrity, deletion and budget evidence; optional GDS direct/fallback evidence; G4 remains off mandatory PD path |
| 7. `feat(dpu): evaluate storage-side BF3 queue offload` | Real NIC-RDMA baseline and compatible BFB/DOCA/firmware | One storage-queue path, raw paired samples and predefined go/no-go decision; no Arm-start, TCP-baseline or unmeasured TCO shortcut |
| 8. `feat(adapter): qualify SGLang HiCache integration` | Primary path stable; ADR 0008 revisited | Separate engine/layout qualification and TP consistency; no claim of interchangeable engine KV bytes |
| 9. `test(scale): publish staged fleet and fault qualification` | Functioning service/integrations and an authorized fleet | Separate simulated/VM/physical scale, failure envelope, SLO and reserve-capacity evidence; no extrapolated 10,000-GPU certification |

Hardware PRs can remain unstarted indefinitely without weakening the portable CPU implementation. A no-go for the DPU candidate is a valid measured result. Do not add an acceleration dependency merely to make an optional feature name look complete.

## PR description contract

Lead with the concrete problem and resulting behavior. For example: “Repeated client reads can retain pins without an admission cap. This change adds a per-tenant read-pin limit and returns a bounded miss/error when the limit is reached.” State only what the code and evidence demonstrate.

Each PR should include:

- Scope and prerequisite decisions, with the exact changed behavior.
- Validation commands and results, including a link/path to raw regression artifacts when produced.
- Compatibility, lifecycle, quota and security implications relevant to the change.
- Hardware not exercised, using existing labels such as `NOT_RUN_HARDWARE`.
- Remaining limitations and a concrete rollback or fallback where behavior changes.

Do not paste secrets, bearer fingerprints, raw prompts or KV bytes into a PR, issue or report. Do not use checkboxes to turn an unavailable run into a pass. Document-only changes can use link/content checks; implementation changes run the appropriate meaningful checks as well as repository-required validation.

## Review gates

Required Rust checks for code changes remain:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Optional feature checks must preserve their existing CPU-only refusal behavior until a separately accepted integration changes it. `dpu-demo`, `paired-baseline` and `v3-paired` returning exit code zero is not a hardware pass. The VM report must preserve the printed unavailable labels.

Use small commits with English messages and rewrite the PR title/body around the final result if scope changes. An unrelated defect should be recorded separately unless it blocks the stated behavior. Each slice should leave a concrete, reviewable result with its evidence limits intact.
