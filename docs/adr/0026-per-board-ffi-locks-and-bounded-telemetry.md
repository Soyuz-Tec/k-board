# ADR-0026: Isolate FFI handles and expose bounded operational telemetry

- **Status:** Accepted
- **Date:** 2026-08-05
- **Owners:** Engine, Server, Operations
- **Related:** [ADR-0004](0004-global-registry-and-side-buffer.md), [ADR-0015](0015-per-scope-room-cells.md)

## Context

ADR-0004 correctly made handles process-global so native schedulers may move a
call between threads. Its registry stored `Board` values directly, however, so
every mutation, projection and JSON serialization held one process-wide mutex.
Unrelated native handles therefore had the cross-scope head-of-line blocking
that room cells had already removed from the server.

The server also lacked measurements that could distinguish directory, mailbox,
validation, storage-queue, SQLite, snapshot and client-delivery costs. Raw
scope labels would make those measurements both sensitive and unbounded.

## Decision

The process-global FFI registry maps integer handles to `Arc<Mutex<Board>>`.
The registry lock is held only to allocate, look up or remove a handle. Every
operation clones the `Arc`, releases the registry, and executes under that
board's mutex. Result bytes remain thread-local and panic recovery continues to
recover poisoned locks.

Close removes the registry entry and is the linearization point for future
lookups. A call that cloned the `Arc` before close may finish; close is not
unsafe cancellation. A later or stale lookup receives `STATUS_NO_BOARD`.

Native FFI lock telemetry records count, total and maximum wait/hold
nanoseconds for seven fixed operation classes. `kb_metrics` emits no handles,
scopes or user values. Server telemetry records exact count/total/max latency
for directory lookup, cold restore, mailbox wait, validation, storage access,
apply, acknowledgement, broadcast and each snapshot phase. Storage queue wait
and SQLite time are distinct. Exporters use the twelve fixed microsecond
buckets published by diagnostics; only component/phase/outcome may be metric
labels. Logs use a fixed scope hash plus validated replica/batch and sequence
correlation fields.

The initial command objectives are cross-scope cold p99 below 250 ms and no
admitted command above 1 s on the CI workload. These are broad regression
thresholds suitable for shared runners, not production promises. Production
SLOs are recalibrated from deployment histograms.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| Keep one global mutex | Small | Serialization/scene on one board stalls all native handles | Violates the isolation quality goal |
| Concurrent map plus lock-free board | Potential throughput | Much larger correctness and unsafe surface | CRDT mutation still requires ordered ownership |
| Per-board synchronized handles | Small registry critical section; explicit close semantics | One mutex/Arc allocation per open handle | **Chosen** |
| Label metrics by raw scope | Easy diagnosis | Tenant disclosure and unbounded cardinality | Unsafe operational contract |

## Consequences

- Operations on one handle remain ordered; independent handles can run on
  independent native threads.
- Hosts must not assume close cancels a call already in progress.
- Wasm remains single-threaded but uses the same handle structure and ABI.
- Fixed summary metrics preserve bounded memory; full distributions belong in
  an external exporter using the declared buckets.

## Validation

- Native tests hold one board lock and prove another handle serializes within
  500 ms, prove close/stale behavior, panic containment and fixed metric shape.
- Identical two-scene native benchmarks measured 1.459 ms serialized versus
  1.168 ms parallel, including thread creation, a 20% wall-time reduction.
- Wasm `scene` for 1,000 elements measured 0.944 ms median and 1.714 ms p99.
- Eight-scope live load measured 9.496 ms cross-scope p99 against the 250 ms
  CI objective. Raw evidence and commands are in `docs/benchmarks.md`.

## Revisit triggers

- One handle needs concurrent read projections with ordered mutation.
- Host profiling shows board mutex contention rather than projection cost.
- An exporter needs histograms or traces beyond the bounded in-process summary.
