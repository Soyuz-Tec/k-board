# ADR-0010: Presence is relayed and forgotten, never merged

- **Status:** Accepted
- **Date:** 2026-08-04
- **Owners:** Server, Client
- **Related:** [ADR-0001](0001-per-property-convergence.md), [ADR-0005](0005-in-memory-room-state.md), [ADR-0006](0006-server-resource-limits.md), [ADR-0007](0007-sqlite-durable-storage.md)

## Context

Seeing where a collaborator is pointing is most of what makes a shared canvas
feel shared. It is also the first piece of state this project has encountered
that is genuinely *not* part of the document.

Everything built so far has one shape: an edit becomes an operation, the
operation merges, the merged result persists. Applying that shape to a cursor
would be the obvious move and it is the wrong one — obvious enough that this
record exists mainly to stop someone restoring it later.

A cursor arrives tens of times a second, per user. It is interesting only while
its owner is connected. And it has no meaning at all after the fact.

## Decision

**Presence is a separate message type end to end.** It never reaches the engine,
never enters the operation log, and is never persisted.

```
ClientMessage::Presence { x, y }  →  server  →  ServerMessage::Presence { actor, x, y }
                                                ServerMessage::Left { actor }
```

Routing a cursor through the CRDT would make every mouse movement a durable
write (ADR-0007), grow the log without bound, and leave the pointer of someone
who closed their laptop in the board forever.

**The server stamps the actor id; it does not accept one.** A connection that
could name an actor could move somebody else's cursor. The client sends
coordinates and nothing else.

**Presence does not take the room lock.** The broadcast handle is captured at
join and used directly. Routing ~17 messages/second/user through the single
`Mutex<ServerState>` would make every room contend on every other room's mouse.

**Departure is announced, not left to time out.** A cursor that lingers after
someone closes a tab reads as a colleague who is still there. The ten-second
expiry stays as a backstop for what an announcement cannot cover: a connection
that died without a close frame, or a relay dropped for lagging (ADR-0005).

**Cursors are throttled by movement as well as by time.** A stationary pointer
produces no traffic at all. This matters because the rate budget from ADR-0006
is now *shared*: drawing at 20/s plus cursors at ~17/s is about 37/s against a
60/s cap. `limits.rs` states that arithmetic rather than leaving the next person
to wonder why the cap is 60.

**Peer colours are derived, not assigned.** A golden-angle rotation of the actor
id, so every replica independently agrees what colour someone is without the
server allocating or remembering anything, and sequential connections land far
apart on the wheel.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Cursor as element properties in the document | Zero new machinery; converges and replicates for free | Every mouse movement becomes a durable write; the log grows without bound; a disconnected user's cursor persists forever | The failure is silent — it works perfectly *and* records everything |
| A second, separate CRDT for presence | Principled; supports offline presence | A whole convergence machine for state whose correct behaviour on partition is "forget it" | Machinery in service of a property nobody wants |
| An ephemeral element kind the store skips | Reuses the element pipeline | Every consumer must now know which kinds are real; the exception spreads | An exception in the data model is worse than a separate channel |
| Server holds a presence map per room | Late joiners see cursors immediately | Puts presence back under the room lock and adds state to reclaim | Cursors are stale within 60ms; there is nothing worth catching up on |
| Client supplies its own actor id | One less server concern | Any client can move any other user's cursor | Trivially spoofable |
| A separate rate budget for presence | Drawing and pointing cannot starve each other | Two budgets to reason about and tune, for a combined load well under one cap | One documented budget is auditable; two are a system |

## Consequences

### Positive

- The document stays a document. Nothing in the log is there because a mouse
  moved.
- Presence costs the server no lock, no allocation in the room, and no storage.
- Losing all of it costs one repaint, which is why the failure modes are dull.
- A host embedding the engine can ignore presence entirely, or implement its
  own, without touching the document path.

### Negative and accepted trade-offs

- **A late joiner sees nobody until they move.** No presence history exists to
  replay. In practice a second of mouse movement fixes it, and the alternative
  is state to hold and reclaim.
- **Presence is lost on reconnect** and the client clears it deliberately —
  leaving cursors on screen would show a room full of people who cannot see
  you.
- The rate budget is shared, so a client that changed its cursor cadence could
  starve its own drawing. The arithmetic is documented in one place precisely
  because it is now a relationship rather than a number.
- Fan-out is O(peers) per cursor message. Fine at room scale; the first thing
  to hurt if a room ever holds hundreds.

### Operational consequences

Presence messages are not logged and not counted in room stats. `accepted` in
`/api/rooms/{scope}/stats` therefore does *not* move when cursors fly, which is
exactly the property the check asserts.

### Security consequences

The actor id is server-assigned, so a cursor cannot be attributed to another
connection. Coordinates are checked for finiteness before relay. Presence
carries no user identity — only the actor number the server allocated — so it
leaks nothing the connection had not already revealed by joining.

Presence does share the ADR-0006 rate budget, which means it is one more way to
spend it; it does not add a new unbounded resource.

## Validation

`scripts/presence-check.mjs` in CI. The interesting assertions are **negative**:

```
PASS  a hundred cursor reports accept no operations
PASS  and add no elements to the board
PASS  a real edit does move the counter presence left alone
```

That third line is the control. Without it the first two would pass just as
happily against a server that records nothing at all — which is the failure the
check would be least likely to notice on its own.

Also covered: relay fidelity, actor spoofing, no self-echo, a malformed frame
that must not end the session, departure announcement, and the pure client-side
throttle, expiry and colour derivation.

## Revisit triggers

- Shared selection or follow-mode, which is presence-shaped but larger and may
  justify a real presence channel with state.
- Named users rather than actor numbers, which introduces identity into a
  channel that currently carries none.
- Rooms large enough that O(peers) fan-out per cursor is measurable.
- A host needs presence to survive a reconnect, which would reopen whether it
  can stay stateless.
