# ADR-0015: Own mutable room state in one ordered cell per active scope

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Server, Storage, Operations
- **Related:** [ADR-0005](0005-in-memory-room-state.md), [ADR-0006](0006-server-resource-limits.md), [ADR-0007](0007-sqlite-durable-storage.md)

## Context

The standalone server stores every active `Room` and the SQLite adapter behind
one `Arc<Mutex<ServerState>>`. An edit, join snapshot, lazy restore or snapshot
write for any scope holds the same lock used by every other scope. The durable
adapter also uses one SQLite connection.

The arrangement is simple and makes same-room ordering accidental, but it makes
fault and latency isolation impossible. A large snapshot or slow database call
for scope A stalls unrelated scope B. Moving work out of the lock without
introducing an explicit owner would instead allow mutation, persistence and
broadcast to reorder.

## Decision

The server remains one deployable modular monolith. Each active scope is owned
by one **room cell**: one task, one materialized document and counters, one
bounded command mailbox, and one ordered command loop.

A lightweight directory maps validated scopes to cell handles and owns the
lifecycle state machine. It never owns documents and never performs storage I/O
while holding its synchronization primitive. Concurrent cold joins coalesce on
one `Restoring` entry. A failed restore remains explicit and never becomes an
empty board.

Commands within one scope are serialized by the cell. Cells for different
scopes execute concurrently. Storage scheduling can remain a shared adapter,
but its queueing, fairness and latency are measured separately and bounded; a
cell does not imply that SQLite itself is parallel.

Mailbox saturation returns a typed overload outcome. It never silently drops a
durable command. Shutdown and idle eviction are cell stop handshakes rather than
removing a map entry while work remains.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Keep one server-wide mutex | Smallest implementation | Cross-scope head-of-line blocking; I/O and serialization under global lock | Fails the fault- and performance-isolation goals |
| Lock each room directly | Less refactoring | Ordering spans room, persistence, ack, presence and lifecycle; lock duration remains easy to violate | A lock does not model the command lifecycle or overload outcome |
| Spawn unrestricted work per message | Maximum apparent concurrency | Same-scope reorder, unbounded work and difficult cancellation | Violates consistency and resource bounds |
| Microservice or process per room | Strong process isolation | Placement, discovery, fencing and operational cost before measured need | The current scale does not justify a distributed system |
| One bounded cell per active scope | Explicit ownership and ordering; independent scopes progress | More lifecycle and mailbox behavior to specify and test | **Chosen** |

## Consequences

### Positive

- The scope boundary becomes the runtime ownership and fault-containment boundary.
- Same-scope ordering is explicit and testable.
- Bounded mailboxes make overload visible and cap queued memory.
- Directory, cell and storage latency can be measured independently.

### Negative and accepted trade-offs

- Every active scope has task, channel and lifecycle overhead.
- Slow storage can still be a shared bottleneck until the adapter is measured
  and scheduled fairly.
- A hot scope is intentionally serialized; CRDT convergence does not make two
  concurrent mutations of the same materialized document safe.
- Horizontal scale needs a later placement and fencing decision. A shared
  database is not permission for two active cells to own the same scope.

### Security and operational consequences

Scope authorization occurs before directory lookup. Scope strings are never raw
metric labels. Cell panic, restore failure, mailbox saturation and drain timeout
must be observable without exposing tokens or document contents.

## Validation

- A deterministic two-scope test blocks scope A in storage and proves scope B
  continues to accept commands.
- Same-scope commands apply and acknowledge in mailbox order.
- Concurrent first joins perform one restore.
- Full mailboxes return overload and remain within their memory bound.
- Idle eviction and shutdown do not abandon an acknowledged or queued command.

## Revisit triggers

- Measured cell/task overhead prevents the configured active-scope limit.
- One process cannot meet the checked-in throughput or fault-domain target.
- A shared storage scheduler dominates cross-scope p99 after the cell split.
- Multi-process deployment is required, in which case scope placement, leases
  and fencing need a superseding ADR.
