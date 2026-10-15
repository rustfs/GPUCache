# ADR 0003: Partial prefix reuse is a whole stored block

- Status: **Accepted**
- Date: 2026-10-15
- Milestone: V1 B

## Context

A shared KV lookup often asks for a token prefix longer than the blocks already stored. Returning a suffix of a stored block, or bytes that correspond to a longer chain than the caller asked for, would mis-align decode. V1 A only hit on an exact physical key.

## Decision

V1 B returns three lookup results:

- `Hit` — the physical key matches a committed block. The bytes are that whole block.
- `PrefixHit` — a committed block's hash chain is a strict prefix of the query, and tenant, model, tokenizer, adapter, dtype, quant, layout, shard, RoPE, and block token count match. `matched_prefix_blocks` is the stored chain length. `Get` must use `identity_for_prefix_hit`. A `Get` with the longer query identity is `Incompatible`.
- `Miss` — including a same-chain identity whose `valid_length` differs. That case is not a sliced block. It stays a miss.

The service does not cut a payload down to a shorter `valid_length`. If a caller needs bytes for a prefix that was not stored as its own block, the result is a miss and the engine recomputes.

## Consequences

- `share-demo` prints `partial_prefix=WHOLE_BLOCK_PREFIX` and `intra_block_slice=DEGRADED_MISS`.
- A wrong-layout or wrong-tenant query never becomes a `PrefixHit`.
- Engines must branch on the variant. Treating every success as the full query identity is a client bug.
