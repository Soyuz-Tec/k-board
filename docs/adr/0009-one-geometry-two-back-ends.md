# ADR-0009: Describe each shape once; render it through two back-ends

- **Status:** Accepted
- **Date:** 2026-08-04
- **Owners:** Client, Export
- **Related:** [ADR-0003](0003-raw-c-abi-over-wasm-bindgen.md), [ADR-0012](0012-host-measured-text.md), [ADR-0013](0013-box-and-turn-not-a-matrix.md)

## Context

A board that cannot leave the browser is a board nobody can put in a document.
Export was the first thing anyone asked for that the engine could not answer,
because rendering is not the engine's job.

Adding an exporter means adding a **second renderer**, and the failure mode of a
second renderer is specific and nasty: the export disagrees with the screen, and
nobody finds out until the file has already been sent to someone. A rectangle
that exports as a diamond is not a bug you catch in review; it is a bug you
catch in a meeting.

## Decision

**Each shape is described once**, in `web/scene.js`, as a sequence of path
commands and — where paths cannot express it — a small number of other figure
kinds. Both back-ends consume that description:

```
geometry(item) ──┬──► canvas  moveTo / lineTo / quadraticCurveTo / ellipse / fillText
                 └──► SVG     <path d="…"> / <ellipse> / <text><tspan>
```

There is one drawing, not two. A canvas that draws one thing and an SVG that
draws another is not a bug that can occur, because there is nothing for them to
disagree about.

**Everything the two back-ends could compute separately is computed in the
description.** Text baselines are the example that proves the rule: the first
implementation computed them in the canvas branch and again in the SVG branch,
which is not one formula used twice — it is two formulas that happen to agree
today. They are computed once now. Rotation is a transform on the description
for the same reason (ADR-0013).

**Export is the board, not the viewport.** Cropped to content, rendered at its
own scale, ignoring pan and zoom entirely. An export is the drawing; a
screenshot of where the author happened to be looking is a different artefact
that nobody asked for. The grid is omitted on the same grounds: it is an
affordance for drawing, not something anyone means to send.

**The background is always white, never the page background.** A file that comes
out dark because the author's laptop was in dark mode renders differently for
whoever receives it. Every colour in the palette is chosen to read on white.
(ADR-0014 extends this to the drawing surface itself.)

**Export is client-side.** The board never leaves the tab to become a file, so
there is nothing to upload, nothing to wait for, and it works on a purely local
board with no server at all.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| A separate SVG serialiser reading the same scene | Simple; no refactor of the canvas path | Two independent implementations of every shape; drift is a matter of when | The failure mode is silent and discovered after distribution |
| Render server-side with `lyon` → `resvg` | One renderer; thumbnails and previews come free | A network round trip for something the client can do instantly, and a second rasteriser that must match the browser's | Still two renderers, plus a server dependency — worse on both counts |
| `canvas.toDataURL` only, no SVG | Almost free; already have the pixels | Fixed resolution, no text selection, no re-editing, and it captures the viewport | PNG alone is not an export format for a diagram |
| Export the viewport | Matches "what I see"; trivial | Cropping depends on where the author was scrolled | An export is the board, not a screenshot |
| Follow the OS theme for the background | Consistent with the app chrome | The same board exports differently on two machines | A shared artefact cannot depend on the sender's theme |
| Pre-rotate geometry rather than emit a transform | Back-ends stay transform-free | Rotated glyphs cannot be expressed as rotated coordinates, so text has no answer | One transform covers every kind; pre-rotation covers all but one |

## Consequences

### Positive

- The export matches the screen by construction, and a check can prove it
  rather than assert it.
- SVG is text: searchable, diffable, re-editable, and resolution-independent.
- Adding an element kind means adding one case, not two.
- Export works offline and on a board that has never touched a server.

### Negative and accepted trade-offs

- **The description is a lowest common denominator.** Anything one back-end can
  do and the other cannot has to be modelled explicitly — which is why
  `ellipse` and `text` exist as figure kinds alongside paths, rather than
  everything being a path.
- **PNG rasterisation itself is unverified.** It uses the same `drawShape` as
  the screen, so the geometry is covered, but the transform and `toBlob` glue
  need a real canvas and no check exercises them.
- Two decimal places of precision in the SVG. Below a pixel at any sane export
  scale, and it keeps files small and stable — but it is a rounding the canvas
  does not apply, so the comparison check rounds both.
- An SVG opened where the font is missing falls back, and the box the author
  measured is then no longer the box it draws at (ADR-0012).

### Operational consequences

None. Export is client-side and touches no server state.

### Security consequences

The scene is serialised into markup, so **everything user-controlled is
escaped** — a board name in the `<title>`, and the text of every label. An
unescaped label would let a user write `</text><script>` into a file that
somebody else opens. The check feeds it angle brackets and ampersands
explicitly.

`xml:space="preserve"` is set because SVG collapses whitespace by default and
would silently reflow indentation a user typed.

## Validation

`scripts/export-check.mjs` in CI drives **both back-ends from the same scene**
and compares them command for command — the canvas through a recording context
stub, so it needs no DOM, and the SVG by parsing the commands back out of the
markup. Parsing happens in document order, because collecting text separately
compared the two in different orders and reported drift that was not there.

The check was confirmed to *fail* when the SVG emitter is deliberately made to
disagree (swapping `Q` for `L`). A consistency check that has never failed is
not yet a check.

Two assertions are less obvious than they look:

- **Stroke width is inside the crop.** A line drawn on the boundary is half
  outside the geometric box, so cropping to geometry clips it.
- **An unfilled shape is transparent rather than black.** An SVG `<path>` with
  no `fill` attribute fills black by default; omitting it turns every hollow
  rectangle solid.

## Revisit triggers

- A headless server renderer is added for thumbnails or notification previews,
  which introduces a third back-end and the same drift question at a larger
  scale.
- A shape needs an effect one back-end cannot express (blur, gradient,
  roughness), forcing either a richer description or a deliberate divergence.
- Export gains options (background, scale, selection-only), at which point the
  single-rule simplicity here is what is being traded away.
- PNG rasterisation gains logic beyond a transform, at which point it needs
  coverage rather than an argument.
