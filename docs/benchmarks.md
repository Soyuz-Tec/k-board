# Performance

Re-measured **2026-08-04**, after persistence, undo, text, transforms and
styling. The baseline column is the run of **2026-08-03**, taken deliberately
before durable persistence landed so that this comparison would be possible.

```
Machine   Intel Core Ultra 7 255H, 16 cores / 16 threads, 31 GB
OS        Windows 11 Pro
Toolchain rustc 1.97.1 (8bab26f4f 2026-07-14)
Commit    158606c   (baseline: b618bd6)
Command   cargo bench --workspace -- --warm-up-time 1 --measurement-time 3
```

Figures are criterion's median. Short measurement windows were used in both
runs, so **treat a change under about 25% as noise**. What matters below is
scaling and orders of magnitude, not the third significant figure — and where a
change is called out, it is because it is both large *and* explicable.

## Engine

| Benchmark | 100 | 1,000 | 10,000 | Baseline at 10,000 |
|---|---:|---:|---:|---:|
| `document/merge` | 26.1 µs | 326 µs | **6.18 ms** | 5.81 ms |
| `document/ordered` | 4.51 µs | 49.4 µs | **2.44 ms** | 2.49 ms |
| `snapshot/absorb` | 47.5 µs | 884 µs | **8.87 ms** | 9.10 ms |
| `document/json` encode | 179 µs | 2.27 ms | **25.3 ms** | 28.4 ms |
| `document/json` decode | 344 µs | 3.29 ms | **26.7 ms** | 39.8 ms |

Nothing in `kboard-core`'s document model changed in items 3–9, and these
numbers agree with that. The decode figure moved 33%, which is larger than the
rest and has no change to attribute it to; it is recorded as unexplained rather
than given a story.

## Client projection

| Benchmark | 100 | 1,000 | 10,000 | Baseline at 10,000 | Change |
|---|---:|---:|---:|---:|---:|
| `board/scene` | 40.2 µs | 531 µs | **12.7 ms** | 10.3 ms | **+23%** |
| `board/scene_json` | 74.1 µs | 790 µs | **25.7 ms** | 20.5 ms | **+25%** |

This one *is* explicable, and it was predictable. `SceneItem` gained four fields
across items 7–9 — `text`, `font_size`, `angle`, `opacity` — and every one is
projected and serialised for every element on every call.

## Fractional indexing

| Benchmark | Time | Baseline |
|---|---:|---:|
| `frac/between` append | 41.1 ns | 38.3 ns |
| `frac/between` subdivided ×10 | 49.6 ns | 48.3 ns |
| `frac/between` subdivided ×100 | 156 ns | 146 ns |
| `frac/between` subdivided ×500 | 338 ns | 425 ns |

## Durable storage

New. This is the measurement the 2026-08-03 baseline existed to make possible.

| Benchmark | Time | Paid |
|---|---:|---|
| `store/append` one edit | **101 µs** | every accepted batch, under the lock |
| `store/append` drag commit (10 ops) | **416 µs** | every 50 ms during a drag |
| `store/append` freehand (100 ops) | **1.65 ms** | once per pen stroke |
| `store/snapshot` 100 elements | 199 µs | every 200 operations |
| `store/snapshot` 1,000 elements | 3.25 ms | every 200 operations |
| `store/snapshot` 10,000 elements | **163 ms** | every 200 operations |
| `store/restore` 100 elements | 1.05 ms | first join after a restart |
| `store/restore` 1,000 elements | 3.62 ms | first join after a restart |
| `store/restore` 10,000 elements | **92.4 ms** | first join after a restart |

Measured against an on-disk database, not `:memory:`. An in-memory store
measures serialisation and nothing else, which is the part that was never in
question.

### Gate 3 exact-snapshot remeasurement

Re-measured **2026-08-05** after versioned migrations, exact captured coverage,
atomic snapshot/prefix truncation and WAL checkpoint policy. This is a single
local run on the same Intel Core Ultra 7 255H / Windows 11 Pro machine using
Rust 1.97.1 and:

```bash
cargo bench -p kboard-store --bench store -- --noplot
```

