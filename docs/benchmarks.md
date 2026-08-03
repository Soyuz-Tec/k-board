# Performance baseline

Captured **2026-08-03**, before durable persistence lands. Persistence changes
the shape of every measurement after it, and the pre-change numbers cannot be
recovered once it does — that is the whole reason this comes first.

```
Machine   Intel Core Ultra 7 255H, 16 cores / 16 threads, 31 GB
OS        Windows 11 Pro
Toolchain rustc 1.97.1 (8bab26f4f 2026-07-14)
Commit    b618bd6
Command   cargo bench --workspace -- --warm-up-time 1 --measurement-time 3
```

Figures are criterion's median. Short measurement windows were used, so treat
these as the right order of magnitude rather than precise to the last digit;
what matters below is the scaling, not the third significant figure.

## Engine

| Benchmark | 100 | 1,000 | 10,000 |
|---|---:|---:|---:|
| `document/merge` | 28.8 µs | 309 µs | **5.81 ms** |
| `document/ordered` | 4.60 µs | 63.0 µs | **2.49 ms** |
| `snapshot/absorb` | 49.8 µs | 624 µs | **9.10 ms** |
| `document/json` encode | 157 µs | 1.81 ms | **28.4 ms** |
| `document/json` decode | 248 µs | 2.69 ms | **39.8 ms** |

## Client projection

| Benchmark | 100 | 1,000 | 10,000 |
|---|---:|---:|---:|
| `board/scene` | 53.1 µs | 422 µs | **10.3 ms** |
| `board/scene_json` | 66.8 µs | 903 µs | **20.5 ms** |

## Fractional indexing

| Benchmark | Time |
|---|---:|
| `frac/between` append | 38.3 ns |
| `frac/between` subdivided ×10 | 48.3 ns |
| `frac/between` subdivided ×100 | 146 ns |
| `frac/between` subdivided ×500 | 425 ns |

## What the numbers say

**The render path is the bottleneck, not the merge.** `board/scene` runs on
every change a client draws — interaction rate, not join rate — and at 10,000
elements it costs **10.3 ms** on its own. A 60 fps frame budget is 16.6 ms.
Serialising it to cross the FFI boundary doubles that to **20.5 ms**, so a
single edit on a large board exceeds one frame before anything is drawn.

This is the concrete form of a concern that was previously only an argument:
`Document::ordered` allocates a `Vec` and sorts the entire document on every
call, and `Board::scene` then allocates a fresh `SceneItem` per element. Both
scale with board size rather than with the size of the change.

**Scaling is superlinear where it hurts.** From 100 to 10,000 elements — a
100× increase:

| Path | Growth | Reading |
|---|---:|---|
| `frac/between` | ~11× | Sub-linear. Fractional indexing is not a concern |
| `board/scene` | ~194× | Roughly 2× worse than linear |
| `document/merge` | ~202× | Roughly 2× worse than linear |
| `snapshot/absorb` | ~183× | Roughly 2× worse than linear |
| `document/json` decode | ~160× | Dominated by allocation |
| `document/ordered` | **~540×** | The sort, and the worst offender |

`document/ordered` degrading 540× for a 100× increase is the clearest signal
here. It is called by `scene`, so its cost is paid on every render.

**Join cost is real but secondary.** Decoding a 10,000-element document takes
**39.8 ms**. That is a one-off on join, and it is why the server holds a
materialised snapshot rather than replaying a log — but it also means a large
board has a visible startup cost that no amount of snapshotting removes,
because the document still has to cross the wire and be parsed.

## What this does not measure

- **Rendering.** Canvas 2D drawing cost is not measured here at all. The
  numbers above are the work done *before* a single pixel is drawn.
- **Memory.** Only time is measured.
- **Concurrency.** Single-threaded throughput; no contention on the server's
  room lock.
- **Realistic edit patterns.** Elements are uniform rectangles. Freehand
  strokes carry point arrays and will serialise very differently.
- **wasm.** These are native measurements. The browser is the target that
  matters and it will be slower.

## Consequences for the roadmap

These numbers do not change the Tier-0 ordering — data loss still outranks
speed — but they make two later items concrete rather than speculative:

1. **Cache the paint order.** `ordered` re-sorts an already-sorted collection
   on every call. Maintaining sorted order incrementally, or memoising until
   the z-index set changes, addresses the single worst scaling curve.
2. **Make `scene` incremental.** Returning only what changed, instead of
   projecting and serialising the whole board, is what actually removes the
   per-frame cost. It is also a wire-format change, so it needs a decision.

Neither is worth doing before persistence, undo, and authentication. Both are
worth doing before claiming the engine handles 10,000 elements, which on these
numbers it does not — at least not at interactive frame rates.

## Reproducing

```bash
cargo bench --workspace
```

Re-run after any change to merge, projection, or serialisation, and update this
file with the machine that produced the numbers. Comparing across machines is
meaningless; comparing across commits on one machine is the point.
