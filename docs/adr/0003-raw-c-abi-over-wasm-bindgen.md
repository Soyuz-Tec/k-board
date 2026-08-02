# ADR-0003: Expose a raw C ABI rather than generated wasm bindings

- **Status:** Accepted
- **Date:** 2026-08-02
- **Owners:** Engine, Integration
- **Related:** [ADR-0004](0004-global-registry-and-side-buffer.md)

## Context

The engine must be callable from a browser *and* from native host runtimes —
the BEAM via Rustler, CPython via PyO3, Node via napi-rs, the JVM via Panama,
.NET via P/Invoke. Being embeddable in a host process is the reason the project
exists; the browser is one host among several, not the privileged one.

`wasm-bindgen` is the default choice for the browser. It generates JavaScript
glue, requires `wasm-pack` or `wasm-bindgen-cli` in the build, and produces an
interface only JavaScript can use. A native host would need a second, separately
maintained interface — and two interfaces over one engine is two places for the
contract to drift.

## Decision

One `#[no_mangle] extern "C"` export set, in a dedicated `kboard-ffi` crate,
serving every host including the browser.

The browser loads the `wasm32` build with `WebAssembly.instantiateStreaming` and
a hand-written binding of roughly 120 lines (`web/kboard.js`). Native hosts load
the `cdylib` or link the `staticlib`. Both call the same twelve functions.

Consequently the toolchain is `cargo` alone. No `wasm-pack`, no
`wasm-bindgen-cli`, no bundler, no npm dependency in the client.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| `wasm-bindgen` for web, separate C ABI for native | Ergonomic JS; idiomatic types | Two interfaces over one engine; two contracts to keep in step | Drift between them is the bug class this project is built to avoid |
| `wasm-bindgen` only | Best browser ergonomics | No native host can call it | Forfeits the entire embedding thesis |
| WASI component model | Standards-track; strong typing | Immature host support in BEAM/JVM/CPython today | Not yet available where it is needed |
| gRPC/HTTP to a sidecar | Language-agnostic | A network hop and a second process in every host | Defeats in-process embedding |

## Consequences

### Positive

- One contract. The browser and a BEAM NIF exercise the same exports, so a
  binding bug surfaces everywhere rather than in one host.
- The published wasm has **no imports** and needs no glue file to load.
- Build is `cargo build --target wasm32-unknown-unknown`. Nothing else.

### Negative and accepted trade-offs

- The JavaScript binding is hand-written and must be maintained alongside the
  ABI. It is small, but it is not generated.
- Callers marshal JSON strings rather than passing structured values. This costs
  a serialise/parse per call.
- No automatic TypeScript types. A `.d.ts` must be written by hand if wanted.

### Operational consequences

Hosts must check `kb_abi_version()` on load. The wasm and native artifacts must
always come from the same build; mixing versions is undefined.

## Validation

- The wasm artifact instantiates with an empty import object and exports all
  twelve functions plus `memory`.
- `scripts/two-replica-check.mjs` drives the engine from **Node**, reusing
  `web/kboard.js` unchanged — demonstrating a second host with no code changes.
- FFI unit tests cover malformed input, null pointers, unknown handles, and
  double close.

## Revisit triggers

- The WASI component model gains usable support in the target host runtimes.
- JSON marshalling is measured as a material cost in the interaction hot path.
- A host requires zero-copy access to scene data rather than a serialised copy.
