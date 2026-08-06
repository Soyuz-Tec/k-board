# Protocol compatibility and delivery contract

## Versions

| Version | Negotiation | Client operations | Durable outcome |
|---|---|---|---|
| v1 | No `kboard.v2` WebSocket subprotocol | `{type:"ops", ops:[...]}` | No application acknowledgement; compatibility only |
| v2 | `kboard.v2` selected, then `hello` | `{type:"ops", batch, ops:[...]}` | `ack(batch, sequence)` or typed `refused` |

Version 1 fixtures are immutable under
`crates/kboard-server/fixtures/protocol/v1`. The server supports v1 throughout
the room-cell migration. Removal requires a superseding ADR, two consecutive
releases (and at least 90 days) with no observed v1 traffic, and an explicit
upgrade path. A v1 client never receives a v2 acknowledgement promise.

Unknown object fields are ignored for forward-compatible additive evolution.
Unknown message types, invalid identifiers and messages from the wrong protocol
state are refused or dropped without panicking. A v2 connection must send
`hello(version=2, replica)` before joining a room.

The bundled browser remains a v1 client until Gate 7 implements its durable,
ack-retained outbox. Protocol v2 is enabled server-side and exercised by
`scripts/protocol-v2-check.mjs`; this avoids advertising durable delivery to a
client that still clears volatile pending work on socket send.

## Identity and batches

- `replica` and `batch` are opaque 128-bit values encoded as 32 lowercase hex
  characters.
- Replica identity is scope-bound through the actor derivation in ADR-0018.
- The durable deduplication key is `(scope, replica, batch)` plus a payload hash.
- Successful outcomes are retained for 30 days. Reusing a key with different
  bytes is a permanent integrity refusal.
- The standalone SQLite adapter commits operation rows and the batch outcome in
  one transaction. Its primary key is `(scope, replica, batch)`; duplicate
  payloads return the recorded sequence across reconnect and process restart.
- A client offline beyond the retention horizon resynchronizes before creating
  replacement batches; CRDT idempotence remains defence in depth, not the
  delivery protocol.

## Ordering and failure outcomes

For one room-cell command, the order is:

1. authenticate and validate the protocol state;
2. validate identity, clock, resource and semantic constraints;
3. append the batch idempotently;
4. apply the prepared batch;
5. enqueue acknowledgement to the sender;
6. broadcast the version-appropriate operation event to peers.

If the connection closes after step 3 but before step 5, the client retries the
same batch. The store returns the original sequence without reapplying or
rebroadcasting it. Retrying an already acknowledged batch has the same outcome.

Typed refusals disclose no storage path, SQL text, token state, unauthorized
scope existence or other replica identity. `not_durable` and `overloaded` are
retryable; semantic, actor, clock, identifier and capacity violations require a
state or user correction.

## Replica lifecycle

- One replica identity belongs to one active top-level client context.
- Reload retains it and advances the element counter from the joined document.
- A new tab creates a new identity.
- Logout/rotation waits for an empty outbox or requires explicit export/discard.
- Gate 7 persists identity and pending batches together in IndexedDB; until
  then, the in-memory outbox cannot claim crash/restart durability.
