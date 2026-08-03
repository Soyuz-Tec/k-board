# ADR-0007: Persist boards in SQLite behind the engine's storage ports

- **Status:** Accepted
- **Date:** 2026-08-03
- **Owners:** Server, Storage
- **Related:** [ADR-0005](0005-in-memory-room-state.md), [ADR-0006](0006-server-resource-limits.md)

## Context

Restarting the server destroyed every board. ADR-0005 removed the in-memory
operation log for good reasons — it leaked, and folding it cost O(document) on
every message — and said plainly where the log belongs: "durable storage behind
the engine's `OpLog` port, where truncation is a storage concern with a real
retention policy rather than a heuristic over a growing `Vec`."

Nothing had implemented that port, so the sentence was a promise rather than a
fact. This is the implementation.

Two constraints shape it. `kboard-core` is `#![forbid(unsafe_code)]` with no
I/O, no clock, and no randomness, so storage cannot live there. And the engine
must stay embeddable: a host platform plugs in its own Postgres and never sees
any of this, which only works if the standalone adapter is an ordinary
implementation of the same ports rather than a special case.

## Decision

A new crate, `kboard-store`, implements `OpLog` and `SnapshotStore` against
SQLite. Nothing in `kboard-core` can reach it; the engine declares what it needs
and the crate supplies it, which is the same arrangement a host uses.

**Writes are durable before they are broadcast.** The server appends to the log
inside the same critical section that accepts the batch, and only broadcasts
after the append succeeds. A peer holding an operation the log never recorded
would have state the board cannot rebuild — and no retry fixes it, because the
originating client already believes it was saved. A failed append refuses the
batch and closes the connection rather than pretending.

**Rooms restore lazily, on join.** A board absent from memory is rebuilt from
its snapshot plus the operations after it, the first time somebody opens it. The
alternative — restoring everything at boot — would make startup scale with total
history and load boards nobody asked for.

**Snapshots are written back every 200 accepted operations.** The log alone
always rebuilds a board, so a snapshot is an optimisation: it bounds how much
replay a restore costs. A failed snapshot write is logged and otherwise ignored.

**Truncation stays explicit.** `truncate_absorbed` exists and nothing calls it
automatically. Deleting history is a retention decision, and the store cannot
know whether a host is obliged to keep what it is about to remove — the same
reasoning that kept truncation out of ADR-0069 in the embedding host.

**Durability is always on; only its persistence varies.** With `KBOARD_DB` the
store is a file, without it an in-memory database. One code path either way, so
the configuration people run in tests cannot drift from the one they deploy.

WAL journalling with `synchronous = NORMAL`. WAL so a joining reader never
blocks a writer; `NORMAL` because losing the last few milliseconds of drawing to
an OS crash is an acceptable trade against fsyncing every stroke, and the
operation log is idempotent so a retrying client closes the gap.

### Undo history is not persisted

Item 2 depends on this answer, so it is decided here rather than discovered
later.

Undo will be implemented as a new write of a prior value with a fresh, higher
stamp (ADR-0001), which means the *effect* of an undo is an ordinary operation
and is durable like any other. What is not persisted is the per-actor stack of
what that actor could still undo.

A stack is session state. It belongs to one actor on one device, it is
meaningless to anyone else, and persisting it would raise questions the product
has no answer for: whether a returning user can undo work a collaborator has
since built on, and whether one device's undo history should apply on another.
Reconnecting therefore starts with an empty undo stack, which matches what every
comparable editor does.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Postgres | Scales past one node; matches the embedding host | An operational dependency for a single-binary product; nobody can `cargo run` it | Wrong default for a standalone server; the port makes it addable later |
| Append-only file, custom format | No dependency; trivial writes | Every read is a full scan, and crash-consistency becomes ours to get right | Rewriting a storage engine badly |
| `sqlx` with async SQLite | Non-blocking | Async I/O under a `std::sync::Mutex` needs the lock released across await points, reintroducing the interleaving this design avoids | Complexity for a write measured in microseconds |
| Write-behind via a channel | Never blocks the accept path | Acknowledges writes that have not landed; a crash loses work the client was told was saved | Trades the defect being fixed for a quieter version of it |
| Restore every board at boot | Simple; warm on first join | Startup scales with total history and loads boards nobody opens | Lazy restore is strictly better and no harder |
| Persist undo stacks | Undo survives reconnection | Undefined semantics across devices and across collaborators' later edits | Session state, and the questions it raises have no product answer yet |

## Consequences

### Positive

- A board survives the process. This is the whole point.
- Restore cost is bounded by the snapshot interval, not by board age.
- The port implementation is a template: a host swapping in Postgres writes the
  same two traits and changes nothing else.

### Negative and accepted trade-offs

- SQLite writes happen while the room lock is held, on a tokio worker thread.
  Measured against the merge work already done under that lock — 5.8 ms at
  10,000 elements per `docs/benchmarks.md` — a small append is not the
  bottleneck, but it is blocking and it is on the hot path.
- `synchronous = NORMAL` means an OS-level crash can lose the last few
  milliseconds of accepted operations. A client retry closes the gap; a client
  that never returns does not.
- Log growth is unbounded until somebody calls `truncate_absorbed`.
- One SQLite connection serialises all boards. Fine at this shape, and the
  first thing to revisit under load.

### Operational consequences

`KBOARD_DB` selects the database file; unset means in-memory and the boot
banner says so. The schema is created on open, so there is no migration step
today — the first schema change will need one.

### Security consequences

The store keys everything by opaque `ScopeId` and never parses it, exactly as
the engine does. `SnapshotStore::store` refuses a snapshot whose scope does not
match the key it is being filed under: the last place a routing bug can be
caught before one tenant's board is stored under another's key. Authentication
remains absent (ADR-0006) and durable storage does not change that — it means
an unauthenticated writer now leaves durable rather than transient state.

## Validation

- Store tests cover round-trip across close and reopen, restore equalling
  snapshot plus tail, sequence continuation, scope isolation, refusal of a
  mismatched snapshot scope, and truncation leaving the restored document
  identical.
- `scripts/restart-durability-check.mjs` proves the property end to end: a
  client draws over a WebSocket, the server is killed, a new process opens the
  same database, and a rejoining client is served the board back with its
  geometry intact. Nothing short of an actual restart demonstrates this.

## Revisit triggers

- Write latency under the room lock becomes measurable in interaction.
- A deployment needs more than one node, at which point SQLite is the
  constraint and the port is the seam to replace.
- A retention policy is agreed, enabling automatic truncation.
- Undo needs to survive reconnection, which would reopen the stack decision.
- The schema changes, requiring the migration path that does not exist yet.
