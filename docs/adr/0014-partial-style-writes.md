# ADR-0014: A style write carries only what changed

- **Status:** Accepted
- **Date:** 2026-08-04
- **Owners:** Engine, Client
- **Related:** [ADR-0001](0001-per-property-convergence.md), [ADR-0009](0009-one-geometry-two-back-ends.md)

## Context

ADR-0001 established per-property convergence to remove lost updates: two people
editing different attributes of the same shape both keep their edit, because
each property is its own register.

Styling is where that guarantee is easiest to hand back by accident. A client
that sends its whole idea of an element's look on every change works perfectly
alone. On a shared board it **silently reverts a colleague's concurrent
restyle** to whatever this client happened to be holding — and the CRDT cannot
help, because both writes are genuine and the later one wins on merit.

The engine would be doing exactly what it was asked. The defect is entirely in
what was asked.

## Decision

**Every field of `Style` is optional, and a control sends only the field it
changed.**

```rust
Command::Style { id, stroke?, fill?, stroke_width?, opacity? }
```

Changing the fill must not require resending a stroke colour this client never
touched. This is what makes ADR-0001's guarantee reach the styling path rather
than stopping at the engine boundary.

**Out-of-range values are clamped or refused, and which one depends on whether
there is a sensible nearest answer.**

- **Opacity is clamped** to 0–1. A slider that overshoots by a rounding error is
  not a mistake worth failing an edit for, and every value past the range means
  the same thing as its nearest edge.
- **A stroke width of zero or less is refused.** There is no nearest sensible
  edge: a zero-width stroke is not a faint one, it is an invisible element that
  cannot be found again to be fixed. Same reasoning as the empty-label refusal
  in ADR-0012.

**Each control does two things: it restyles the selection, and it becomes the
style of the next thing drawn.** Splitting those into separate controls is the
arrangement where people restyle a shape and then wonder why the next one came
out with the old colour.

**Selecting an element adopts its style into the toolbar**, so the controls
describe what is *there* rather than the last thing drawn. Without it, nudging
the opacity slider to read the current value would instead set it.

**Opacity is omitted from an export when solid**, so the common case does not
put an attribute saying "unchanged" on every element in the file.

### The drawing surface does not follow the OS theme

The canvas background used to follow the page theme. In dark mode the default
ink `#1e1e1e` sat on a `#121212` background — a contrast ratio of about
**1.12:1**, against the 4.5:1 WCAG asks of body text and the 3:1 it asks of
graphics. Anyone opening a board in dark mode drew with ink they could not see.

The surface is now light regardless of theme, for the reason ADR-0009 gave for
exports: **a board is shared and exported, so its background cannot depend on
whose laptop is in dark mode.** The application chrome still follows the theme.

A per-board background colour is the real answer. This is the honest version of
not having one yet, and the CSS says so.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Send the whole style on every change | Simple client; one code path | Silently reverts a peer's concurrent restyle; indistinguishable from working, alone | Hands back ADR-0001's guarantee at the last step |
| A style object per element | Fewer properties; tidy model | One register for all of style, so any two concurrent style edits conflict | Precisely the granularity ADR-0001 rejected |
| Refuse out-of-range opacity | Consistent with stroke width | A slider that overshoots by 0.001 fails a legitimate edit | Values past the range have an unambiguous meaning; zero width does not |
| Clamp stroke width to a minimum | Consistent with opacity | Silently produces a hairline where the user asked for nothing | A refusal is honest where a clamp would be a guess |
| Separate "current style" and "restyle selection" controls | Explicit; no hidden coupling | Twice the toolbar, and users still expect one to imply the other | Doubles the surface to serve a distinction nobody makes |
| Keep the canvas following the OS theme | Consistent with the chrome | Default ink invisible in dark mode; a board that looks different to each viewer | An accessibility defect, not a preference |
| Add a per-board background property now | The real answer | A document-model change, a merge story, and export implications, in a styling change | Correct, and larger than this; named as the revisit trigger |

## Consequences

### Positive

- Two people restyling different attributes of the same shape both keep their
  edit — the property ADR-0001 exists for, now true end to end.
- Ink is visible in dark mode.
- Exports are smaller and cleaner in the common case.
- The toolbar always describes something real.

### Negative and accepted trade-offs

- **The client must be disciplined.** Nothing in the engine prevents a host
  from sending all four fields; the optionality is an affordance, not an
  enforcement. The check is what holds the line.
- **A bright white canvas in a dark room.** The chrome dims and the surface does
  not. This is a real cost of the fix, accepted because invisible ink is worse
  than a bright surface.
- **No per-board background colour**, so a board cannot be dark on purpose.
- Style adoption reads from a single selection only; with several selected the
  toolbar keeps its previous values rather than guessing which element speaks
  for the group.
- `Opacity` and `StrokeWidth` join `Angle` and `FontSize` as properties with a
  read default that reversal depends on (ADR-0013). That list is now four long
  and is the kind of thing that goes stale quietly.

### Operational consequences

None. Styling is an ordinary property write through the paths ADR-0007 already
established.

### Security consequences

None new. `Style` carries numbers and packed colours, all range-checked, and no
user-controlled strings.

## Validation

`scripts/style-check.mjs` in CI, through the real engine. The assertion that
matters is negative:

```
PASS  styling one property leaves the others alone
PASS  and setting it did not disturb the fill
```

Also covered: clamping in both directions, refusals leaving nothing behind,
undo restoring a value that was never explicitly written, and what an export
makes of a faded element versus a solid one.

Five Rust unit tests, including that undoing a *first* fade returns an element
to fully opaque — the case where nothing had ever written an opacity, so there
is no prior value to restore.

One check here was wrong before the code was: it asserted an undone fade
returned opacity to 1, which was only true of an element the clamping
assertions above it had not already left at 0. It captures the value it expects
now rather than assuming one.

## Revisit triggers

- A per-board background colour, which supersedes the fixed light surface and
  reopens what an export should use.
- A dark mode for the drawing surface that does not change the document —
  a rendering filter rather than a stored colour.
- More style properties (dash patterns, roughness, shadows), each of which
  extends the optional-field pattern and the read-default list.
- Style adoption from a multiple selection, which needs a rule for what a group
  of differing values reports.
