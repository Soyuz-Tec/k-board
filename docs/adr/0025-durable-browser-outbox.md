# ADR-0025: Persist client batches until matching durable acknowledgement

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Web, Protocol, Operations
- **Related:** [ADR-0016](0016-durable-batch-acknowledgement.md), [ADR-0018](0018-replica-identity-and-hlc-trust.md)

## Context

WebSocket `send` reports only that bytes entered a browser buffer. It does not
prove that the server validated, appended or committed an edit. The original
client discarded operations immediately after `send`, changed identity on
reload, and offered no recovery path after a permanent refusal or browser
storage failure.

## Decision

Each browser tab has a session-stable tab id. IndexedDB assigns a stable
replica id to the `(scope, tab)` pair and stores bounded operation batches.
The actor is derived deterministically from that replica using the same rule as
the server. A reload reopens the same board identity and replays every
unacknowledged operation before connecting.

The client persists a batch before sending it, marks one batch `sending`, and
removes it only when an acknowledgement carries the same batch id. A socket
close or acknowledgement timeout converts outcome-unknown `sending` work back
to `pending`, retaining the original id. Duplicate and unknown acknowledgements
are harmless. Overload retains the batch and retries with bounded exponential
backoff and jitter. Permanent refusal retains the batch and enables a JSON
recovery export.

The outbox is bounded to 4,096 operations and 8 MiB, with at most 512 operations
per protocol batch. Capture is transactional: exceeding a bound or an
IndexedDB quota never creates a partial durable batch. Operations that cannot
be persisted remain in this tab's memory and are included in recovery export
while the tab remains alive. Private browsing or unavailable IndexedDB is an
explicit tab-only limitation, never reported as durable.

Server initialization is merged into the locally restored document only after
protocol version and replica/actor identity match. No pending operation is sent
before that negotiation completes.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Clear on WebSocket `send` | Minimal code | Loses outcome-unknown edits on close or crash | Confuses transport acceptance with durability |
| Persist raw operations without batch ids | Simple queue | Retry cannot use server deduplication | Cannot make ambiguous outcomes safe |
| One identity per browser profile | Fewer identities | Concurrent tabs share actor/counter space | Creates element-id and ordering collisions |
| Transactional IndexedDB outbox per scope/tab | Survives reload with bounded exact retries | Requires explicit fallback and recovery UX | **Chosen** |

## Consequences

- `durable` means the matching server acknowledgement arrived; `sending` does
  not.
- Closing a tab can still lose operations that IndexedDB refused. The UI says
  `this tab only` and offers recovery export while it can.
- Users may see locally refused edits until they export or remove the recovery
  record; silent deletion is intentionally forbidden.
- The implementation has no multi-tab leader election. Each tab is an
  independent replica and can connect concurrently.

## Validation

- Deterministic outbox checks cover close before acknowledgement, reload,
  stable batch and replica identity, duplicate acknowledgement, overload,
  permanent refusal, bounds, quota failure and private-mode fallback.
- Browser automation creates offline work, reloads it, and observes that the
  same pending batch survives until a real durable acknowledgement.
- Existing protocol checks prove append-before-ack and idempotent retry.

## Revisit triggers

- Browser eviction becomes material enough to require persistent-storage
  permission or encrypted host storage.
- Multiple tabs should deliberately share one replica rather than remain
  independent.
- Recovery import, user-selectable retry/discard, or end-to-end encrypted local
  persistence is required.
