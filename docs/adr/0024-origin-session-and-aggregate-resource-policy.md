# ADR-0024: Enforce origin, session expiry and aggregate scope/identity budgets

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Security, Server, Web, Operations
- **Related:** [ADR-0006](0006-server-resource-limits.md), [ADR-0008](0008-scoped-bearer-tokens.md), [ADR-0018](0018-replica-identity-and-hlc-trust.md)

## Context

Per-connection rate limiting can be multiplied with many connections. Browser
WebSocket handshakes carry an `Origin` but the server does not validate it.
Bearer expiry is checked only when the socket opens, captured query tokens stay
in browser history, and document/snapshot bytes have no aggregate ceiling.

## Decision

Browser WebSocket origins are exact-match allowlisted. Loopback development
defaults to the server's localhost and loopback origins. A non-loopback bind
requires both authentication and an explicit comma-separated
`KBOARD_ALLOWED_ORIGINS` allowlist. Handshakes without `Origin` remain available
to non-browser clients that present the same bearer/authorization controls.
WebSocket extensions, including compression, are not negotiated. Offers are
ignored because browsers send them automatically and do not let application
code disable them. The upgrade response must never select an extension until
bounded decompression has its own decision and tests.

The browser removes `token` from `location.search` with `history.replaceState`
as soon as the module starts, retaining it only in memory for the handshake.
The query form remains a bootstrap compatibility surface. Fragments conflict
with the current scope selector, cookies would introduce CSRF and same-site
deployment policy, and a new exchange endpoint would add replay state. A future
short-lived bootstrap exchange is preferred when the standalone UI gains an
authentication page.

Signed grants are revalidated on every reconnect. Their expiry becomes a live
session deadline: the server closes the socket when the grant expires. Pending
v2 batches remain client-owned and may be retried after a fresh grant using the
same replica and batch identities. No token, signature, SQL text, scope string
or document content is emitted in public error bodies or metric labels.

Resource governance is layered: per connection, aggregate replica identity,
aggregate scope and process-wide connection limits; at most 64 connections per
scope; bounded room and storage mailboxes; four simultaneous restores; a
32-MiB materialized document payload estimate; and a 64-MiB serialized
snapshot/restore ceiling. These are operational safety ceilings, not product
entitlements. Scope correlation in logs uses a fixed SHA-256 prefix.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Accept every Origin because tokens are explicit | Simple | Enables cross-site socket use when a bearer leaks into hostile page state | Fails browser trust-boundary defense in depth |
| Put bearer in a cookie | Browser-native | Adds CSRF, SameSite and host-integration policy | Wrong implicit authority for this standalone adapter |
| Keep only per-connection limits | Smallest state | Attackers multiply budgets with sockets | Does not bound identity or scope work |
| Exact origins, expiring sessions and layered budgets | Bounded and auditable | Requires deployment configuration and shared counters | **Chosen** |

## Consequences

- Public deployments must configure allowed browser origins explicitly.
- Long-lived clients reconnect with a fresh grant rather than remaining
  authorized forever.
- Aggregate limiter state is bounded by admitted active connections and removed
  on release.
- Oversized durable state fails closed; operators must repair or archive it
  rather than loading it into unconstrained memory.

## Validation

- Unit and raw-handshake tests cover missing, allowed, hostile and malformed
  origins, and prove offered WebSocket extensions are not negotiated.
- Live auth tests prove tokens stay out of negotiated protocols, response
  bodies and server logs, and expiry closes an active socket.
- Many-connection tests prove per-scope and per-identity aggregation.
- Cross-scope, lagging-subscriber, reconnect-storm and snapshot-size tests prove
  isolation and bounded failure.

## Revisit triggers

- The UI gains a first-party login/bootstrap endpoint.
- A reverse proxy supplies a separately authenticated Origin policy.
- Compression is required and decompression budgets are defined.
- Measured legitimate usage regularly approaches an aggregate ceiling.
