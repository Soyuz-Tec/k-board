# ADR-0004: Hold boards in a process-global registry and return results via a side buffer

- **Status:** Accepted
- **Date:** 2026-08-02
- **Owners:** Engine, Integration
- **Related:** [ADR-0003](0003-raw-c-abi-over-wasm-bindgen.md)

## Context

Two mechanical decisions at the FFI boundary have consequences disproportionate
to their size, and both are invisible from the call site.

**Where board state lives.** Thread-local storage is the obvious choice and is
wrong for the primary target. A BEAM NIF is invoked from whichever scheduler
thread happens to run the calling process, and dirty NIFs run on a different
thread again. A board opened on one thread would be invisible on the next call.
The JVM and .NET thread pools behave the same way.

**How results come back.** A C function returns one value. Returning both a
pointer and a length means packing them, which works on `wasm32` (32-bit
pointers) and breaks on 64-bit native — so the signatures would have to differ
per target, defeating ADR-0003.

## Decision

**Boards live in a process-global registry** behind a `Mutex`, keyed by an
opaque `u32` handle. Handles, not pointers, cross the boundary: a host cannot
manufacture a dangling reference, and a stale handle yields `STATUS_NO_BOARD`
rather than undefined behaviour.

Mutex poisoning is recovered from with `into_inner()` rather than propagated. A
panic in one call must not permanently disable the library for a host process
that may live for months.

**Results are published to a per-thread buffer**, read by `kb_last_ptr()` and
`kb_last_len()`. Functions return only a status code. The buffer is
thread-*local* even though the registry is global, so two concurrent callers
cannot overwrite each other's output.

Every export is wrapped in `catch_unwind`. A panic unwinding into a foreign
runtime does not fail one call — it can abort the host process. This is also why
the release profile deliberately does not set `panic = "abort"`.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Thread-local board storage | No lock; no contention | A board opened on one thread is invisible on another | Breaks on the BEAM, the primary embedding target |
| Return `*mut Board` directly | No registry; no lookup | Host can hold a dangling pointer past close; use-after-free in *their* process | Unacceptable in code linked into a customer's runtime |
| Pack pointer and length into `u64` | One return value | Only valid where pointers are 32-bit | Would require different signatures per target |
| Caller-supplied output buffer | No side channel; explicit | Caller must size it in advance or retry on overflow | More boundary crossings and more ways for a host to get it wrong |

## Consequences

### Positive

- One board is usable from any thread of any host runtime.
- Handles are unforgeable in practice and fail safe when stale.
- Identical signatures on `wasm32` and 64-bit native.

### Negative and accepted trade-offs

- A global lock serialises all board operations. Acceptable at current
  granularity; a per-board lock is the escape hatch if it becomes a bottleneck.
- Callers must read the result buffer **before** their next call on that thread.
  This is documented but not enforceable by the type system.
- Closed handles are never reused, so the counter grows monotonically. At `u32`
  that is 4 billion opens per process lifetime.

### Security consequences

`catch_unwind` at every export is the difference between a malformed input
failing one call and taking down a host node. Combined with
`#![forbid(unsafe_code)]` in `kboard-core`, the auditable unsafe surface is one
small file.

## Validation

- `two_handles_sync_through_pending_and_merge` — independent boards in one
  process.
- `malformed_input_is_refused_not_fatal` — the library stays usable after bad
  input.
- `null_and_unknown_handles_are_refused`, `allocation_round_trips`.

## Revisit triggers

- Lock contention is measured under a realistic multi-board host.
- A host requires concurrent operations on the *same* board from several threads.
- `catch_unwind` proves insufficient for a host whose runtime cannot tolerate
  unwinding at all, in which case the boundary needs an abort-free design.
