# ADR-0020: Version SQLite explicitly and fail closed when recovery is incomplete

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Storage, Operations, Server
- **Related:** [ADR-0007](0007-sqlite-durable-storage.md), [ADR-0017](0017-exact-snapshot-log-coverage.md)

## Context

The standalone store creates tables with `CREATE TABLE IF NOT EXISTS` but does
not record a schema version or run ordered migrations. Snapshot decode failure
is currently returned by the adapter, while the server converts every restore
failure into an empty room. That can present durable data loss as a new board.
WAL is enabled without an explicit checkpoint or busy-wait policy.

## Decision

SQLite `PRAGMA user_version` is the authoritative schema version. Migration 1
creates the original operations and snapshots schema. Migration 2 adds durable
protocol-batch outcomes. Each migration runs in its own immediate transaction,
sets the next version only after its statements succeed, and is covered by a
checked-in SQL fixture. A database newer than this binary is refused.

Snapshot restore validates both JSON and scope. A corrupt snapshot is preserved
unchanged. Log-only recovery is permitted only when the operation log is proven
complete from sequence 1 with no gaps. Otherwise the scope fails closed and is
not replaced with an empty room. The resulting diagnostic is observable but
never includes SQL text, filesystem paths, bearer tokens or document content.

The standalone adapter keeps one bounded writer connection, configures a finite
busy timeout, uses WAL with `synchronous=NORMAL`, and sets an automatic passive
checkpoint threshold. Health exposes schema version plus passive checkpoint
frame counts. Gate 4 may put the connection behind a FIFO mailbox; it must not
weaken these transaction and recovery contracts.

## Consequences

- Existing unversioned databases are recognized as legacy schema 1 or 2 and
  adopted without discarding rows.
- Failed or future migrations stop startup instead of partially upgrading.
- A corrupt compacted scope becomes unavailable rather than silently blank.
- A complete untruncated log can safely recover a corrupt snapshot while
  retaining the corrupt row as evidence.
- Busy, read-only and full-disk failures are explicit storage failures and do
  not mutate room state.

## Validation

- Open every supported schema fixture and verify preservation plus final
  `user_version`.
- Refuse a future schema version.
- Corrupt a snapshot with and without a complete log and verify the two recovery
  outcomes without deleting the corrupt row.
- Exercise read-only, busy and page-limit/full failures.
- Query checkpoint health on an open WAL database.

## Revisit triggers

- SQLite is replaced or the host owns migrations.
- Multiple writer connections are introduced.
- Recovery tooling gains an operator-approved quarantine store.
