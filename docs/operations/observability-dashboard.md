# Operational dashboard and alert review

This is the vendor-neutral dashboard contract for the supported single-process
topology. A deployment may implement it in its monitoring platform, but must
preserve the fields, windows and actions below. Poll `/health/diagnostics` at
least every 30 seconds and probe readiness separately.

## Required panels

| Panel | Source | Display and threshold |
|---|---|---|
| Service state | `status`, `accepting`, `/health/ready` | Current state plus time not ready; page when not ready for 2 minutes |
| Persistence | `persistent_database`, `storage.readable`, `storage.writable`, `storage.schema_version` | Page immediately on read/write failure or unexpected schema |
| Room lifecycle | `directory.entries`, `restoring`, `ready`, `draining`, `failed`, `restore_permits_available` | Page on any failed room; warn when all restore permits are busy for 30 seconds |
| Directory latency | `directory.lookup` | Count, average and maximum; warn when maximum exceeds the qualified command SLO |
| Storage queue | `storage.metrics.queue_depth`, `max_queue_depth`, `overloads`, `queue_wait` | Warn at 50% for 5 minutes; remove readiness at 75%; investigate every overload |
| SQLite work | `storage.metrics.sql` | Count, average and maximum separated from queue wait |
| WAL health | `wal_busy`, `wal_log_frames`, `wal_checkpointed_frames` | Apply the WAL thresholds in the production lifecycle runbook |
| Capacity | directory entries, restore permits, configured process limits | Show current/limit and retain operating headroom below the qualified 64-scope envelope |

Latency summaries expose count, total and maximum rather than unbounded labels.
The published bucket boundaries are `latency_buckets_micros`; external adapters
must use only bounded component, phase and outcome labels. Raw scope, document,
token, replica and batch values are prohibited dashboard labels.

## Alert playbook links

- Readiness, WAL, disk, dedupe, corruption and partial-storage response:
  [`production-lifecycle.md`](production-lifecycle.md).
- Authentication, origin, scope-capacity and credential incidents:
  [`../security/deployment-and-incidents.md`](../security/deployment-and-incidents.md).
- Single-writer and scale-out boundary:
  [`../adr/0028-single-process-topology-and-scale-triggers.md`](../adr/0028-single-process-topology-and-scale-triggers.md).

## Review result

The application exposes the data needed for these panels and the alert actions
are documented. No monitoring provider or target environment is configured in
this repository, so target dashboard provisioning and alert routing remain
deployment evidence rather than application-code evidence.
