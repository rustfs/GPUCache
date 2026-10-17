## Change

Describe the concrete problem and resulting behavior. Link the relevant ADR or roadmap stage.

## Validation

List the commands actually run and their results. Link regression artifacts when available. A stub returning zero is not a hardware pass.

## Compatibility and failure behavior

Describe affected identity/layout versions, tenant boundaries, admission limits, buffer/pin lifecycle, and miss/recompute behavior where relevant.

## Evidence limits

List unrun device/engine checks explicitly (`NOT_RUN_HARDWARE` where applicable). Keep `TCO_UNKNOWN` and `NOT_EVALUATED` until paired evidence exists. Distinguish CPU simulation, VM execution, real hardware and proposed work.

## Scope and rollback

State remaining limitations and the rollback/fallback for changed behavior. GPUCache does not add hot-tier erasure coding or durability replicas. Do not include credentials, prompts or KV payloads in this PR.
