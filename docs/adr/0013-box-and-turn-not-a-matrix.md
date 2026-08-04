# ADR-0013: A box and a turn, not a matrix

- **Status:** Accepted
- **Date:** 2026-08-04
- **Owners:** Engine, Client
- **Related:** [ADR-0001](0001-per-property-convergence.md), [ADR-0009](0009-one-geometry-two-back-ends.md), [ADR-0012](0012-host-measured-text.md)

## Context

Selecting several elements and moving, scaling or turning them as one is the
point at which a canvas stops being a drawing toy.

It is also the point at which the element model gets asked a question it has
been avoiding. An element carries `x, y, w, h` and — newly — `angle`. That is a
box and a turn. It is **not** a matrix, and the difference has teeth:

> Stretching a tilted rectangle along the screen's x axis **shears** it. A shear
> has no representation in `x, y, w, h, angle`.

So either the model grows, or non-uniform resize of a rotated shape is
approximate, or it is refused. Choosing quietly and finding out later is the one
option that is definitely wrong.

## Decision

**Keep the box-and-turn model.** Resizing scales each element about its own
centre and leaves its angle alone: exact for uniform scaling, a deliberate
approximation otherwise. The limitation is stated at the top of
`web/transform.js`, not left to be discovered.

**One command writes a whole transform.**

```rust
Command::Geometry { id, x, y, w, h, angle?, font_size?, points? }
```

A move, a resize and a rotate sent separately would put a half-transformed shape
in the log and three entries in the history where the user made one gesture.
`Move` and `Resize` remain for the cases that genuinely change one thing.

**Rotation is a transform on the shared description, not rotated coordinates.**
Rotated glyphs cannot be expressed as rotated points, so a description that
pre-rotated its paths would have nothing to say about text (ADR-0009,
ADR-0012). One transform covers every kind.

**Hit-testing rotates the pointer backwards** into the element's frame rather
than rotating the box forwards. Cheaper, simpler, and the same answer — and
without it the clickable region drifts away from the drawn one and *grows* as
the shape turns.

**Bounds use corners, not the axis-aligned box.** A 100×100 square at 45° reaches
141.4, and an export cropped to 100 clips its corners off.

**A group rotation orbits its members.** Each element gains the same turn *and*
moves around the pivot, which is what makes rotating a selection behave like
rotating one object rather than spinning each piece where it stands.

**Freehand paths scale with their box.** The path *is* the shape; a box that
grew without it leaves the ink its original size inside a box claiming
otherwise.

**Text scales uniformly** by the smaller factor however it is dragged. The
stored box and the glyphs have to agree (ADR-0012), and a font size stretched on
one axis only leaves words that no longer fit the box recorded for them.

**A minimum extent.** Nothing can be resized to zero, because at zero it can
never be grabbed again to be made larger.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| A full 2×3 matrix per element | Exact for every transform including shear; composes cleanly | Every consumer — hit-testing, export, accessibility, the host's own renderer — must decompose it; `w`/`h` stop meaning anything simple; the Excalidraw-shaped interop surface loses its footing | A large model change to serve a gesture nobody has asked for |
| Refuse to resize a rotated shape | Never approximate; honest | The user cannot do an obviously reasonable thing, and the reason is invisible to them | Correctness the user experiences as a bug |
| Store the shear as an extra property | Cheap; keeps the box | A model that is a matrix in all but name, expressed worse | If the answer is a matrix, use a matrix |
| Resize in the element's own frame | Exact per element | A group resize then moves elements in directions unrelated to the drag | Correct for one, incoherent for many |
| Scale about the group box corner rather than each centre | Simpler arithmetic | A rotated element's corner is not where its box says, so it drifts under repeated resizes | Centre-based is exact under uniform scale, corner-based is not |
| Separate move / resize / rotate commands | Reuses what exists | Half-transformed states in the log; three history entries per gesture | The properties only make sense together |

## Consequences

### Positive

- `x, y, w, h` keep their obvious meaning for every consumer, including hosts.
- A gesture is one operation, one log entry, one undo step.
- Rotation cost nothing in the data model: `PropKey::Angle` already existed.
- The approximation is bounded and named, so nobody has to reverse-engineer it.

### Negative and accepted trade-offs

- **Non-uniform resize of a rotated shape is not exact.** It scales the
  element's box and keeps its angle. Repeated non-uniform resizes of a rotated
  selection will drift from what a true affine transform would produce.
- **Rotation is not composable with skew**, because skew is not representable.
  A skew tool would force this decision open.
- Group resize scales each element about its own centre, so relative gaps
  scale but internal composition of a rotated arrangement is approximate in
  the same way.
- Freehand paths are rewritten on every resize, so a stroke with thousands of
  points produces a proportionally large operation.

### Operational consequences

The `points` field on `Geometry` makes a freehand transform O(path length) in
the log. ADR-0006's frame cap bounds a single message; a very long stroke
resized repeatedly is the shape of write amplification to watch for.

### Security consequences

None new. `Geometry` writes only geometry, validates the element exists, and
carries no user-controlled strings.

## Validation

`scripts/transform-check.mjs` in CI — 26 assertions covering handle grabbing,
anchored and flipped drags, freehand path scaling, uniform label scaling, the
minimum extent, group orbiting versus spinning in place, rotation accumulation,
and hit-testing through a rotation.

Five Rust unit tests for `Geometry` itself.

Two defects were found in existing code while building this, and both are
pinned by checks so they cannot come back:

- **`move` wrote only x and y**, so dragging a freehand stroke moved its
  bounding box and left the ink behind — and because `bounds()` reads the path,
  it then hit-tested where it used to be. The check asserts the old behaviour
  *and* the new one, so the next person does not "fix" the command that
  replaced it back into the one that did not work.

- **`capture()` skipped absent properties**, so undoing the first rotation of a
  shape left it rotated: nothing had ever written an angle, so there was
  nothing to put back. Reversal now falls back to the same default `scene()`
  reports — not a guess, but the value the reader would have seen.

## Revisit triggers

- A skew or free-transform tool, which forces the matrix question open.
- Nested groups with their own transforms, where composition makes decomposed
  boxes genuinely painful.
- A host that stores a matrix natively and needs round-tripping.
- Complaints about drift under repeated non-uniform resize of rotated
  selections — the specific symptom of the approximation accepted here.
