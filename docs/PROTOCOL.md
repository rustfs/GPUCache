# Protocol (wire version 1)

Request frames are unchanged from V1 A. V1 B adds reply frames and a TCP envelope. The TCP listener speaks the V1 A request framing as the envelope body.

## Framing

Multi-byte integers are little-endian. Byte strings are `u32` length plus bytes. Decoders reject truncated buffers, trailing bytes, unknown versions, and unknown TLV tags. Declared lengths above 64 MiB are rejected before allocation. Store quotas are tighter than that cap.

Request frame:

| Field | Contents |
| --- | --- |
| magic | `GCK1` |
| version | `u16` = 1 |
| op | `u16` (1 lookup, 2 reserve/put, 3 commit, 4 get, 5 release/cancel, 6 invalidate) |
| auth | tenant, trust domain, 32-byte credential fingerprint |
| items | `u32` count, then items |

Hello (`op` 7) carries the versions the peer supports and no tenant data. HelloAck (`op` 8) returns the selected version and the store epoch. `negotiate` picks the highest version present on both sides and errors on an empty overlap.

Export frame (`GEXP`, version 1) carries the identity body, source generation, SHA-256 checksum, and payload. Decode recomputes the checksum and fails closed on mismatch. The credential is not in the frame; the importer supplies it out of band.

## Item headers

Every item has:

- `request_id` — 16 bytes, not all zeros
- `epoch` — must equal the store epoch
- `deadline_ms` — absolute monotonic deadline; `0` is malformed; `now >= deadline` is `TimedOut`

Mutating ops are idempotent inside a bounded window. The same `request_id` and the same canonical arguments replay the stored result. The same id with different arguments is `IdempotencyConflict`. Lookup and Get are not logged; they are safe to repeat. A deadline miss before any mutation is not logged, so the caller can retry that id with a later deadline.

## Operations

**Lookup.** Identity in, hit metadata or a miss out. No payload. Miss reasons include absent, not committed, incompatible, evicting, and quarantined.

**Reserve/Put.** `generation == 0` creates a reservation and, when a payload is present, moves it to `WRITING`. A non-zero generation continues an existing `RESERVED` slot. Checksum is declared here and checked at commit. Bytes are not readable yet.

**Commit.** Generation, length, and checksum must match the stored write. The checksum is recomputed. Mismatch deletes the write and returns an error. Success moves the block to `COMMITTED`.

**Get.** Returns payload only for a visible committed block whose identity matches and whose bytes still hash to the stored checksum. A bad checksum quarantines the region and returns no bytes. A successful get pins the region until release or a fenced drop.

**Release/Cancel.** With no pin owner, cancels an in-flight reserve/write. `Drained` reclaims. `Unknown` and `KnownIdle` quarantine a registered region. With a pin owner, releases that reader pin under the same rules.

**Invalidate.** `COMMITTED` becomes `EVICTING` and is immediately unreadable. The buffer is freed only after reader pins drain. If a pin's completion is unknown, freeing waits for a newer epoch.

## Deadlines and leases

Request deadlines abort the call. Writer leases move `RESERVED` with no buffer back to `ABSENT`, and move `WRITING` to `QUARANTINED`. A reader lease expiry keeps the pin and blocks buffer reuse. It does not, by itself, hide a checksum-valid committed block from a new reader, and it does not free memory.

## Compatibility

A hit requires the physical key and a `COMMITTED` block. Logical-key overlap with a different layout, dtype, quant, or shard is a miss.

`PrefixHit` is a whole stored block whose hash chain is a strict prefix of the query and whose layout-affecting fields match. `matched_prefix_blocks` is the stored chain length. `Get` uses `identity_for_prefix_hit`. The same chain with a different `valid_length` is an incompatible miss: the service does not return a sliced payload. See [adr/0003-partial-prefix.md](adr/0003-partial-prefix.md).

## Replies

Reply magic is `GCKR`, version 1. `op` matches the request (`1` through `6`). `op` `0` is a batch-level failure and carries the request op, the store epoch, and an error code. Item status `1` is success and `0` is an item error. Lookup success tag `1` is an exact hit, `2` is `PrefixHit`, `3` is a miss. Static error strings are not on the wire; decoded remote errors use the reason `remote`.

## TCP envelope

Each TCP message is:

| Field | Contents |
| --- | --- |
| payload_len | `u32` little-endian, includes the clock field |
| server_now_ms | `u64` little-endian. Clients send `0`. The server sends its monotonic clock |
| body | a `GCK1` request or hello, or a `GCKR` reply or hello ack |

Deadlines are absolute on the server clock. `TcpClient` samples `server_now_ms` from the hello ack and from each reply. A length above the configured frame cap is rejected before the body is read. The server read-buffer pool does not grow past its capacity.
