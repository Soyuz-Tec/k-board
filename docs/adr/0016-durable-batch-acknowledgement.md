# ADR-0016: Acknowledge durable idempotent batches, not socket sends

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Protocol, Client, Server, Storage
- **Related:** [ADR-0007](0007-sqlite-durable-storage.md), [ADR-0015](0015-per-scope-room-cells.md)

## Context

The browser drains engine operations into an in-memory outbox, calls
`WebSocket.send`, and immediately clears the outbox. The protocol has no batch
identifier or acknowledgement. `send` only transfers bytes to a browser buffer;
it does not prove that the server validated, persisted or applied them. A close
before server commit loses retry intent. A close after commit cannot be
distinguished from a close before commit, so blind retry has no explicit
deduplication contract.

The server's stated durability rule — writes are durable before broadcast — is
therefore not a complete end-to-end user guarantee.

## Decision

Every operation command in the next protocol version carries an opaque stable
batch ID under an authenticated scope and replica identity. The store appends it
idempotently and returns a durable log sequence. The server sends
`Ack(batch_id, durable_sequence)` only after successful validation and durable
append. It then applies and broadcasts the prepared batch in the room cell's
ordered command path.

A duplicate `(scope, replica_id, batch_id)` returns its original successful
acknowledgement without duplicating durable effects. Reusing a batch ID with
different content is an integrity violation and is refused.

The client retains a batch — eventually in persistent storage — until the
matching durable acknowledgement arrives. Socket close, reconnect and reload do
not change its identity. Refusal and overload are explicit outcomes and do not
masquerade as acknowledgement.

Protocol negotiation protects mixed-version rollout. Version 1 remains a
documented compatibility surface until its removal window is decided; the new
server must not silently assign version 2 durability semantics to a version 1
client.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Treat `WebSocket.send` as success | No protocol work | Says nothing about validation or durability | Factually false delivery guarantee |
| Retry raw operations without batch identity | CRDT operations are often idempotent | Log duplication, ambiguous outcomes and no batch-level refusal identity | Convergence is not a delivery protocol |
| Ack after in-memory apply | Low latency | Ack can be lost on process crash before append | Violates acknowledged durability |
| Durable idempotent batch ack | Explicit outcome and safe retry | Schema, client and compatibility complexity | **Chosen** |

## Consequences

### Positive

- “Saved” has a precise end-to-end definition.
- Commit-before-disconnect is safe to retry.
- Duplicate retry does not inflate the log or rebroadcast as new work.
- The client can show local-only, sending, durable and refused states honestly.

### Negative and accepted trade-offs

- Batch identity and deduplication records consume storage and need a retention
  horizon consistent with maximum offline retry.
- Persistent browser outbox behavior introduces quota and privacy concerns.
- Ack latency includes durable storage latency by design.
- The protocol needs version negotiation and mixed-client tests.

### Security consequences

Batch IDs are opaque, not authorization. Deduplication keys include authorized
scope and replica identity. Error responses do not reveal whether another
replica used a guessed ID.

## Validation

- Close before append: no ack; retry commits once.
- Close after append but before ack: retry receives the original sequence.
- Repeat an acknowledged batch: no duplicate log row, mutation or peer event.
- Reuse a batch ID with changed bytes: refuse it.
- Reload the browser before ack: persisted outbox retries the same ID.
- Mixed protocol versions negotiate or fail explicitly.

## Revisit triggers

- The transport changes to one with equivalent application delivery receipts.
- Dedupe retention becomes materially larger than operation history.
- End-to-end encryption requires dedupe over opaque payloads.