| Benchmark | Criterion median |
|---|---:|
| append, one shape | 131 µs |
| append, ten shapes | 478 µs |
| append, one hundred shapes | 2.15 ms |
| snapshot, 100 shapes | 255 µs |
| snapshot, 1,000 shapes | 9.47 ms |
| snapshot, 10,000 shapes | **631 ms** |
| restore, 100 shapes | 1.25 ms |
| restore, 1,000 shapes | 6.26 ms |
| restore, 10,000 shapes | **102 ms** |

These are comparative baselines, not hard CI thresholds. The large-snapshot
result reinforces the architectural choice in ADR-0022: one bounded writer,
one in-flight durable request per scope, coalesced snapshot candidates and FIFO
cross-scope admission. Gate 4 now implements that bounded writer and removes the
former global document lock.

### Gate 4 room-cell burst baseline

Measured **2026-08-05** on the same Windows workstation with the deterministic
room-cell burst test:

```text
cargo test -q -p kboard-server \
  room_cell::tests::a_legitimate_sixty_four_command_burst_drains_within_the_deadline \
  -- --exact --nocapture
```

Five isolated debug-test runs drained a 64-command FIFO burst in 234, 285, 220,
201 and 249 microseconds (median **234 µs**). The burst contains one command per
maximum admitted scope connection and is accepted while the cell is deliberately
paused; command 65 receives typed overload in the adjacent isolation test. This
is scheduler evidence, not a storage latency SLO. Capacity 64 is therefore the
initial measured legitimate burst envelope and must be revisited with production
traffic histograms rather than silently increased.

## What the numbers say

### Gates 8–10 isolation, SLO and capacity qualification

Re-measured **2026-08-05** on the same Windows workstation. Raw summary:
[`docs/benchmarks/raw/2026-08-05-gates-8-10.json`](benchmarks/raw/2026-08-05-gates-8-10.json).

```text
cargo bench -p kboard-ffi --bench scene -- ffi --warm-up-time 1 --measurement-time 3 --noplot
node scripts/wasm-ffi-benchmark.mjs
node scripts/room-cell-slo-check.mjs
node scripts/single-process-capacity-check.mjs
```

| Workload | Median / p99 | Result |
|---|---:|---|
| Native FFI scene, 1,000 elements | 850 µs median | Per-handle path |
| Two native scenes, serialized | 1.459 ms median | Identical two-call comparison baseline |
| Two independent native scenes, parallel | 1.168 ms median | 20% lower wall time including thread creation |
| Wasm FFI scene, 1,000 elements | 944 µs median / 1.714 ms p99 | 100 samples |
| Eight-scope durable load | 9.496 ms cold cross-scope p99 | Under 250 ms shared-runner objective |
| 64 simultaneously writing scopes | 31.247 ms cold p99 | Qualified on this debug build/machine |
| 96 simultaneously writing scopes | bounded overload | Storage mailbox protected the process |

The capacity result is a measured operating envelope, not a universal product
ceiling. It identifies the single storage writer/64-entry queue as the first
boundary under a synchronized first wave. ADR-0028 therefore retains one
process and makes sustained legitimate overload a topology-revisit trigger.

The 250 ms p99 and 1 s maximum CI checks are intentionally wide enough for
shared hardware. Criterion microbenchmarks remain report-only because shared
runner timings are not stable enough for a meaningful narrow threshold.

### The durable write dominates the critical section, and ADR-0007 said otherwise

ADR-0007 accepted a blocking SQLite append inside the room lock, arguing:

> Measured against the merge work already done under that lock — 5.8 ms at
> 10,000 elements per `docs/benchmarks.md` — a small append is not the
> bottleneck.

That comparison was against the wrong number. `document/merge` at 10,000 is the
cost of merging **90,000 operations** at once; it is not what one edit costs.
`upsert` emits one operation per property, so 10,000 elements is 90,000
operations and the per-operation cost is about 0.07 µs. A **creation** — the
most expensive kind of edit, nine properties — therefore merges in about 0.6 µs.
A move writes two properties and costs a fifth of that. The comparison below
uses the expensive case, because it is the one most favourable to the argument
being disputed.

| Under the lock, for one ordinary edit | Cost |
|---|---:|
| Merge into a 10,000-element document | ~0.6 µs |
| Durable append | **101 µs** |

**The append is about 160× the merge it was compared against.** It is
not a rounding error on top of existing work; it *is* the work.

The decision to be durable before broadcasting is still right — the reasoning in
ADR-0007 about a peer holding operations the log never recorded stands
untouched. What does not survive measurement is the performance argument
attached to it.

