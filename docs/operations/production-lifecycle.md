# Production lifecycle and recovery runbook

This runbook is the operator contract for the supported single-process SQLite
topology. Never run two active writers against one database.

## Probes and states

- `/health/live`: restart signal only. HTTP 200 means the event loop responds.
- `/health/ready`: routing signal. HTTP 503 during drain, database read/write
  failure, restore saturation or storage queue saturation.
- `/health/diagnostics`: `ready`, `degraded`, `not_ready` or `draining`, with
  bounded directory, WAL, queue and phase measurements.
- `/health` is the liveness compatibility alias.

An in-memory development process reports degraded diagnostics. Public binds
must configure `KBOARD_SECRET`, `KBOARD_ALLOWED_ORIGINS` and `KBOARD_DB`.
Startup rejects invalid ports/bind addresses, missing web/wasm assets,
directories used as database files and nonexistent database parents.

## Normal shutdown and deployment

Promote the commit-addressed artifact produced by CI after verifying its
`SHA256SUMS`; do not rebuild components in the target environment. Record the
artifact name, source commit, manifest digest, target, operator and time. The
server, native libraries, wasm engine and web assets are one qualified set.

Send the platform termination signal and allow at least
`KBOARD_DRAIN_TIMEOUT_SECS` (default 20, valid 1–300) plus the platform's final
kill margin. The server immediately refuses new WebSockets, drains active cells
in mailbox order, reports hashed incomplete scopes, places a storage FIFO
barrier and performs a full WAL checkpoint. Do not start an overlapping writer.

If the report says `storage_flush=incomplete`, retain the database and WAL,
restart from the same volume, and let idempotent client batches close ambiguous
outcomes. Do not delete WAL/SHM files manually.

After start, require readiness, packaged wasm fetch, authenticated room join,
one durably acknowledged mutation, restart recovery of that mutation and an
isolated restore check. Roll back executable/assets only when the older binary
supports the current database schema. Otherwise preserve the database and WAL
and forward-fix. Restoring an older backup requires an explicit accepted RPO.

## Backup and restore

Create a unique destination on separate durable media:

```text
KBOARD_DB=/data/boards.sqlite3 kboard-server --backup /backup/boards-YYYYMMDD-HHMM.sqlite3
kboard-server --verify-database /backup/boards-YYYYMMDD-HHMM.sqlite3
kboard-server --restore-check /backup/boards-YYYYMMDD-HHMM.sqlite3
```

Backup refuses an existing destination. Success includes integrity, foreign-key
and schema checks plus an isolated in-memory restore of every scope. Encrypt and
access-control the file as tenant content.

Schedule daily backups, retain 7 daily and 4 weekly copies, and run the isolated
restore check on every copy. Run `node scripts/backup-restore-drill.mjs` monthly
and before storage/schema releases. Record backup size/duration, newest durable
sequence, acknowledged-operation RPO and service RTO. Initial objectives are
zero acknowledged operations lost for a post-ack backup and RTO below 30 s for
the qualification fixture. The 2026-08-05 drill observed RPO 0, 27.58 ms backup
and 154.53 ms recovery.

## Crash-consistency transitions

| Kill point | Durable truth | Recovery action |
|---|---|---|
| Queued before append | No sequence; no ack | Client retries original batch id |
| Append transaction in progress | Whole transaction committed or rolled back | Batch outcome lookup resolves ambiguity |
| Appended before apply/ack | Log and batch outcome are durable | Restore/retry returns original sequence |
| Applied before broadcast | Durable log is authoritative | Reconnect init/replay supplies state |
| Snapshot encode | Existing snapshot/log unchanged | Retry snapshot later |
| Snapshot transaction | New snapshot and exact prefix truncation commit together or neither does | Restore snapshot plus tail |

Never infer success from socket delivery. A matching durable ack is the client
contract.

## WAL, disk, dedupe and corruption alerts

Poll diagnostics at least every 30 seconds.
The required provider-neutral panels and label restrictions are defined in
[`observability-dashboard.md`](observability-dashboard.md).

- Warning: uncheckpointed WAL exceeds 10,000 frames or 512 MiB; critical at 1
  GiB, three consecutive `wal_busy` probes, or no checkpoint progress for 10
  minutes.
- Warning: free volume below 25% or 10 GiB; critical below 15% or 5 GiB. At
  critical, stop admission before SQLite returns disk-full mid-traffic.
- Warning: storage queue at 50% for five minutes; readiness removes traffic at
  75%. Any overload in legitimate traffic starts a capacity incident.
- Warning: all four restore permits busy for 30 seconds. Readiness fails when
  restore work is saturated.
- Critical: any failed/corrupt scope. Preserve snapshot/log evidence and do not
  replace it with an empty room.
- Warning: dedupe rows grow more than 10% per hour after the retention horizon;
  critical at five million rows or when cleanup fails. Never delete operation
  rows as a dedupe remedy.

For partial storage availability, keep liveness up for diagnostics, let
readiness remove traffic, stop new WebSockets, preserve the database/WAL, and
recover the volume or restore the latest verified backup. Clients retain v2
batches and retry unchanged identities.

## Schema and compaction

Migrations are forward-only and transactional. Before release: verified online
backup, release-mode fixture migration smoke, staging restore, then production
drain/upgrade. If migration fails, preserve the file and forward-fix the
migration; do not run an older binary against a newer schema. Restore an older
backup only with an explicit accepted RPO decision.

Do not run in-place `VACUUM` during ordinary traffic. Produce a compact candidate
with `VACUUM INTO` or an approved SQLite maintenance tool on a verified backup,
validate it with both CLI checks, then activate it through the same bounded
drain/restart. Candidate creation does not cause a global outage; activation
never overlaps two writers. Snapshot/log compaction remains online and exact
inside the storage writer.

## Capacity model

| Resource | Implemented bound | Readiness/response |
|---|---:|---|
| Active directory entries | 10,000 | typed room capacity refusal |
| Concurrent restores | 4; 32 restoring entries | not ready when permits/backlog saturate |
| Room mailbox | 64 commands | retryable overload |
| Storage mailbox | 64 commands | retryable overload; not ready at 75% |
| Connections | 20,000 process; 64 per scope | handshake overload |
| Operations per batch | 512 | permanent batch-too-large refusal |
| Materialized/serialized scope | 32 MiB / 64 MiB | room unavailable rather than unbounded allocation |

The 2026-08-05 debug-build qualification sustained 64 simultaneously writing
scopes with 31.25 ms cold p99; 96 scopes hit explicit storage overload. Re-run
the capacity script on target hardware and size admission below the observed
failure point with operating headroom.
