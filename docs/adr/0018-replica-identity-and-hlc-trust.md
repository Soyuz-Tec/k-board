# ADR-0018: Bind durable operations to a stable replica and governed HLC policy

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Engine, Protocol, Security, Client
- **Related:** [ADR-0001](0001-per-property-convergence.md), [ADR-0016](0016-durable-batch-acknowledgement.md)

## Context

Actor IDs participate in element IDs and hybrid logical clock tie-breaking. The
standalone server currently allocates a connection actor from a process-local
counter that resets on restart. Offline browser mode can allocate a random
32-bit actor. Durable operations carry client-supplied actor and HLC fields, but
the server does not bind them to the connection or an authenticated replica.

An authorized malicious peer can therefore impersonate another actor or send a
far-future wall value that dominates last-writer-wins properties. Simply
requiring the current connection actor would reject legitimate offline work
created before reconnect. This must be resolved before durable batch replay is
presented as a secure protocol.

## Decision

Protocol v2 uses a 128-bit random replica identity encoded as exactly 32
lowercase hexadecimal characters. It is scoped to one browser tab/host replica,
not to a human. The browser initially keeps it in `sessionStorage`, which
survives reload in that tab; Gate 7 persists it with pending batches in
IndexedDB. A replica rotates only when it has no pending batch, or after pending
work is explicitly exported/discarded during logout.

The actor is a deterministic SHA-256 derivation over a domain separator, opaque
scope and full replica identity, truncated to a non-zero 53-bit integer. Scope
binding prevents one replica identity from sharing an actor across tenants. The
53-bit range is exact in JSON/JavaScript and gives negligible accidental
collision probability at the room limit; grinding a chosen collision is not
practical. All durable operation stamps must carry that derived actor. A newly
materialized element ID must also carry it in the high 64 bits; edits to an
existing element may retain its creator's prefix.

The C ABI accepts the engine's existing `u64` actor rather than truncating it to
`u32`; ADR-0019 governs that ABI revision. On loading a document, a replica
advances its local element counter past every element ID carrying its actor, so
reload cannot recreate an old ID. Two simultaneous sessions may not claim the
same `(scope, replica)` identity.

The server refuses the whole batch when a stamp actor differs from the derived
actor or when an HLC wall value is more than five minutes ahead of server wall
time. It never restamps accepted operations: doing so would change their CRDT
identity. Accepted stamps are observed through the normal document maximum and
client merge behavior; the server uses physical time only for the upper bound.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Current assessment |
|---|---|---|---|
| Trust all client actor/HLC fields | Works offline without coordination | Actor forgery, collision and future-clock domination | Reject |
| Use connection actor only | Simple authorization | Breaks offline/reconnect batches; resets on restart | Reject |
| Server restamps every operation | Server controls time/actor | Changes operation identity and offline convergence semantics | Needs proof; not preferred |
| Stable authorized replica + governed HLC | Preserves offline authorship with enforceable trust | Lifecycle and skew complexity | **Chosen** |

## Consequences

### Positive

- Operation authorship can be checked rather than inferred.
- Server restart does not intentionally reuse actor space.
- Batch deduplication has a stable principal.
- Future-clock abuse has an explicit outcome.

### Negative and accepted trade-offs

- Browser identity persistence can be cleared, copied or unavailable.
- A 53-bit actor is smaller than the engine's `u64`; the full replica ID remains
  the security and deduplication principal.
- Strict clock-skew limits can reject edits from badly configured devices.
- Long-offline replicas interact with dedupe and tombstone-retention horizons.

### Security consequences

Replica identity must be authorized but must not become a bearer secret logged
or exposed as a raw metric label. A human user may own multiple replicas.

## Validation

- Restart and reconnect retain a non-colliding actor mapping.
- Offline batches created before reconnect remain admissible.
- Forged actors and unbounded future clocks are refused atomically.
- Two browser tabs/profile clones have documented identity behavior.
- Batch deduplication retains `(scope, replica, batch)` outcomes for 30 days;
  longer-offline clients must resync before rebatching.

## Revisit triggers

- A host supplies its own globally stable replica/actor assignment.
- The engine changes element IDs or abandons HLC-based ordering.
- End-to-end encryption prevents the server from inspecting operation stamps.
