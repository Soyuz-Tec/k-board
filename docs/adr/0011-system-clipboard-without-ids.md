# ADR-0011: Copy through the system clipboard, with the element id dropped

- **Status:** Accepted
- **Date:** 2026-08-04
- **Owners:** Client
- **Related:** [ADR-0001](0001-per-property-convergence.md), [ADR-0009](0009-one-geometry-two-back-ends.md)

## Context

Copy and paste is the least interesting feature on any roadmap right up until
the moment someone wants the same diagram on two boards.

Two decisions in it are genuinely load-bearing, and both have a version that
looks correct in a demo and is wrong in use.

## Decision

### The payload goes on the system clipboard

JSON text, marked with `k-board/clipboard`, written through
`navigator.clipboard`.

An in-tab variable would be simpler and would behave identically in every demo:
copy, paste, works. It fails the first time it matters — a second board, a
second tab, or a reload — which is the whole reason anyone copies rather than
redrawing.

`navigator.clipboard` needs a secure context and can be refused by the user, so
there is a documented in-tab fallback. The status line says which one was used
rather than pretending they are the same thing.

### The element id is dropped

An element id names a **place in the document**, not a shape. A copy that keeps
its id is not a copy: pasting it rewrites the original, so what looks like
"paste" *moves the thing you copied*.

The board still converges, still persists, still undoes. It is simply wrong —
and only visibly wrong after the original has already moved, which is the worst
moment to discover it.

### A paste is an ordinary edit

Copied elements are turned back into the `add` / `stroke` / `text` commands that
would have created them. A paste therefore merges, replicates and undoes with no
special case anywhere, and the copy gets a new identity by construction rather
than by remembering to strip something.

### Paste centres on the pointer

The original coordinates mean nothing on a board that has since been panned, or
on a different board entirely. Centring on the pointer is the only rule that is
predictable across all three cases. With no pointer — a keyboard-driven
duplicate — the content is nudged instead.

### Foreign clipboard content is ignored, not reported

The clipboard belongs to the whole machine and most of what is on it was never
meant for this board. Text that is not a k-board payload is not an error and
must not be surfaced as one.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| In-tab clipboard variable | No permissions, no async, works everywhere | Does not cross tabs, boards, or reloads | Identical in a demo, useless in practice |
| Keep ids and remap on paste | Preserves references between copied elements | Needs a remap pass, and a bug in it silently edits the originals | Dropping the id makes the failure impossible rather than caught |
| A custom clipboard MIME type | Other applications would not see JSON | Poorly supported outside Chromium; `text/plain` works everywhere | Portability beats tidiness for a format nobody else reads |
| Put a rendered image on the clipboard | Pastes into documents and chat directly | Not re-editable; a one-way export, not a copy | That is what ADR-0009 export is for |
| Server-side clipboard keyed by user | Works across devices, not just tabs | Server state, an identity model, and a lifetime policy for something that lives for seconds | Enormous cost for a marginal case |
| Report unrecognised clipboard content | Explains why nothing happened | Fires whenever the user copied anything else — which is most of the time | Noise indistinguishable from a real fault |

## Consequences

### Positive

- A shape can be carried between boards, tabs, and reloads, which is the point.
- The paste path has no special case in the engine, the log, or undo.
- Pasting cannot corrupt the thing that was copied, structurally.
- Copy also works on a purely local board with no server.

### Negative and accepted trade-offs

- **The payload is `text/plain`**, so pasting into a text editor yields JSON.
  Acceptable; the alternative is a MIME type most browsers ignore.
- **Nothing can be pasted *into* k-board from another application.** No image
  paste, no SVG paste, no text paste. The parser deliberately rejects
  everything unrecognised, which forecloses that until it is designed.
- **The in-tab fallback is silently worse.** It is named in the status line,
  but a user who does not read it will find the copy did not reach their other
  tab.
- Copied elements lose their relative z-order against elements that were not
  copied; they are re-added on top.

### Operational consequences

None. The clipboard is client-side and touches no server state.

### Security consequences

The clipboard is a **shared, untrusted channel** — anything on the machine can
put anything there. `parse()` is therefore hardened as a parser of hostile
input, not as a deserialiser of our own data: it rejects non-strings, malformed
JSON, foreign payloads, non-array element lists, and elements whose coordinates
are not finite.

That last one matters beyond tidiness: an element pasted at a `NaN` position
renders nowhere and can never be selected again to be deleted.

Reading the clipboard requires a user gesture and may prompt. k-board never
reads it except in response to an explicit paste.

## Validation

`scripts/clipboard-check.mjs` in CI runs the round trip **through the real
engine**, because the claim is about identity and only the engine can settle it:

```
PASS  a paste adds an element rather than editing one
PASS  the copy has its own identity
PASS  and the original has not moved
PASS  deleting the original leaves the copy alone
```

That last assertion is the clearest statement that the two are genuinely
separate elements rather than two views of one.

The parser is fed empty strings, prose, truncated JSON, someone else's payload,
`null`, `undefined`, and a number. None may throw and none may paste.

## Revisit triggers

- Pasting *into* k-board from other applications — images, SVG, plain text —
  which needs a designed inbound format rather than a hardened rejection.
- Copied elements needing to preserve references to each other (arrows bound to
  shapes, groups), which is the case where dropping ids stops being free and a
  remap becomes genuinely necessary.
- Preserving relative z-order across a copy.
- A clipboard format version bump, which the `version` field anticipates and
  nothing yet reads.
