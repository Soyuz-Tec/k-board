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

## Not yet recorded

Decisions that are made but whose reasoning still lives only in code comments.
Each should become a record before it becomes load-bearing for someone else:

- Element ids as `(actor, counter)` rather than random, keeping the engine free
  of an entropy source
- Fractional z-indexing over integer ordering
- Tombstone retention and host-driven collection horizons
- JSON as the wire format, and property keys as strings rather than a derived
  enum representation

## Template

Context → Decision → Alternatives considered → Consequences (positive /
negative and accepted trade-offs / operational / security) → Validation →
Revisit triggers.

The alternatives table is not decoration. A record without a genuinely
considered rejected option is usually a decision that was never really made.
