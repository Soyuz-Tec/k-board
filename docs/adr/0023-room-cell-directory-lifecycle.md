# ADR-0023: Supervise bounded room cells through lifecycle-bearing directory entries

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Server, Storage, Operations
- **Related:** [ADR-0015](0015-per-scope-room-cells.md), [ADR-0022](0022-bounded-fair-storage-writer.md)

## Context

The server-wide `Mutex<ServerState>` makes same-scope ordering accidental and
forces unrelated rooms to share document, restore and snapshot latency. Merely
putting a mutex around each room would not define restoration coalescing,
mailbox overload, command cancellation, abnormal task exit or idle removal.

## Decision

Each active scope has one Tokio task owning its materialized `Room`, counters,
broadcast channel and snapshot schedule. Its bounded FIFO mailbox has capacity
64, matching the maximum admitted connections per scope and the invariant that
each connection awaits one command outcome before submitting another. Commands
are typed: join, durable commit, presence, leave, stats and drain. Every
request/response command uses a oneshot response. A full mailbox returns typed
transient overload immediately.

The caller deadline is five seconds. Cancellation before mailbox admission has
no effect. Once admitted, a durable command runs to a terminal result even if
the caller or response receiver disappears; the client retries the same batch
identity when it did not observe the result. Dropped response receivers never
panic the cell.

The directory stores only lightweight handles and published lifecycle state:
`Restoring`, `Ready`, `Draining`, `Failed`, or `Stopped`. Insertion under one
short asynchronous write lock coalesces concurrent cold joins. Restoration
runs outside that lock and is bounded globally. A restore failure leaves a
failed tombstone with a retry deadline, so repeated joins cannot create a
stampede or an empty replacement.

Idle eviction is a stop handshake. The cell receives `Drain` after all earlier
mailbox commands, closes admission, rejects commands already queued behind the
drain, publishes `Stopped`, and terminates. The directory removes an entry only
after termination is observed. A join during drain receives a retryable
draining outcome and never creates a second owner.

Lifecycle terms are exact: **empty** means zero live elements (tombstones may
still exist); **disconnected** means the broadcast receiver count is zero; and
**idle** is elapsed time since the last join, accepted durable command,
presence, or departure. Reclamation requires disconnected plus idle beyond the
TTL; an empty board alone is never a reason to discard durable state.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Per-room mutexes | Small refactor | No lifecycle, overload or cancellation model | Does not establish cell ownership |
| Remove map entry on idle | Simple | Can orphan queued writes and create duplicate owners | Violates durability and single ownership |
| Spawn a task per command | Parallel | Reorders one scope and has unbounded memory | Violates ordering and resource goals |
| Supervised cell plus lifecycle directory | Explicit terminal states and bounded work | More message types and tests | **Chosen** |

## Consequences

- No mutable document or storage I/O is held under the directory lock.
- Same-scope commands are ordered; different scopes can make task progress.
- Slow SQLite remains one measured shared resource behind its own bounded
  writer, rather than a hidden document lock.
- Failed entries consume a separately bounded tombstone budget until retry or
  expiry; operators can distinguish them from active rooms.

## Validation

- Deterministic tests prove FIFO same-scope results and two-scope progress.
- A full hot mailbox returns overload while a cold scope still responds.
- Concurrent cold opens return one cell identity and perform one restore.
- Drain, rejoin-during-drain, restore failure/retry and abnormal exit exercise
  every lifecycle transition.
- A 10,000-scope churn test leaves no active task or directory entry behind.

## Revisit triggers

- Measured task/channel overhead requires a different active-room bound.
- Multi-process ownership requires leases and fencing.
- A storage backend permits useful bounded parallel writers.
