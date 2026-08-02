# ADR-0005: Rooms hold a materialised document, not an operation log

- **Status:** Accepted
- **Date:** 2026-08-02
- **Owners:** Server
- **Related:** [ADR-0006](0006-server-resource-limits.md)
- **Supersedes:** the log-and-snapshot room design shipped in `ff04d24`

## Context

The first server kept, per room, a `Snapshot` plus a `tail` of operations not
yet folded into it, compacting when the tail crossed a threshold. It mirrored
the durable model the engine's `OpLog`/`SnapshotStore` ports describe.

Applied to in-memory state it was wrong twice.

**It leaked.** The tail grew with every operation and was only ever folded, never
bounded. A long-lived board accumulated operations for the process lifetime.

**It was O(document) per message.** To report how many operations changed
anything, `accept` cloned the entire snapshot and folded the tail into the copy
— on *every incoming frame*. At 10,000 elements and twenty frames per second per
user, the server spent its time copying documents.

Both faults came from importing a durability design into a component that has no
durability responsibility.

## Decision

A room holds one `Snapshot` that absorbs operations immediately, plus a
broadcast channel. There is no in-memory operation log.

`accept` becomes O(operations). Serving a join becomes O(1) — it borrows the
document rather than reconstructing it.

The operation log is not abandoned; it is relocated. It belongs behind the
engine's `OpLog` port in durable storage, where truncation is a storage concern
with a real retention policy rather than a heuristic over a growing `Vec`.
Snapshot compaction returns with persistence.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Keep log + snapshot, drop the clone | Retains incremental-resume capability | Log still unbounded in RAM | Fixes the CPU cost, keeps the leak |
| Keep log, cap it with a ring buffer | Bounded memory | A client behind the ring cannot resume incrementally and must full-sync anyway | Complexity for a capability the join path does not use |
| Materialised document only | O(ops) accept, O(1) join, no leak | No incremental resume; joins always full-sync | **Chosen.** Full-sync is what the current join path does regardless |

## Consequences

### Positive

- Accept is linear in the batch, not in the document.
- Room memory is bounded by document size, not by history length.
- The server no longer pretends to be a durability layer.

### Negative and accepted trade-offs

- **Rooms are volatile.** Restarting the server destroys every board. This is
  the top item on the roadmap and is stated plainly in the README.
- Reconnecting clients always receive the full document, never a delta. Costly
  on large boards over poor links.
- The `compactions` statistic is gone; `accepted` replaces it.

### Operational consequences

Memory per room is now predictable: roughly document size plus the broadcast
channel. Capacity planning is a function of element count and room count, both
of which ADR-0006 bounds.

## Validation

- `a_joining_client_sees_everything_accepted`
- `replayed_operations_change_nothing` — idempotent absorption survives the
  removal of the log
- `an_oversized_batch_is_refused_whole`
- `stats_report_tombstones_separately`

## Revisit triggers

- Durable persistence lands — the `OpLog` port then holds history and this ADR's
  premise changes.
- Full-sync on join is measured as too expensive on realistic boards, requiring
  a delta path and therefore a bounded server-side log again.
