//! Performance baseline for the document engine.
//!
//! Captured before durable persistence lands, because persistence changes the
//! shape of every measurement after it and the pre-change numbers cannot be
//! recovered once it does.
//!
//! These measure the operations that sit on a user-visible path:
//!
//! - `merge` runs on every incoming batch from a peer.
//! - `ordered` runs on every render, and currently sorts the whole document.
//! - `absorb` runs on every accepted batch on the server.
//! - the JSON round trip runs on every join, in both directions.
//! - `frac::between` runs on every element creation.
//! - `Board::scene` runs on every change the client renders.
//!
//! Element counts of 100 / 1k / 10k bracket the range that matters: a working
//! sketch, a busy board, and the point where Excalidraw itself starts to
//! struggle.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

use kboard_core::clock::{ActorId, HlcGenerator};
use kboard_core::document::{Document, ScopeId};
use kboard_core::element::ElementId;
use kboard_core::frac;
use kboard_core::op::{apply_all, upsert, StampedOp};
use kboard_core::prop::{ElementKind, PropKey, PropValue};
use kboard_core::snapshot::Snapshot;

const SIZES: [u128; 3] = [100, 1_000, 10_000];

fn scope() -> ScopeId {
    ScopeId::new("bench-tenant/bench-board")
}

/// A realistic element: the property set the renderer actually reads, not a
/// single field, so the measurement reflects real merge and serialisation cost.
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

fn document_of(count: u128) -> Document {
    let mut document = Document::new(scope());
    apply_all(&mut document, &shape_ops(1, count));
    document
}

fn bench_merge(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("document/merge");
    for size in SIZES {
        // Two replicas that each hold the same elements but from different
        // actors: the worst realistic case, where every property is compared.
        let mine = document_of(size);
        let mut theirs = Document::new(scope());
        apply_all(&mut theirs, &shape_ops(2, size));

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |bencher, _| {
            bencher.iter_batched(
                || mine.clone(),
                |mut document| {
                    document.merge(black_box(&theirs)).unwrap();
                    document
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_ordered(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("document/ordered");
    for size in SIZES {
        let document = document_of(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |bencher, _| {
            bencher.iter(|| black_box(document.ordered().len()));
        });
    }
    group.finish();
}

fn bench_absorb(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("snapshot/absorb");
    for size in SIZES {
        let ops = shape_ops(1, size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |bencher, _| {
            bencher.iter_batched(
                || Snapshot::empty(scope()),
                |mut snapshot| {
                    snapshot.absorb(black_box(&ops));
                    snapshot
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_json_round_trip(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("document/json");
    for size in SIZES {
        let document = document_of(size);
        let encoded = serde_json::to_string(&document).unwrap();
        group.throughput(Throughput::Bytes(encoded.len() as u64));

        group.bench_with_input(BenchmarkId::new("encode", size), &size, |bencher, _| {
            bencher.iter(|| serde_json::to_string(black_box(&document)).unwrap())
        });
        group.bench_with_input(BenchmarkId::new("decode", size), &size, |bencher, _| {
            bencher.iter(|| {
                let decoded: Document = serde_json::from_str(black_box(&encoded)).unwrap();
                decoded
            });
        });
    }
    group.finish();
}

fn bench_fractional_index(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("frac/between");

    group.bench_function("append", |bencher| {
        bencher.iter_batched(
            frac::first,
            |key| frac::between(Some(black_box(&key)), None),
            criterion::BatchSize::SmallInput,
        );
    });

    // The pathological case: always insert at the same position, so keys grow
    // longer with every insertion. Integer ordering would need a renumbering
    // pass here; this must not degrade.
    for depth in [10usize, 100, 500] {
        let mut low = frac::first();
        let high = frac::between(Some(&low), None);
        for _ in 0..depth {
            low = frac::between(Some(&low), Some(&high));
        }
        group.bench_with_input(
            BenchmarkId::new("subdivided", depth),
            &depth,
            |bencher, _| bencher.iter(|| frac::between(black_box(Some(&low)), Some(&high))),
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_merge,
    bench_ordered,
    bench_absorb,
    bench_json_round_trip,
    bench_fractional_index
);
criterion_main!(benches);
