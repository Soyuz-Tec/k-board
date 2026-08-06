# k-board

A multi-tenant collaborative canvas engine, built to work equally well as its
own product and as a component inside someone else's platform.

**Status: working, early.** You can run it, draw in it, and watch two tabs
converge. The renderer is Canvas 2D rather than GPU; SQLite durability and
scoped bearer authentication are available, while loopback development may
still run in memory without authentication. See [Roadmap](#roadmap) for exactly
what exists.

---

## Quick start

```bash
cargo build -p kboard-ffi --target wasm32-unknown-unknown --release
KBOARD_DB=boards.db cargo run -p kboard-server
```

Without `KBOARD_DB` the store is in-memory and boards vanish on restart; the
boot banner tells you which mode you are in.

To run it anywhere other than loopback, authentication is required:

```bash
export KBOARD_SECRET="something long and random"
cargo run -p kboard-server -- --token my-board   # prints a grant
KBOARD_BIND=0.0.0.0 \
KBOARD_ALLOWED_ORIGINS=https://board.example.com \
cargo run -p kboard-server
```

Then open `/?token=<grant>`. A grant opens one scope and expires; it travels as
a WebSocket subprotocol rather than a query parameter on the socket URL.

Open <http://127.0.0.1:8080> in **two tabs**. Each tab is an independent
replica with its own copy of the document. Draw in either one.

## Why this exists

Existing open-source canvases are excellent drawing surfaces and incomplete
collaboration systems. Two gaps recur:

**They lose edits.** Editors that resolve conflicts per *element* — Excalidraw's
`version`/`versionNonce` rule is the well-known example — discard one of two
concurrent edits whenever two people touch the same shape. One person drags a
rectangle while another recolours it, and one of those edits silently vanishes.
There is no conflict signal; the user simply finds their work undone.

**They assume they are the application.** An engine that decides identity,
tenancy, permission, and storage for itself cannot be embedded in a platform
that already owns those things. Integrating it means either forking it or
running a second authority for your own data.

k-board fixes both by construction.

## The two design commitments

### Conflict resolution is per property, not per element

Every property is an independently convergent register stamped with a hybrid
logical clock. Two people editing different attributes of the same shape never
contend, so both edits survive. Two people editing the *same* attribute resolve
deterministically — every replica picks the same winner, in any arrival order.

This is verified end to end against a running server, not just in unit tests:

```
PASS  replicas converge after concurrent edits
PASS  the move survived
PASS  the concurrent restyle also survived
```

### The engine holds no ambient authority

It cannot read a clock, resolve an identity, decide a permission, or reach a
database. Each is a trait the host implements:

| Port | Standalone | Embedded in a host |
|---|---|---|
| `PhysicalClock` | System time | Host's clock |
| `Authority` | Own RBAC | Host's membership check |
| `OpLog` | Own storage | Host's existing table |
| `SnapshotStore` | Own storage | Host's storage |

The engine cannot tell the difference, and that is the point. The day it can, it
has stopped being embeddable.

Tenancy is enforced in exactly one place: a document carries an opaque
`ScopeId` and *refuses* to merge with a document from another scope. The engine
never parses that identifier — it only compares it.

## Architecture

[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) is the architecture source of
truth. The dependency, runtime, deployment, quality and risk views there govern
the ordered room-cell transformation.

Public deployment policy, enforced limits and incident signals are documented
in [`docs/security/deployment-and-incidents.md`](docs/security/deployment-and-incidents.md);
the repository threat model is
[`docs/security/threat-model.md`](docs/security/threat-model.md).

```
crates/kboard-core     safe, forbid(unsafe_code) — the convergent document model
crates/kboard-ffi      the only unsafe code — C ABI, panic-trapped at every export
crates/kboard-server   standalone adapter — WebSocket fan-out, compacting op log
web/                   Canvas 2D client, no bundler, no npm
scripts/               end-to-end convergence check
```

One crate, four targets:

```
kboard-ffi ──┬── wasm32          → browsers          (359K, no imports, no JS glue)
             ├── cdylib (C ABI)  → BEAM/Rustler, PyO3, napi-rs, JVM, .NET
             ├── staticlib       → hosts that link statically
             └── native          → the standalone server
```

Client and server run **the same machine code** for merge. The largest source of
bugs in collaborative editors is two implementations of the merge rule drifting
apart; this removes the category rather than testing for it.

### The FFI rule

No panic ever crosses the boundary. A panic unwinding into a foreign runtime
does not fail one call — it can abort the host process. Every export is wrapped
in `catch_unwind`, boards live in a process-global registry (a BEAM NIF may open
a board on one scheduler thread and call it from another), and mutex poisoning
is recovered from rather than propagated.

## Verification

```bash
node scripts/architecture-check.mjs
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
node scripts/two-replica-check.mjs   # against a running server
node scripts/protocol-v2-check.mjs    # negotiated ack/identity/mixed-version contract
node scripts/storage-health-check.mjs # schema/WAL/writer operational contract
```

These are laws, not samples. A CRDT that converges only for the orderings you
happened to try is not a CRDT.

| Property | How it is checked |
|---|---|
| Order independence | Every one of 5,040 orderings of a contended log yields one document |
| Associativity | All groupings of three replicas agree |
| Idempotence | Replaying any log twice changes nothing |
| Partition healing | Three isolated replicas gossip in arbitrary order and converge |
| No lost updates | 20 actors write 20 properties of one element concurrently; all 20 survive |
| Snapshot fidelity | `snapshot + tail` equals full replay, under every ordering |
| Tenant isolation | Cross-scope merge is refused and does not partially apply |
| Wire stability | Documents and op logs round-trip through JSON, including custom keys |
| End to end | Two live replicas over WebSocket converge with both concurrent edits intact |
| Durable delivery | Protocol v2 ack loss, duplicate retry, actor forgery, clock skew and v1/v2 overlap are exercised live |
| Exact recovery | Migrations, snapshot/log races, corrupt snapshots, retention and busy/read-only/full storage faults are deterministic tests |
| Refusal atomicity | An over-limit batch is rejected whole and never partially applied |
| Abuse resistance | Scope charset rejects traversal and injection; rate limiter throttles bursts and disconnects floods |

## Accessibility

The client mirrors the scene into a live DOM list, not just pixels. Canvas
content carries no semantics, so a canvas-only app is unreadable to assistive
technology no matter how good its keyboard handling is.

The mirror is deliberately **not** driven by `requestAnimationFrame` — rAF is
paused entirely in a background tab, and a screen reader user does not need
pixels to be painting for content to be readable.

## Roadmap

**Done**
- Convergent document model with per-property merge
- Hybrid logical clocks, deterministic tiebreaking, fractional z-ordering
- Snapshot compaction and tombstone collection
- Authority ports; tenant scope isolation
- C ABI with panic trapping; wasm32 and native from one crate
- WebSocket sync server with one bounded ordered room cell per active scope
- Browser client: shapes, freehand, select/move, erase, pan, zoom, colours,
  text, undo/redo with Ctrl+Z / Ctrl+Shift+Z
- IndexedDB outbox — retain stable batch/replica identity through offline work,
  reload, reconnect, durable acknowledgement and recovery export
- DOM accessibility mirror
- Server hardening: rate limiting, frame/batch caps, room bounds, idle
  reclamation, scope validation, security headers
- Architecture decision records
- Performance baseline ([`docs/benchmarks.md`](docs/benchmarks.md)) captured
  before persistence changes it
- Dependency policy via `cargo-deny` — licences, duplicates, and sources
- Durable persistence: SQLite behind the engine's `OpLog`/`SnapshotStore`
  ports, with boards restored on join ([ADR-0007](docs/adr/0007-sqlite-durable-storage.md))
- Negotiated protocol v2 with stable scope-bound replica actors, idempotent
  batch append, durable sequence acknowledgements and typed refusals; v1 stays
  available during the documented compatibility window
- Undo/redo, per actor, as fresh writes of prior values — so a reversal
  converges like any other edit and reaches collaborators
- Per-scope bearer authentication, enforced on the WebSocket handshake, with a
  loopback-only refusal when no secret is set
- Client-side PNG and SVG export, cropped to the board rather than the
  viewport, from one geometry description shared with the on-screen renderer
- Presence: live peer cursors, relayed but never merged, stored, or logged
- Copy, cut, paste, duplicate and delete — on the *system* clipboard, so a
  shape can be carried between boards, tabs, and reloads
- Keyboard selection: Tab cycles, Escape clears, and every clipboard shortcut
  acts on the result
- Text elements, edited in place through a real textarea, with the measured box
  stored on the element because the engine has no font
- Multi-select by marquee or shift-click; move, resize and rotate a selection
  as one thing
- Stroke colour, fill, width and opacity, applied to the selection and to what
  is drawn next — each control sending only what it changed
- CI: fmt, clippy, tests, MSRV, wasm build, convergence proofs, e2e, audit,
  dependency policy, benchmark compilation
- Separate liveness/readiness/diagnostics, bounded process drain, verified
  online SQLite backup and isolated restore drills

**Next**
- Rustler binding so a BEAM host can call `merge`, `snapshot`, `validate`
- Presence: shared selection, and names rather than actor ids
- Images
- Headless renderer (`lyon` → `resvg`) so a *server* can render a board without
  a browser — the client-side export above does not cover thumbnails or
  notification previews
- GPU renderer (`wgpu`/`vello`) with a WebGL2 fallback
- Framework-agnostic Web Component packaging

**Measured, not yet addressed**
- `board/scene` costs **10.3 ms** at 10,000 elements, and **20.5 ms** once
  serialised — past a 60 fps frame budget before anything is drawn. The
  projection re-sorts and re-allocates the whole board on every change.
  See [`docs/benchmarks.md`](docs/benchmarks.md).

**Deliberately not yet decided**
- Text CRDT. Text is currently a last-writer-wins property, so concurrent edits
  to one text block keep only one. A sequence CRDT is the fix; it is not free,
  and the current behaviour is honest and documented rather than silently wrong.

## Not production

- Authentication is opt-in: set `KBOARD_SECRET` and mint per-scope grants with
  `--token <scope>`. Without a secret the server refuses to bind anything but
  loopback, so running open is a development convenience rather than an
  unauthenticated writable store on a network.
- Boards are durable when `KBOARD_DB` is set; without it the store is
  in-memory and they are lost on restart. The boot banner says which.

Resource limits *are* enforced — frame size, frame rate, operations per batch,
elements per room, room count, scope charset and length, and idle-room
reclamation. See [ADR-0006](docs/adr/0006-server-resource-limits.md) for the
values and why each exists. That closes the denial-of-service routes; it does
not substitute for authentication.

Production lifecycle commands, probes, alerts, capacity and recovery objectives
are in [`docs/operations/production-lifecycle.md`](docs/operations/production-lifecycle.md).
The supported topology is one authoritative server process; ADR-0028 explains
why a second writer requires leased scope ownership and storage fencing first.

## Decisions

Non-obvious choices are recorded in [`docs/adr/`](docs/adr/README.md) — including
why conflicts resolve per property, why there is no `Clear` operation, why the
FFI is a raw C ABI, and why rooms no longer hold an operation log.

## Licence

MIT.

Permissive by intent. For an engine meant to be embedded, adoption is the moat —
a production licence key is precisely what makes an otherwise better engine the
worse dependency.

Worth revisiting before any public release: Rust crates conventionally
dual-licence `MIT OR Apache-2.0`, because Apache-2.0 carries an explicit patent
grant that MIT does not. That matters for an engine intended to be embedded by
other companies.
