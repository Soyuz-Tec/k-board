//! What durability costs.
//!
//! ADR-0007 put a SQLite append inside the room's critical section — durable
//! before broadcast, because a peer holding an operation the log never recorded
//! has state the board cannot rebuild. That record accepted a blocking write on
//! the hot path on the argument that "a small append is not the bottleneck".
//!
//! These benchmarks are that argument's evidence, and the three numbers worth
//! having are:
//!
//! - **append**, paid on every accepted batch while the room lock is held;
//! - **snapshot**, paid every 200 operations, and scaling with board size;
//! - **restore**, paid once when a board is first opened after a restart, which
//!   is join latency for anyone returning to a persisted board.
//!
//! Written against an on-disk database, not `:memory:`. An in-memory store
//! measures serialisation and nothing else, which is the part that was never
//! in question.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

use kboard_core::clock::{ActorId, HlcGenerator};
use kboard_core::document::ScopeId;
use kboard_core::element::ElementId;
use kboard_core::frac;
use kboard_core::op::{upsert, StampedOp};
use kboard_core::ports::{OpLog, SnapshotStore};
use kboard_core::prop::{ElementKind, PropKey, PropValue};
use kboard_core::snapshot::Snapshot;
use kboard_store::SqliteStore;

const SIZES: [u128; 3] = [100, 1_000, 10_000];

/// Batch sizes a real client actually sends, named for what produces them.
const BATCHES: [(&str, u128); 3] = [("one_edit", 1), ("drag_commit", 10), ("freehand", 100)];

fn scope() -> ScopeId {
    ScopeId::new("bench-tenant:bench-board")
}

/// The same element shape the engine benchmarks use, so the two files are
/// measuring the same document rather than two different ideas of one.
fn shape_ops(actor: u64, count: u128) -> Vec<StampedOp> {
    let mut clock = HlcGenerator::new(ActorId(actor));
    let mut ops = Vec::new();
    let mut z = frac::first();

    for index in 0..count {
        z = frac::between(Some(&z), None);
        ops.extend(upsert(
            ElementId((u128::from(actor) << 64) | index),
            [
                (PropKey::Kind, PropValue::Kind(ElementKind::Rectangle)),
                (PropKey::X, PropValue::Num(index as f64)),
                (PropKey::Y, PropValue::Num((index % 97) as f64)),
                (PropKey::Width, PropValue::Num(120.0)),
                (PropKey::Height, PropValue::Num(80.0)),
                (PropKey::Stroke, PropValue::Color(0x1e1e1eff)),
                (PropKey::Fill, PropValue::Color(0)),
                (PropKey::StrokeWidth, PropValue::Num(2.0)),
                (PropKey::ZIndex, PropValue::Text(z.clone())),
            ],
            &mut clock,
            1_000 + index as u64,
        ));
    }
    ops
}

fn snapshot_of(count: u128) -> Snapshot {
    Snapshot::materialize(scope(), &shape_ops(1, count))
}

/// A store on real disk. `tempfile` cleans up when the guard drops, so each
/// benchmark starts from an empty database rather than one the previous
/// benchmark filled.
fn on_disk() -> (tempfile::TempDir, SqliteStore) {
    let directory = tempfile::tempdir().expect("temp dir");
    let store = SqliteStore::open(directory.path().join("bench.db")).expect("open");
    (directory, store)
}

/// The write on the hot path.
///
/// Measured per batch rather than per operation, because that is the unit the
/// room lock is held for — a client sending ten operations blocks the room once,
/// not ten times.
fn bench_append(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("store/append");

    for (name, size) in BATCHES {
        let ops = shape_ops(1, size);
        group.throughput(Throughput::Elements(ops.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &size, |bencher, _| {
            let (_guard, mut store) = on_disk();
            bencher.iter(|| {
                store.append(&scope(), black_box(&ops)).expect("append");
            });
        });
    }
    group.finish();
}

/// Written back every 200 accepted operations, and the whole document each
/// time. This is the cost that grows with the board rather than with the edit.
fn bench_snapshot(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("store/snapshot");

    for size in SIZES {
        let snapshot = snapshot_of(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |bencher, _| {
            let (_guard, mut store) = on_disk();
            bencher.iter(|| {
                store.store(&scope(), black_box(&snapshot)).expect("store");
            });
        });
    }
    group.finish();
}

/// Join latency for a board that was not already in memory.
///
/// Snapshot plus the operations after it, which is why the tail is populated:
/// restoring from a snapshot alone would measure the best case and call it the
/// cost.
fn bench_restore(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("store/restore");

    for size in SIZES {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |bencher, _| {
            let (_guard, mut store) = on_disk();
            store.store(&scope(), &snapshot_of(size)).expect("store");
            // A hundred operations since the snapshot: the snapshot interval is
            // 200, so this is a board caught mid-interval, which is the usual
            // case rather than the lucky one.
            store.append(&scope(), &shape_ops(2, 100)).expect("append");

            bencher.iter(|| {
                black_box(store.restore(black_box(&scope())).expect("restore"));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_append, bench_snapshot, bench_restore);
criterion_main!(benches);