### Snapshotting a large board is the most expensive thing the server does

**163 ms**, every 200 accepted operations, on a 10,000-element board. Roughly
ten frames.

Two details make it worse than the number looks:

- It happens **inside the same critical section** as the append, in
  `with_existing`.
- That lock is a **single server-wide `Mutex<ServerState>`**, not one per room.
  A snapshot on one large board therefore stalls *every room on the server*,
  including rooms whose users are doing nothing unusual.

At 1,000 elements it is 3.25 ms and unremarkable. Between 1,000 and 10,000 it
grows about 50× for a 10× increase, so it degrades sharply rather than
gradually.

### The render path got slower for a known reason

`board/scene` is now **12.7 ms** at 10,000 elements against a 16.6 ms frame
budget, and **25.7 ms** once serialised across the FFI boundary. The baseline
figures were 10.3 ms and 20.5 ms.

Four new fields cost 23%. That is a fair price for text, rotation and opacity,
and it is worth recording that the cost was proportional rather than surprising
— but it moves an already-over-budget path further over budget.

### Scaling, from 100 to 10,000 elements

| Path | Growth | Reading |
|---|---:|---|
| `frac/between` | ~8× | Sub-linear. Still not a concern |
| `document/json` decode | ~78× | Better than linear |
| `store/restore` | ~88× | Close to linear |
| `snapshot/absorb` | ~187× | ~2× worse than linear |
| `document/merge` | ~237× | ~2× worse than linear |
| `board/scene` | ~316× | ~3× worse than linear |
| `document/ordered` | **~541×** | The sort, still the worst engine curve |
| `store/snapshot` | **~819×** | The worst curve on the board, and new |

`document/ordered` degrading ~540× for a 100× increase was the headline last
time and is unchanged. It has been overtaken by `store/snapshot`, which is worse
and is paid under a server-wide lock.

### Join cost

A returning user opening a 10,000-element board that is not in memory waits
**92.4 ms** for the restore, then the document still has to be encoded (25.3 ms)
and crossed to the client. Persistence did not make joins cheap; it made them
possible.

## What this does not measure

- **Rendering.** Canvas 2D drawing cost is not measured here at all. Everything
  above is work done *before* a pixel is drawn.
- **Memory.** Only time.
- **Concurrency.** Single-threaded throughput. The server-wide lock contention
  described above is inferred from the code path, not measured under load —
  which is exactly the measurement this file is now most obviously missing.
- **Realistic documents.** Elements are uniform rectangles. Text carries
  strings and freehand carries point arrays; both serialise very differently,
  and neither is represented.
- **wasm.** These are native measurements. The browser is the target that
  matters and it will be slower.
- **fsync behaviour.** `synchronous = NORMAL` (ADR-0007) means the append
  figures do not include a full flush to disk. Under `FULL` they would be
  substantially worse.

## Consequences for the roadmap

The 2026-08-03 recommendations were to cache the paint order and make `scene`
incremental. Both are still worth doing, and neither is now the top item.

1. **Move the snapshot write out of the critical section.** 163 ms under a
   server-wide lock is the single worst thing in this file. The snapshot is
   already best-effort — a failed one costs replay time, not data — so it does
   not need to be synchronous with the accept path at all.
2. **Make the room lock per-room.** One board's storage cost should not be every
   board's latency. This is a precondition for the item above being fully
   effective, and is worth doing regardless.
3. **Reconsider the snapshot interval as a function of board size.** Every 200
   operations is cheap at 1,000 elements and ruinous at 10,000. A size- or
   time-based trigger would degrade more gracefully than a count.
4. **Cache the paint order.** `ordered` still has the second-worst curve and is
   called by `scene` on every render.
5. **Make `scene` incremental.** Returning only what changed is what actually
   removes the per-frame cost, and the four new fields have made it 23% more
   urgent. Still a wire-format change, so still needs a decision.

The engine handles 10,000 elements correctly. On these numbers it does not
handle them at interactive frame rates, and the server does not handle them
without periodically stalling every room. The first was already recorded; the
second is newly measured rather than newly true.

## Reproducing

```bash
cargo bench --workspace
```

Re-run after any change to merge, projection, serialisation or storage, and
update this file with the machine that produced the numbers. Comparing across
machines is meaningless; comparing across commits on one machine is the point.
