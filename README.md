# k-board

A multi-tenant collaborative canvas engine, built to work equally well as its
own product and as a component inside someone else's platform.

**Status: working, early.** You can run it, draw in it, and watch two tabs
converge. The renderer is Canvas 2D rather than GPU, storage is in memory, and
the standalone deployment has no authentication yet. See
[Roadmap](#roadmap) for exactly what exists.

---

## Quick start

```bash
cargo build -p kboard-ffi --target wasm32-unknown-unknown --release
cargo run -p kboard-server
```

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
cargo test --workspace          # 75 tests
cargo clippy --workspace --all-targets -- -D warnings
node scripts/two-replica-check.mjs   # against a running server
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
- WebSocket sync server with per-room compacting log
- Browser client: shapes, freehand, select/move, erase, pan, zoom, colours
- Local-first queueing — draw offline, reconnect, replay
- DOM accessibility mirror
- Server hardening: rate limiting, frame/batch caps, room bounds, idle
  reclamation, scope validation, security headers
- Architecture decision records
- CI: fmt, clippy, tests, MSRV, wasm build, convergence proofs, e2e, audit

**Next**
- Durable persistence behind `OpLog`/`SnapshotStore` — the top gap
- Undo/redo (cheap here: rewrite the prior value with a fresh stamp)
- Authentication in the standalone server
- Rustler binding so a BEAM host can call `merge`, `snapshot`, `validate`
- Headless renderer (`lyon` → `resvg`) for server-side SVG/PNG
- GPU renderer (`wgpu`/`vello`) with a WebGL2 fallback
- Durable storage behind `OpLog`/`SnapshotStore` (currently in memory)
- Authentication in the standalone server — `authorize()` admits everyone today
- Undo/redo; text elements; images
- Framework-agnostic Web Component packaging

**Deliberately not yet decided**
- Text CRDT. Text is currently a last-writer-wins property, so concurrent edits
  to one text block keep only one. A sequence CRDT is the fix; it is not free,
  and the current behaviour is honest and documented rather than silently wrong.

## Not production

- **`authorize()` in `kboard-server` returns `true` for everyone.** It is marked
  as the seam where a real deployment authenticates. This is the single reason
  the server cannot face a public network.
- **Rooms are in memory.** Restarting the server loses every board.

Resource limits *are* enforced — frame size, frame rate, operations per batch,
elements per room, room count, scope charset and length, and idle-room
reclamation. See [ADR-0006](docs/adr/0006-server-resource-limits.md) for the
values and why each exists. That closes the denial-of-service routes; it does
not substitute for authentication.

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
