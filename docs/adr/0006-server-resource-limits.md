# ADR-0006: Bound every resource an unauthenticated peer can consume

- **Status:** Accepted
- **Date:** 2026-08-02
- **Owners:** Server, Security
- **Related:** [ADR-0005](0005-in-memory-room-state.md)

## Context

An audit of the server found that a single client, without authenticating,
could exhaust the process by at least three independent routes:

1. **Unbounded frame size** — no cap on an inbound WebSocket message.
2. **Unbounded message rate** — no throttle; operations were accepted as fast as
   they arrived and retained in room state.
3. **Unbounded room creation** — rooms are minted from the URL path, so any
   visitor could allocate a new room per request, none of which were ever
   reclaimed.

Two further issues compounded them. Scope identifiers arrived from a URL path
unvalidated and unbounded in length, then became map keys and log output. And a
consumer that fell behind the broadcast channel would silently terminate its
relay task, which happened to be safe but was neither deliberate nor documented.

Authentication is still absent. That is tracked separately and is a larger piece
of work; it does not justify leaving these open in the meantime, because they
are exploitable *with* authentication too.

## Decision

Every limit lives in one module, `limits.rs`, so the deployed posture is
auditable by reading one file.

| Limit | Value | Rationale |
|---|---|---|
| Frame size | 256 KiB | Above a legitimate batch, far below memory pressure. Enforced at the protocol layer *and* in the read loop |
| Operations per frame | 512 | A batch larger than this is not a drawing client |
| Frame rate | 60/s sustained, 120 burst | The client commits a drag at most every 50 ms (20/s); this leaves headroom for a fast stylus |
| Rate strikes before disconnect | 20 | A transient burst is dropped; a sustained flood closes the connection |
| Elements per room | 50,000 | Beyond the renderer's practical ceiling |
| Rooms | 10,000 | Bounds total process state |
| Scope length | 128 bytes | Bounds map keys |
| Scope charset | `[A-Za-z0-9._:-]` | Removes path traversal and log injection in one step |
| Idle room TTL | 30 minutes | Empty rooms are reclaimed by a periodic sweep |

Rate limiting is **per connection**, not global. A shared limiter would let one
abusive peer degrade every user, which is the failure it exists to prevent.

Two refusals are deliberate in their granularity:

- An oversized batch is refused **whole**. Applying part of it would leave peers
  holding operations the room rejected — divergence introduced by the server.
- The server relays **only what the room accepted**, for the same reason.

Broadcast lag is now handled explicitly: the connection is closed so the client
reconnects and receives a fresh full document. Skipping operations silently is
the one outcome a CRDT cannot repair.

Read endpoints never create rooms. Polling `/api/rooms/{scope}/stats` was
otherwise an allocation vector of its own.

Response security headers are set globally: `nosniff`, `DENY` framing,
`no-referrer`, and a CSP permitting only same-origin resources plus
`'wasm-unsafe-eval'` for the engine.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Wait for authentication, then add limits | One coherent change | Every listed route is exploitable by authenticated users too | Limits are not a substitute for authn, nor authn for limits |
| Reverse proxy handles rate and size | No application code | Does not bound rooms, elements, or scope charset; ties correctness to deployment topology | Application-level invariants belong in the application |
| Global rate limiter | Simpler; one bucket | One abusive peer degrades everyone | Inverts the goal |
| Evict the least-recently-used room when full | Never refuses a connection | Destroys somebody else's board to admit a stranger | Refusing is the honest failure |

## Consequences

### Positive

- No single peer can exhaust memory, and the failure modes are explicit
  refusals rather than degradation.
- The deployed posture is one file.
- Lag handling is now intentional and documented, not incidental.

### Negative and accepted trade-offs

- A legitimate client that exceeds 60 frames/second has frames dropped. The
  current client cannot, but a future one drawing at higher fidelity might.
- At the room cap, new boards are refused outright.
- `frame-ancestors 'none'` blocks iframe-embedding of the demo server. Hosts
  embedding the canvas serve the client themselves and set their own policy.

### Security consequences

This closes the three denial-of-service routes found in the audit. It does
**not** close the absence of authentication, which remains the top security
item: the server still admits every connection.

## Validation

- `scope_charset_is_constrained` — rejects empty, over-long, path-traversal, and
  newline-injection scopes.
- `burst_is_allowed_then_throttled`, `a_sustained_flood_eventually_disconnects`.
- `an_oversized_batch_is_refused_whole` — asserts no partial application.
- `an_occupied_room_is_never_reclaimed`, `an_empty_idle_room_is_reclaimable`.

## Revisit triggers

- Authentication lands, at which point per-identity budgets replace or
  supplement per-connection ones.
- Durable persistence lands, changing what "room memory" means.
- A legitimate client is observed hitting the frame-rate ceiling.
- The server is deployed behind infrastructure with its own limits, requiring
  the two layers to be reconciled rather than stacked.
