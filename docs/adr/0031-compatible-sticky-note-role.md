# ADR-0031: Compatible sticky-note role

- **Status:** Accepted
- **Date:** 2026-08-06

## Context

Sticky notes are a basic whiteboard primitive: they combine a bounded coloured
surface with editable text. Adding `sticky` to the serialized `ElementKind`
enum would be visually direct, but older browser, server, and native replicas
deserialize that enum exhaustively. One new value would therefore make a
document containing a note unreadable during a mixed-version rollout.

The document already supports namespaced custom properties specifically for
host extensions, and rectangles already carry the geometry and style a note
needs. Text is an independently convergent property and can coexist on any
element without changing the merge algorithm.

## Decision

Represent a sticky note as:

- `kind = rectangle`;
- custom property `kboard.role = sticky`;
- the existing text, font-size, geometry, stroke, fill, opacity, and z-index
  properties.

Expose the role as the optional `role` field in the flattened FFI scene. Add a
host-side `sticky` command that creates all note properties as one undoable
change. The browser renderer, editor, clipboard, canvas export, and SVG export
interpret the role consistently. The wire operation vocabulary and serialized
element-kind enum do not change.

Because this feature spans stable-named browser modules and the WebAssembly
engine, serve unversioned web assets with `Cache-Control: no-cache`. Browsers
must revalidate them on reload instead of combining files from different
release candidates. This is a compatibility control until packaging emits
content-addressed asset names. The shell also uses a one-time `v=1` URL
namespace to leave behind responses cached before revalidation became policy.

Lines remain the existing `line` element kind; this change only exposes their
already-supported model through the browser interaction and both renderers.

## Alternatives considered

| Alternative | Decision | Reason |
|---|---|---|
| Add a serialized `sticky` element kind | Rejected | Older replicas would reject the unknown enum value |
| Use `frame` as the note kind | Rejected | A frame has containment semantics a note does not have |
| Create a rectangle and text as two elements | Rejected | Move, resize, copy, delete, and undo would not be one semantic action |
| Keep notes as plain text with a presentation-only background | Rejected | The background would have no durable geometry or style to collaborate on |

## Consequences

Positive:

- a note is one independently convergent and undoable element;
- no new operation or serialized element-kind variant crosses the room-cell
  protocol;
- old replicas preserve every property and render at least the rectangular
  surface instead of failing the document;
- screen, clipboard, and exports share the same role-aware representation.

Negative and accepted trade-offs:

- an older renderer does not show the note text because it does not understand
  the role;
- scene consumers must treat `role` as the semantic refinement of `kind`;
- the initial editor auto-grows the note and does not yet provide rich-text or
  per-character formatting.

Security and operations:

- the existing text-size, operation-size, durability, scope, and resource
  limits apply unchanged;
- `kboard.role` is data, not executable markup, and both export backends escape
  text as before;
- standalone and Embedded SDK shells receive the feature from the same web
  artifact.
- unversioned modules revalidate, preventing mixed-release startup failures.

## Validation

- Rust tests prove one sticky command creates a role-bearing, styled, undoable
  rectangle with text;
- clipboard tests prove note copy/paste preserves its role and receives a new
  identity;
- export tests prove lines and notes produce matching Canvas and SVG geometry;
- rendered browser checks exercise creation, editing, selection, zoom, fit, and
  both standalone and embedded delivery.

## Revisit triggers

- protocol capability negotiation can safely introduce new element kinds;
- note layout needs collaborative rich text or independently convergent spans;
- frames gain containment rules that require a more general semantic-role
  registry.
