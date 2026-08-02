# k-board

A multi-tenant collaborative canvas engine, built to work equally well as its
own product and as a component inside someone else's platform.

**Status: early. The document engine is complete and tested. The renderer, the
UI shell, and the sync server are not written yet.** See [Roadmap](#roadmap) for
what exists and what does not.

---

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

```rust
// Alice drags it. Bob recolours it. Neither has seen the other.
alice.merge(&bob)?;
bob.merge(&alice)?;

assert_eq!(alice, bob);                          // they converge
assert_eq!(shape.num(PropKey::X), Some(250.0));  // the drag survived
assert_eq!(shape.get(&PropKey::Stroke), ...);    // so did the recolour
```

### The engine holds no ambient authority

It cannot read a clock, resolve an identity, decide a permission, or reach a
database. Each is a trait the host implements:

| Port | Standalone | Embedded in a host |
|---|---|---|
| `PhysicalClock` | System time | Host's clock |
| `Authority` | Own RBAC | Host's membership check |
| `OpLog` | Own Postgres | Host's existing table |
| `SnapshotStore` | Own storage | Host's storage |

The engine cannot tell the difference, and that is the point. The day it can, it
has stopped being embeddable.

Tenancy is enforced in exactly one place: a document carries an opaque
`ScopeId` and *refuses* to merge with a document from another scope. The engine
never parses that identifier — it only compares it.

## One crate, four targets

```
kboard-core ──┬── wasm32          → browsers
              ├── cdylib (C ABI)  → BEAM/Rustler, PyO3, napi-rs, JVM, .NET
              ├── staticlib       → hosts that link statically
              └── native          → the standalone sync server
```

Client and server therefore run **the same machine code** for merge. The largest
source of bugs in collaborative editors is two implementations of the merge rule
drifting apart; this removes the category rather than testing for it.

## Try it

```bash
cargo test
```

58 tests, including a 5,040-case exhaustive permutation proof.

## What is proven

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

## Layout

```
crates/kboard-core/src/
  clock.rs      Hybrid logical clock — the total order everything rests on
  lww.rs        Last-writer-wins register
  prop.rs       Property keys and values
  element.rs    Element as a map of independently convergent properties
  document.rs   Convergent element map, scoped to one tenant
  frac.rs       Fractional z-index — concurrent reorder without renumbering
  op.rs         Operations, apply, and why there is no `Clear` operation
  snapshot.rs   Compaction, so joining is not O(history)
  ports.rs      Everything the engine refuses to decide for itself
```

## Roadmap

**Done**
- Convergent document model with per-property merge
- Hybrid logical clocks and deterministic tiebreaking
- Fractional z-ordering
- Snapshot compaction and tombstone collection
- Authority ports with in-memory implementations
- Tenant scope isolation
- JSON wire format
- Exhaustive convergence proofs

**Next**
- FFI surface: `#[no_mangle]` C ABI exports with panic trapping at the boundary
- Rustler binding, so a BEAM host can call `merge`, `snapshot`, and `validate`
- Headless renderer (`lyon` tessellation → `resvg`) for server-side SVG/PNG
- GPU renderer (`wgpu`/`vello`) with a WebGL2 fallback
- TypeScript shell as a framework-agnostic Web Component
- DOM-projected accessibility tree — the scene graph as live ARIA, not pixels
- Standalone sync server

**Deliberately not yet decided**
- Text CRDT. Text is currently a last-writer-wins property, so concurrent edits
  to one text block keep only one. A sequence CRDT is the fix; it is not free,
  and the current behaviour is honest and documented rather than silently wrong.

## Licence

MIT.

Permissive by intent. For an engine meant to be embedded, adoption is the moat —
a production licence key is precisely what makes an otherwise better engine the
worse dependency.

Worth revisiting before any public release: Rust crates conventionally
dual-licence `MIT OR Apache-2.0`, because Apache-2.0 carries an explicit patent
grant that MIT does not. That matters for an engine intended to be embedded by
other companies.
