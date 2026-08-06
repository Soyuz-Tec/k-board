# ADR-0030: One web client for standalone and embedded delivery

- **Status:** Accepted
- **Date:** 2026-08-06

## Context

k-board is both a standalone whiteboard application and an embeddable
whiteboard for K-Comms or another host. Forking the client would let drawing,
accessibility, recovery, protocol, and durability behavior drift. Loading the
standalone page directly in an arbitrary frame would instead expose navigation
and bootstrap assumptions that belong to the standalone shell.

The first public web integration also needs an independently deployable
boundary. A host must be able to upgrade its own framework without importing
k-board's DOM ownership or WebAssembly lifecycle into the host process.

## Decision

Ship one browser client in two modes:

- `/` is the standalone shell. It derives its scope from the URL and may accept
  the existing short-lived query bootstrap before removing it from history.
- `/embed` is the embedded shell. It waits for a versioned initialization
  handshake and never reads a bearer from its URL or DOM attributes.

`embed-sdk.js` exposes a framework-neutral `<k-board>` Web Component and a
`KBoard.mount(container, options)` helper. The component owns a sandboxed iframe
that loads `/embed`. A transferred `MessagePort` carries the bounded lifecycle
contract. Initial configuration contains the opaque scope and optional
scope-bound grant. Later messages contain lifecycle requests and non-secret
status events; the grant is never echoed.

The standalone and embedded shells load the same `app.js`, WebAssembly engine,
outbox, renderer, protocol, and room-cell server. Embedded mode changes only
bootstrap and host lifecycle. K-Comms-specific identity, navigation, and policy
remain in a K-Comms adapter outside `kboard-core`.

The server permits `/embed` to be framed by `'self'` and by exact HTTP(S)
origins configured through `KBOARD_EMBEDDING_ORIGINS`. Every other page retains
`frame-ancestors 'none'` and `X-Frame-Options: DENY`. The WebSocket grant,
scope authorization, origin policy, and resource controls remain authoritative.
Only the public SDK module, its contract dependency, and component stylesheet
receive cross-origin resource headers; no authenticated API response does.

Contract version 1 supports:

- initialization with `scope`, optional `accessToken`, and a transferred port;
- `ready`, `status`, `error`, and `openStandalone` events;
- `focus` and `flush` requests;
- explicit destruction by the host.

## Alternatives considered

| Alternative | Decision | Reason |
|---|---|---|
| Maintain separate standalone and embedded clients | Rejected | Behavior and durability semantics would drift |
| Import the editor directly into every host DOM | Deferred | Stronger coupling to host frameworks, CSS, globals, and upgrade cadence |
| Let any origin frame the standalone page | Rejected | It mixes shells and removes explicit framing authority |
| Put the grant in the iframe URL | Rejected | URLs leak into history, logs, screenshots, and referrers |
| Native FFI only | Retained as an additional path | It suits BEAM/native hosts but does not provide a browser integration surface |

## Consequences

Positive:

- standalone and embedded users run identical editing and recovery code;
- hosts receive a small framework-neutral integration contract;
- iframe lifecycle and release isolation make independent deployment practical;
- exact framing origins remain an operator decision.

Negative and accepted trade-offs:

- iframe focus, clipboard permissions, sizing, and accessibility need explicit
  integration tests;
- the initial web SDK cannot offer zero-copy in-process renderer access;
- host and embedded client versions require negotiation and clear refusal.

Operational:

- the release artifact must contain the SDK, contract module, embedded demo,
  stylesheet, and shared web client;
- cross-origin hosts must be listed exactly in
  `KBOARD_EMBEDDING_ORIGINS`;
- authenticated hosts mint short-lived grants for the requested scope and pass
  them only through initialization.

Security:

- frame permission does not grant board permission;
- initialization is accepted only from the actual parent window, once, and on
  the transferred private channel thereafter;
- tokens are neither persisted by the SDK nor emitted in lifecycle events.

## Validation

- contract unit tests cover scope, URL, message, and option validation;
- Rust tests cover exact embedding-origin parsing and the generated CSP;
- a rendered host demo mounts, focuses, flushes, destroys, and remounts a board;
- the demo also mounts from a separate allowed host origin, exercising module
  CORS, framing policy, and the private initialization channel;
- the same scope opened standalone and embedded converges through protocol v2;
- release assembly verifies all required web assets.

## Revisit triggers

- measured iframe overhead breaches the interactive SLO;
- a host requires offline use without the hosted embedded document;
- multiple hosts require direct DOM composition or native framework bindings;
- lifecycle contract v1 cannot add a capability without ambiguity.
