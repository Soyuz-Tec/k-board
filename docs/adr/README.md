# Architecture decision records

One decision per record. A record exists when the decision was not obvious, when
the obvious choice was rejected, or when a future reader would otherwise be
likely to "fix" something deliberate.

Records are immutable once accepted. A decision that changes gets a new record
that supersedes the old one; the old one stays, because the reasoning that was
true at the time is what makes the change legible.

| # | Decision | Status | Why it exists |
|---|---|---|---|
| [0001](0001-per-property-convergence.md) | Resolve conflicts per property, not per element | Accepted | The lost-update defect this project was built to remove |
| [0002](0002-clear-as-explicit-deletes.md) | Expand "clear" into explicit deletes at the origin | Accepted | A board-wide operation would permanently diverge replicas |
| [0003](0003-raw-c-abi-over-wasm-bindgen.md) | Raw C ABI rather than generated wasm bindings | Accepted | One contract for browsers and native hosts, not two |
| [0004](0004-global-registry-and-side-buffer.md) | Process-global registry; results via a side buffer | Accepted | Thread-local state breaks on the BEAM; packed returns break on 64-bit |
| [0005](0005-in-memory-room-state.md) | Rooms hold a materialised document, not a log | Accepted | The log leaked and cost O(document) per message |
| [0006](0006-server-resource-limits.md) | Bound every resource an unauthenticated peer can consume | Accepted | Three independent denial-of-service routes found in audit |
| [0007](0007-sqlite-durable-storage.md) | Persist boards in SQLite behind the engine's storage ports | Accepted | ADR-0005 relocated the log to durable storage; nothing implemented it |
| [0008](0008-scoped-bearer-tokens.md) | Scoped bearer tokens; refuse to serve openly on a public interface | Accepted | The gap ADR-0006 named as the top security risk |
| [0009](0009-one-geometry-two-back-ends.md) | Describe each shape once; render it through two back-ends | Accepted | An export that disagrees with the screen is found after it is sent |
| [0010](0010-presence-is-relayed-not-merged.md) | Presence is relayed and forgotten, never merged | Accepted | Cursors through the CRDT would make every mouse movement durable |
| [0011](0011-system-clipboard-without-ids.md) | Copy through the system clipboard, with the element id dropped | Accepted | A copy that keeps its id moves the thing you copied |
| [0012](0012-host-measured-text.md) | The host measures text; the engine stores the box | Accepted | Text has a size only once you know the font, and the engine has none |
| [0013](0013-box-and-turn-not-a-matrix.md) | A box and a turn, not a matrix | Accepted | Non-uniform resize of a rotated shape is a shear the model cannot hold |
| [0014](0014-partial-style-writes.md) | A style write carries only what changed | Accepted | Resending a whole style silently reverts a peer's concurrent restyle |
| [0015](0015-per-scope-room-cells.md) | One ordered room cell owns each active scope | Accepted | Remove cross-scope head-of-line blocking without losing same-scope ordering |
| [0016](0016-durable-batch-acknowledgement.md) | Acknowledge durable idempotent batches, not socket sends | Accepted | Make retry and user-visible save state truthful end to end |
| [0017](0017-exact-snapshot-log-coverage.md) | Persist the exact log sequence captured by a snapshot | Accepted | Prevent asynchronous snapshot work from truncating unseen operations |
| [0018](0018-replica-identity-and-hlc-trust.md) | Bind operations to stable replicas and governed clocks | Accepted | Close actor forgery, restart collision and future-clock integrity gaps |
| [0019](0019-u64-actor-c-abi.md) | Carry the engine's u64 actor through the C ABI | Accepted | Remove adapter truncation and support collision-resistant stable actors |
| [0020](0020-sqlite-schema-recovery-and-checkpoint-policy.md) | Version SQLite and fail closed when recovery is incomplete | Accepted | Prevent partial migrations and silent empty-room recovery |
| [0021](0021-tombstone-and-offline-replica-horizon.md) | Retain tombstones until causal safety is provable | Accepted | Avoid offline resurrection from timer-based collection |
| [0022](0022-bounded-fair-storage-writer.md) | Use one bounded FIFO storage writer with one in-flight write per scope | Accepted | Bound SQLite contention without letting a hot room starve cold rooms |
| [0023](0023-room-cell-directory-lifecycle.md) | Supervise bounded room cells through lifecycle-bearing directory entries | Accepted | Make ordering, overload, restore, drain and failure explicit |
| [0024](0024-origin-session-and-aggregate-resource-policy.md) | Enforce origin, session expiry and aggregate scope/identity budgets | Accepted | Stop browsers and many connections from multiplying trust and resource authority |
| [0025](0025-durable-browser-outbox.md) | Persist client batches until matching durable acknowledgement | Accepted | Stop socket close, reload and refusal from silently discarding edits |
| [0026](0026-per-board-ffi-locks-and-bounded-telemetry.md) | Isolate FFI handles and expose bounded operational telemetry | Accepted | Remove global native-handle contention and make phase costs visible safely |
| [0027](0027-production-lifecycle-and-recovery.md) | Make readiness, drain, backup and recovery explicit lifecycle states | Accepted | Route and recover from operational truth rather than process-up assumptions |
| [0028](0028-single-process-topology-and-scale-triggers.md) | Keep one authoritative process until placement and fencing exist | Accepted | Prevent shared-storage split brain and premature service extraction |
| [0029](0029-immutable-release-candidate.md) | Qualify and promote one commit-addressed artifact set | Accepted | Prevent environment rebuilds from replacing the bits that passed qualification |
| [0030](0030-standalone-and-embedded-web-delivery.md) | Deliver one web client through standalone and Embedded SDK shells | Accepted | Prevent product-mode drift while keeping host integration explicit and independently deployable |

## Not yet recorded

Decisions that are made but whose reasoning still lives only in code comments.
Each should become a record before it becomes load-bearing for someone else:

- Element ids as `(actor, counter)` rather than random, keeping the engine free
  of an entropy source
- Fractional z-indexing over integer ordering
- JSON as the wire format, and property keys as strings rather than a derived
  enum representation
- The growing list of properties with a *read default* that undo depends on —
  `Angle`, `FontSize`, `Opacity` — introduced in ADR-0013 and extended by
  ADR-0014. Four entries is where an implicit convention starts going stale
  quietly.

## Template

Context → Decision → Alternatives considered → Consequences (positive /
negative and accepted trade-offs / operational / security) → Validation →
Revisit triggers.

The alternatives table is not decoration. A record without a genuinely
considered rejected option is usually a decision that was never really made.
