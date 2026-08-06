# ADR-0022: Use one bounded FIFO storage writer with one in-flight write per scope

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Server, Storage, Operations
- **Related:** [ADR-0015](0015-per-scope-room-cells.md), [ADR-0017](0017-exact-snapshot-log-coverage.md), [ADR-0020](0020-sqlite-schema-recovery-and-checkpoint-policy.md)

## Context

SQLite permits one writer at a time. Giving every room its own connection would
move contention into SQLite, increase busy failures and make scheduling depend
on lock races. Keeping the current server-wide mutex is bounded but not a fair
cross-scope design: a large snapshot for one scope blocks unrelated commands.

The 2026-08-05 on-disk baseline measured median append latency of 131 microseconds
for one shape, 478 microseconds for ten and 2.15 milliseconds for one hundred.
Snapshot medians were 255 microseconds at 100 shapes, 9.47 milliseconds at
1,000 and 631 milliseconds at 10,000. A large snapshot therefore cannot share
an unbounded or hot-path execution policy with ordinary commands.

## Decision

The SQLite adapter has exactly one writer. Gate 4 places it behind a bounded
FIFO `tokio::sync::mpsc` mailbox with an initial capacity of 64 requests. A room
cell awaits its durable result before accepting its next durable command, so one
scope can hold at most one writer request while other scopes can take their
place in FIFO order. Full-mailbox admission is bounded waiting followed by a
typed transient overload; it never creates a detached retry task.

The storage deadline applies only while a request is still queued. The writer
and caller share an atomic queued/started/cancelled lifecycle: a deadline wins
only by cancelling a command before the writer starts it. Once a mutating
command starts, its room cell awaits the definitive SQLite outcome. Returning a
timeout while a transaction could still commit would make a later duplicate
retry acknowledge durable state that the live room never applied.

Snapshot work is coalesced to at most one retained candidate per scope and one
active storage write globally. A newer candidate replaces an unstarted older
candidate. Capture, encode and SQLite-write time remain separately observable.
Ordinary durable appends take precedence over queued best-effort snapshots; a
snapshot failure backs off from one second to a maximum of sixty seconds and
does not reject the edit that triggered it.

Gate 3 retains the existing single serialized connection as the correctness
baseline, which already bounds active snapshot work and candidate memory to
one. Gate 4 implements the mailbox and removes the server-wide document lock.
Capacity 64 is an initial operational value, not a permanent SLO; Gate 4 and
Gate 8 load evidence may lower or raise it without changing the one-writer and
one-in-flight-per-scope invariants.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| One writer connection per room | Simple room ownership | SQLite still serializes writes; busy races and connection growth | Moves rather than governs contention |
| Unbounded writer channel | Smooth short bursts | Memory and acknowledgement latency have no ceiling | Violates bounded-resource goals |
| Global FIFO with unlimited requests per scope | Simple ordering | A hot room can fill the queue ahead of cold rooms | Not scope-fair |
| One bounded FIFO writer and one in-flight request per scope | Deterministic SQLite ownership; bounded and fair admission | Requires backpressure and typed overload | **Chosen** |

## Consequences

- SQLite sees one deterministic writer instead of a connection race.
- A hot scope cannot enqueue an unbounded run ahead of cold scopes.
- Backpressure reaches the room cell and client as an explicit outcome.
- A queued command may be cancelled at its deadline; a started write is never
  detached from the room cell that must apply its durable result.
- Large snapshot serialization still needs isolation from unrelated scopes;
  the Gate 3 synchronous path is measured fallback behavior, not the target.
- A queue of 64 bounds request objects but not the size of a snapshot candidate;
  Gate 6 must enforce snapshot byte/CPU limits.

## Validation

- Reproduce the storage baseline with `cargo bench -p kboard-store --bench store -- --noplot`.
- Expose writer concurrency and batch bounds through storage health.
- Gate 4 tests fill the writer mailbox, prove typed overload, prove a hot scope
  cannot starve a cold scope, and prove at most one write per scope is in flight.
- Gate 8 records queue depth, wait time and cross-scope p99 latency.
- A deterministic delayed-writer test proves queued work times out without
  committing while already-started work completes with a definitive outcome.

## Revisit triggers

- Measurements show the single writer cannot meet the cross-scope SLO.
- SQLite is replaced by a store with safe parallel transaction semantics.
- Queue saturation or snapshot candidate bytes exceed their operational budget.
