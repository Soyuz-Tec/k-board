# ADR-0012: The host measures text; the engine stores the box

- **Status:** Accepted
- **Date:** 2026-08-04
- **Owners:** Engine, Client
- **Related:** [ADR-0001](0001-per-property-convergence.md), [ADR-0003](0003-raw-c-abi-over-wasm-bindgen.md), [ADR-0009](0009-one-geometry-two-back-ends.md)

## Context

`kboard-core` already modelled text — `ElementKind::Text`, `PropKey::Text` and
`FontSize`, `PropValue::Text` with a size cap. None of it was reachable, because
the FFI command surface had no way to create or edit a label.

The obstacle is not the data model. It is that **text has a size only once you
know the font**, and the engine deliberately has none. `kboard-core` is
`#![forbid(unsafe_code)]` with no I/O, no clock and no randomness (ADR-0007);
adding a font engine and a shaper would be by far the largest thing in it, and
would exist to answer a question the renderer already knows the answer to.

Every consumer needs the box: hit-testing, selection, marquee intersection,
export cropping, and the transform arithmetic in ADR-0013. Deriving it on demand
means every one of them needs a font.

## Decision

**The host supplies the box; the engine stores it.** The client measures with
the same canvas context, at the same size, in the same font it will draw with —
the only arrangement in which the stored box is the box the words actually
occupy.

```rust
Command::Text    { x, y, w, h, text, font_size, stroke }
Command::SetText { id, text, w, h }
```

**Content and box are written together, always.** `SetText` takes both. A label
whose box no longer fits hit-tests to the wrong region and exports cropped
through the middle of the words. A peer that never measured this font could not
derive the new box for itself, so it has to travel with the content on every
edit.

**Two things are refused rather than accepted quietly:**

- **Text that is empty or only whitespace.** An element that draws nothing
  cannot be selected again to be fixed, and cannot be seen to be deleted.
- **Text over `MAX_TEXT_BYTES`.** The engine drops an oversized value on write
  rather than storing it, so without an explicit refusal the host would be told
  its edit succeeded and then find an empty label.

Trailing newlines are trimmed — a textarea hands back whatever the user left
behind, and a label ending in a blank line measures taller than it looks.
Leading and interior whitespace is kept, because someone who indented a line
meant to. This is the same reason the SVG export sets `xml:space="preserve"`
(ADR-0009).

**A shape reports `text: None`, not `Some("")`.** A host has to be able to tell
a label containing an empty string from a rectangle, and the distinction is the
option itself.

**The editor is a real `<textarea>`**, positioned over the canvas, not a caret
drawn on it. It brings selection, an IME, spellcheck, screen-reader support and
the platform's own text conventions with it — all of which would be worse
reimplemented and wrong in most cases.

**A generic font family**, not a specific face. An SVG opened on a machine
without the font falls back to something, and the width the author measured is
then no longer the width it draws at. Naming only what every system has keeps
the measured box honest in more places than naming a favourite would.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Embed a font and shaper in the engine (`cosmic-text`, `rusttype`) | Self-contained; a box derivable anywhere, including server-side | The largest dependency in a crate that has none; fonts to ship or discover; still would not match what the browser draws | Answers a question the renderer already answered, differently |
| Derive the box on each client at render time | Nothing to store; always matches the local font | Every consumer needs a font engine; two clients with different fallbacks silently disagree about hit regions | Moves the problem to five call sites instead of one |
| Store no box; treat text as a point | Simplest data model | No hit-testing, no marquee, no selection outline, no export crop | Text would not be a first-class element |
| Store the *text metrics* rather than a box | More precise; supports better layout later | A richer contract across the FFI boundary that only one host can produce | A box is what every consumer actually asks for |
| Write content and box in separate commands | Smaller commands; reuses `Resize` | A window in which the label's box does not match its words, and two history entries for one edit | The two only make sense together |
| Draw the caret and selection on the canvas | Full visual control; no DOM overlay | Reimplementing IME, spellcheck, accessibility and platform text conventions | Worse in every case, wrong in most |

## Consequences

### Positive

- `kboard-core` is unchanged. Text needed no engine work at all, which is a
  reasonable sign the property model was right.
- Every consumer reads one number instead of measuring.
- The editor is accessible, IME-capable and spellchecked because it is a real
  form control.

### Negative and accepted trade-offs

- **The engine trusts the host's arithmetic.** A host that measures badly
  stores a wrong box, and nothing here can detect it. This is the actual price
  of the decision.
- **Two clients with different font fallbacks disagree.** The one that typed
  the label wins, because its measurement is the one that persisted. The
  generic family narrows this but does not close it.
- **A stored box goes stale if the font stack changes** — a system update, a
  different platform. The words re-render at a new width inside an old box.
- **The editor and the canvas do not align perfectly.** A textarea centres each
  line in its line box using the font's own metrics, which nothing outside the
  browser can read, so the two conventions agree within about a pixel rather
  than exactly. The CSS says so rather than claiming otherwise.
- Rich text, wrapping, and alignment are all out of scope, and a stored box is
  the wrong primitive for wrapping.

### Operational consequences

None. Text is an element like any other: it merges, persists and restores
through the paths ADR-0007 already established.

### Security consequences

`MAX_TEXT_BYTES` bounds a single label, and ADR-0006's frame and batch caps
bound how many arrive. Label content is user-controlled and reaches two
untrusted sinks: the DOM accessibility mirror, where it is set as `textContent`
rather than markup, and the SVG export, where it is escaped (ADR-0009).

## Validation

Eight unit tests: round trip off the scene, box-moves-with-content, undo
restoring both together, the empty and oversized refusals, a trailing newline
not becoming part of the label, a missing element, and that a *shape* reports
`text: None` rather than `Some("")`.

One of these caught a real gap: `"   \n"` trimmed to `"   "`, which is not empty
by a naive check and draws nothing on the board — precisely the element the
refusal exists to prevent. Emptiness is judged after trimming now.

`scripts/two-replica-check.mjs` proves it over the wire: a label replicates,
`set_text` converges back, **the box the author measured travels with it**, and
it deletes like anything else.

## Revisit triggers

- A server-side renderer for thumbnails, which needs a shaper anyway and would
  make an engine-side measurement suddenly cheap to add.
- Font choice becoming a document property rather than a constant, at which
  point stored boxes must be invalidated when it changes.
- Wrapping, alignment or rich text, none of which a fixed box expresses.
- A second host that measures text differently from the browser — the case
  where the trust this record extends is actually tested.
