# Security deployment and incident signals

## Deployment modes

| Mode | Required configuration | Intended use |
|---|---|---|
| Loopback development | Default `127.0.0.1`; authentication optional; in-memory storage allowed | One developer workstation only |
| Authenticated loopback | `KBOARD_SECRET`, optional `KBOARD_DB` | Local integration and security testing |
| Public | non-loopback `KBOARD_BIND`, `KBOARD_SECRET`, explicit `KBOARD_ALLOWED_ORIGINS`, persistent `KBOARD_DB`, TLS reverse proxy | Controlled deployment |

The process refuses a public bind without authentication and refuses every
non-loopback bind without an explicit browser Origin allowlist. Origins are
comma-separated absolute HTTP(S) origins with no path. Matching is exact after
normalization; do not use wildcards.

Example configuration (values are illustrative, not secrets to copy):

```text
KBOARD_BIND=0.0.0.0
KBOARD_SECRET=<high-entropy secret from the deployment secret manager>
KBOARD_ALLOWED_ORIGINS=https://board.example.com
KBOARD_DB=/var/lib/kboard/boards.db
PORT=8080
```

Terminate TLS before the server, restrict the database and backups to the
service account, and run only one server writer for a SQLite database. A shared
database is not a horizontal-scaling protocol. Rotate the signing secret by
ending existing sessions and minting new short-lived grants; old grants cannot
be selectively revoked in the current stateless format.

Mint grants only in a trusted operator context:

```text
kboard-server --token <scope> --ttl <seconds>
```

The browser accepts `?token=<grant>` only as a bootstrap compatibility input and
removes it from visible history immediately. Prefer passing grants directly to
the WebSocket integration when a host owns login. Do not put grants in logs,
analytics, support tickets or metric labels.

## Enforced resource policy

The source-of-truth values live in `crates/kboard-server/src/limits.rs` and
`crates/kboard-store/src/lib.rs`. The important independent ceilings are:

- 256 KiB WebSocket frames and 512 operations per frame;
- 64 commands per room-cell mailbox and 64 requests in the storage queue;
- 64 active connections per scope and 20,000 per process;
- eight active scopes per stable replica identity;
- four concurrent restores, 32 restoring directory entries and 256 failed
  tombstones;
- 50,000 live elements and a 32 MiB materialized payload estimate per room;
- 64 MiB serialized snapshots and one million replay operations;
- five-second command/storage caller deadlines and bounded retry backoff.

These are safety ceilings, not product entitlements. Raise one only with a
measured workload, aggregate-memory calculation and ADR update. Compression is
not negotiated; define a decompressed-size and CPU budget before enabling it.

## Health and incident signals

`GET /health` proves the HTTP process responds. It is not a storage readiness
claim. Use these operational endpoints as well:

- `GET /api/storage/health`: schema version, WAL checkpoint state, configured
  single-writer concurrency and operation batch bound;
- `GET /api/directory/health`: entries by lifecycle, available restore permits,
  admitted connections/identities/replicas and mailbox capacities;
- `GET /api/rooms/<scope>/stats`: existing authorized-operational room state;
  it never creates or restores a missing room.

Alert or investigate when:

- failed or restoring entries remain elevated, restore permits remain exhausted,
  or directory capacity approaches a ceiling;
- scope mailboxes/storage queues repeatedly return overload or command deadlines;
- subscriber lag closures or per-connection rate closures rise sharply;
- WAL busy frames persist, schema health fails, or snapshot failures/backoff rise;
- authentication refusals, hostile Origins or replica-in-use conflicts spike;
- room payload/snapshot limits are reached by ordinary user activity.

Logs intentionally use a fixed scope hash, never the raw scope or bearer. During
an incident, correlate the scope hash with trusted application ownership data
outside public logs. Preserve the database, WAL and corrupt snapshot evidence
before repair.

## Response playbooks

### Suspected bearer or signing-secret exposure

1. Remove public access or stop the service if active misuse is plausible.
2. Rotate `KBOARD_SECRET`; this invalidates all existing grants and sessions.
3. Review reverse-proxy and application diagnostics for accidental URL/header
   capture without copying grants into the incident record.
4. Reissue scoped, short-lived grants and confirm hostile Origin tests still
   fail before restoring public access.

### Storage unavailable or corrupt

1. Stop new writes and preserve the database plus WAL/SHM files and backups.
2. Check filesystem capacity/permissions and `/api/storage/health`.
3. Do not delete failed directory tombstones or corrupt snapshots to make joins
   appear healthy; the fail-closed state protects evidence.
4. Restore into a separate path, run migration/restart checks, and compare exact
   snapshot coverage before replacing the production database.

### Resource saturation

1. Identify whether the pressure is one scope, one identity, restores or the
   SQLite writer using bounded health counters and safe scope correlations.
2. Block the offending principal at the outer authenticated boundary where
   possible; do not raise global limits during an active incident.
3. Allow room-cell drain/backoff to complete; avoid repeated process restarts
   that turn a transient store failure into a restore stampede.
4. Capture a reproducible load profile before changing a ceiling.
