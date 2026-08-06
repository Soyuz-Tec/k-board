# ADR-0027: Make readiness, drain, backup and recovery explicit lifecycle states

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Operations, Storage, Server
- **Related:** [ADR-0020](0020-sqlite-schema-recovery-and-checkpoint-policy.md), [ADR-0023](0023-room-cell-directory-lifecycle.md)

## Context

`/health` previously proved only that HTTP could answer. It did not prove
SQLite could read and acquire a writer, report restore/storage saturation, or
distinguish a process that was intentionally draining. Shutdown drained cells
before telling the server to stop accepting, had no overall deadline, and did
not force a WAL checkpoint. Backup and restore validation were manual.

## Decision

`/health/live` proves the event loop can respond. `/health/ready` fails closed
during drain, database read/write failure, full restore capacity or storage
queue saturation. `/health/diagnostics` reports `ready`, `degraded`,
`not_ready` or `draining`, directory/restore state, WAL health and bounded
telemetry. `/health` remains the liveness compatibility alias. An in-memory
development store is live and ready but diagnosed as degraded; non-loopback
binds require a persistent database.

On termination the process atomically stops WebSocket admission, drains all
restoring/ready cells within a 20-second default overall deadline, reports
hashed incomplete scopes, and places a FIFO storage barrier followed by a full
WAL checkpoint. The deadline is configurable from one through 300 seconds.

SQLite online backup writes to a new destination, then performs read-only
integrity, foreign-key and schema verification. Restore verification copies the
backup through SQLite's restore API into an in-memory database and rebuilds
every durable scope; the source is never opened writable. CLI commands are
`--backup`, `--verify-database`, and `--restore-check`. Existing backup targets
are refused rather than overwritten.

The initial recovery objectives are zero acknowledged operations lost for a
backup taken after acknowledgement, and recovery within 30 seconds for the
qualification fixture. Operations accepted after the backup began belong to
the next recovery point. Exact schedules, alerts and procedures are normative
in `docs/operations/production-lifecycle.md`.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| One health endpoint | Simple | Routes traffic into storage/restore failure | Conflates process and dependency health |
| Copy the SQLite/WAL files | Familiar | Easy to capture an inconsistent set | Ignores SQLite's supported online backup protocol |
| Unbounded graceful shutdown | Maximizes work completion | Deployment can hang indefinitely | Availability needs a reported deadline |
| Admission stop, bounded drain, writer flush and verified online backup | Explicit transitions and measurable recovery | More operational code and drills | **Chosen** |

## Consequences

- Orchestrators route only on readiness and restart only on liveness.
- Operators receive incomplete-drain evidence rather than a false clean exit.
- Backup success is not claimed until physical and logical restore checks pass.
- Schema rollback remains forward-fix; restoring an older verified backup is a
  data recovery decision, not an application downgrade shortcut.

## Validation

- Unit tests cover database read/write readiness, path validation, saturation
  policy, backup integrity and isolated logical restore.
- Room/store tests cover queued durable drain, append failure, full/read-only/
  busy SQLite, exact snapshot transitions and recovery from complete logs.
- The 2026-08-05 game day lost zero acknowledged operations, produced and
  verified a 20,480-byte online backup in 27.58 ms, and restored service in
  154.53 ms against the 30-second objective.

## Revisit triggers

- Backup size makes the synchronous CLI run exceed the maintenance budget.
- Storage moves from local SQLite to a managed database.
- Rolling deployment requires overlapping processes, which first requires the
  ownership/fencing decision in ADR-0028.
