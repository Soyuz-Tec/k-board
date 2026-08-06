//! Baseline for the projection the client renders from.
//!
//! `Board::scene` runs on every change a client draws, so its cost is paid at
//! interaction rate rather than at join. It flattens the document, sorts by
//! fractional index, and allocates a fresh `SceneItem` per element — all of
//! which scale with board size rather than with the size of the change.
//!
//! Measured before persistence lands, alongside the engine baseline.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

use kboard::board::{Board, Command};
use kboard::{kb_close, kb_exec, kb_open, kb_scene, STATUS_OK};

const SIZES: [u128; 3] = [100, 1_000, 10_000];

fn board_of(count: u128) -> Board {
    let mut board = Board::open("bench-tenant/bench-board", 1);
    for index in 0..count {
        board
            .exec(
                &Command::Add {
                    kind: "rectangle".to_owned(),
                    x: index as f64,
                    y: (index % 97) as f64,
                    w: 120.0,
                    h: 80.0,
                    stroke: 0x1e1e_1eff,
                    fill: 0,
                    stroke_width: 2.0,
                },
                1_000 + index as u64,
            )
            .expect("bench fixture must build");
    }
    board
}

fn bench_scene(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("board/scene");
    for size in SIZES {
        let board = board_of(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |bencher, _| {
            bencher.iter(|| black_box(board.scene().len()));
        });
    }
    group.finish();
}

fn bench_scene_json(criterion: &mut Criterion) {
    // What actually crosses the FFI boundary is the serialised form, so the
    // projection alone understates what a render costs a host.
    let mut group = criterion.benchmark_group("board/scene_json");
    for size in SIZES {
        let board = board_of(size);
        let encoded = serde_json::to_vec(&board.scene()).unwrap();
        group.throughput(Throughput::Bytes(encoded.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |bencher, _| {
            bencher.iter(|| serde_json::to_vec(black_box(&board.scene())).unwrap());
        });
    }
    group.finish();
}

fn ffi_board(scope: &str, actor: u64, count: u128) -> u32 {
    let handle = unsafe { kb_open(scope.as_ptr(), scope.len(), actor) };
    assert_ne!(handle, 0);
    for index in 0..count {
        let command =
            format!(r#"{{"cmd":"add","kind":"rectangle","x":{index},"y":0,"w":120,"h":80}}"#);
        assert_eq!(
            unsafe {
                kb_exec(
                    handle,
                    command.as_ptr(),
                    command.len(),
                    1_000.0 + index as f64,
                )
            },
            STATUS_OK
        );
    }
    handle
}

fn bench_native_ffi(criterion: &mut Criterion) {
    const ELEMENTS: u128 = 1_000;
    let single = ffi_board("bench-tenant/ffi-single", 101, ELEMENTS);
    criterion.bench_function("ffi/scene/1000", |bencher| {
        bencher.iter(|| assert_eq!(kb_scene(black_box(single)), STATUS_OK));
    });

    let left = ffi_board("bench-tenant/ffi-left", 102, ELEMENTS);
    let right = ffi_board("bench-tenant/ffi-right", 103, ELEMENTS);
    criterion.bench_function("ffi/serialized_two_scene/1000", |bencher| {
        bencher.iter(|| {
            assert_eq!(kb_scene(black_box(left)), STATUS_OK);
            assert_eq!(kb_scene(black_box(right)), STATUS_OK);
        });
    });
    criterion.bench_function("ffi/parallel_independent_scene/1000", |bencher| {
        bencher.iter(|| {
            std::thread::scope(|scope| {
                scope.spawn(|| assert_eq!(kb_scene(black_box(left)), STATUS_OK));
                scope.spawn(|| assert_eq!(kb_scene(black_box(right)), STATUS_OK));
            });
        });
    });
    assert_eq!(kb_close(single), STATUS_OK);
    assert_eq!(kb_close(left), STATUS_OK);
    assert_eq!(kb_close(right), STATUS_OK);
}

criterion_group!(benches, bench_scene, bench_scene_json, bench_native_ffi);
criterion_main!(benches);
