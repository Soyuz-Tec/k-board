# ADR-0019: Carry the engine's u64 actor through the C ABI

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Engine, Integration, Protocol
- **Related:** [ADR-0003](0003-raw-c-abi-over-wasm-bindgen.md), [ADR-0018](0018-replica-identity-and-hlc-trust.md)

## Context

`ActorId` is `u64`, and element IDs reserve their high 64 bits for it, but
`kb_open` accepted `u32`. The browser also truncated with `actor >>> 0`. That
adapter mismatch prevented a collision-resistant stable actor and silently
changed any host value above `u32::MAX`.

Protocol v2 derives non-zero 53-bit actors so they round-trip exactly through
JSON and JavaScript. The wasm C ABI represents Rust `u64` as an i64 parameter,
which JavaScript supplies as `BigInt`.

## Decision

Change `kb_open(..., actor)` from `u32` to `u64` and increment the ABI version
from 2 to 3. The JavaScript binding validates a non-zero safe integer and passes
`BigInt(actor)` to wasm. Native hosts use the ordinary `uint64_t` equivalent.

The wire actor remains at most `Number.MAX_SAFE_INTEGER`; the engine and native
hosts retain the full `u64` type. Hosts must reject an ABI version mismatch, as
already required by ADR-0003.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Keep u32 and accept collision risk | No ABI break | Material collision probability and existing silent truncation | Violates stable replica identity |
| Pass actor as UTF-8 text | No wasm BigInt | Parsing on every open and a different shape from native integer APIs | Adds boundary complexity without value |
| Accept u64 / wasm BigInt | Matches engine and C hosts; exact | ABI bump and BigInt call in JavaScript | **Chosen** |

## Consequences

- Existing native bindings must recompile/update their declaration.
- Old wasm and new JavaScript, or the reverse, fail loudly on ABI version.
- Actor values used in JSON remain within the exact 53-bit range.

## Validation

- Open through the ABI with an actor greater than `u32::MAX` and verify the
  resulting element ID keeps that actor prefix.
- Build and exercise native and wasm targets.
- Verify ABI mismatch remains a startup failure.

## Revisit triggers

- The wire format moves away from JSON numbers.
- The component model replaces the raw C/wasm ABI.
