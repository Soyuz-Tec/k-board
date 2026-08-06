# ADR-0017: Persist the exact log sequence captured by a snapshot

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Storage, Server
- **Related:** [ADR-0007](0007-sqlite-durable-storage.md), [ADR-0015](0015-per-scope-room-cells.md)

## Context

The current snapshot store serializes a document and then asks the operation log
for the scope's latest sequence while writing. This is safe only because the
server currently holds one global lock across mutation and snapshot storage.

Room cells are intended to move serialization and writes away from unrelated
scope work. Once snapshot work can overlap later appends, write-time sequence
lookup can record a sequence newer than the document. Truncating through that
sequence would delete operations absent from the snapshot and cause permanent
data loss on restore.

The HLC `through` inside the CRDT snapshot and the storage log sequence answer
different questions and are not interchangeable.

## Decision

A snapshot candidate is an immutable pair of:

1. the materialized document captured by the room cell; and
2. the exact durable log sequence included in that document at capture time.

The `SnapshotStore` adapter accepts that sequence as data. It never calls
`next_seq` or otherwise infers coverage at write time. Snapshot commit and
deletion of log entries at or before the captured sequence occur atomically in
one storage transaction. Operations after that sequence remain replayable even
if they were appended before snapshot serialization finishes.

Snapshot capture, serialization, write and eligible truncation are separately
timed and fault-injectable. Failed snapshot work keeps the existing snapshot and
log; it does not block continued durable edits indefinitely.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Query latest sequence when writing | Current API; compact log | Can claim operations the document lacks | Unsafe with asynchronous work |
| Pause the cell for complete serialization/write | Simple exactness | Large documents block all edits in that scope | Kept only as baseline/fallback, not the target |
| Never truncate the log | No coverage-loss risk | Unbounded storage and restore time | Violates resource and operational goals |
| Capture document plus exact sequence | Safe overlap and truncation | Requires API and transaction changes | **Chosen** |

## Consequences

### Positive

- Later appends cannot be deleted by an older snapshot.
- Snapshot correctness no longer depends on a server-wide lock.
- Restore is well-defined as snapshot at sequence N plus log after N.

### Negative and accepted trade-offs

- Capturing an immutable document may still have a measurable in-cell copy cost.
- Multiple in-flight candidates must be bounded; an older successful candidate
  may be superseded by a newer one.
- Store interfaces and fixtures need migration.

### Operational consequences

Operators can distinguish capture, encode, SQLite write and truncation latency.
Disk-full or corrupt-snapshot errors preserve the untruncated log and make the
scope/readiness state observable.

## Validation

- Block snapshot serialization after capturing sequence N, append N+1, then
  finish the snapshot; restore includes N+1 from the log.
- Crash before snapshot commit retains the previous snapshot and complete log.
- Crash after atomic commit restores from the new snapshot plus remaining tail.
- A failed snapshot never advances stored coverage or truncates operations.

## Revisit triggers

- The storage engine supplies an equivalent transactional snapshot primitive.
- Snapshot capture cost dominates the scope's latency budget and requires an
  immutable/persistent document representation.
