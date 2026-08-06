//! # kboard-ffi
//!
//! The C ABI surface for [`kboard_core`]. One export set serves every host:
//! a browser loading the `wasm32` build, and a native runtime — BEAM via
//! Rustler, CPython via PyO3, the JVM via Panama, .NET via P/Invoke — loading
//! the `cdylib`.
//!
//! `kboard-core` is `#![forbid(unsafe_code)]`. This crate is the only place
//! unsafe exists, it is deliberately thin, and all real logic lives in
//! [`board`], which is safe and unit-tested on its own.
//!
//! ## The rule that matters
//!
//! **No panic ever crosses this boundary.** A panic unwinding into a foreign
//! runtime does not fail one call — it can abort the entire host process. Every
//! export is wrapped in [`catch_unwind`] and degrades to [`STATUS_PANIC`].
//! This is why the release profile does not set `panic = "abort"`.
//!
//! ## Calling convention
//!
//! Functions return a status code. Any produced bytes are left in a per-thread
//! buffer that the caller reads with [`kb_last_ptr`] and [`kb_last_len`] before
//! its next call. Keeping results out of the return value is what lets the same
//! signatures work on wasm32 and 64-bit native without pointer packing.
//!
//! ```text
//! const status = exports.kb_exec(handle, ptr, len, Date.now());
//! if (status === 0) readUtf8(exports.kb_last_ptr(), exports.kb_last_len());
//! ```

pub mod board;

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use kboard_core::op::StampedOp;

use crate::board::{Board, Command};

pub const STATUS_OK: u32 = 0;
/// A panic was trapped. The call had no effect the caller can rely on.
pub const STATUS_PANIC: u32 = 1;
/// Input was not valid UTF-8, not valid JSON, or a null pointer.
pub const STATUS_BAD_INPUT: u32 = 2;
/// No board is open under that handle.
pub const STATUS_NO_BOARD: u32 = 3;
/// The command was well-formed but could not be applied.
pub const STATUS_REFUSED: u32 = 4;

/// ABI version. Hosts should check this on load — the wasm and native artifacts
/// must always come from the same build.
pub const ABI_VERSION: u32 = 3;

#[derive(Default)]
struct Registry {
    boards: HashMap<u32, Arc<Mutex<Board>>>,
    next_handle: u32,
}

#[derive(Clone, Copy)]
#[repr(usize)]
enum OperationClass {
    Lifecycle,
    Exec,
    Merge,
    Pending,
    Load,
    History,
    Scene,
}

const OPERATION_CLASSES: [(&str, OperationClass); 7] = [
    ("lifecycle", OperationClass::Lifecycle),
    ("exec", OperationClass::Exec),
    ("merge", OperationClass::Merge),
    ("pending", OperationClass::Pending),
    ("load", OperationClass::Load),
    ("history", OperationClass::History),
    ("scene", OperationClass::Scene),
];

#[derive(Default)]
struct LockTiming {
    calls: AtomicU64,
    wait_ns: AtomicU64,
    hold_ns: AtomicU64,
    max_wait_ns: AtomicU64,
    max_hold_ns: AtomicU64,
}

impl LockTiming {
    #[cfg(not(target_arch = "wasm32"))]
    fn record(&self, wait_ns: u64, hold_ns: u64) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.wait_ns.fetch_add(wait_ns, Ordering::Relaxed);
        self.hold_ns.fetch_add(hold_ns, Ordering::Relaxed);
        self.max_wait_ns.fetch_max(wait_ns, Ordering::Relaxed);
        self.max_hold_ns.fetch_max(hold_ns, Ordering::Relaxed);
    }

    fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "calls": self.calls.load(Ordering::Relaxed),
            "wait_ns": self.wait_ns.load(Ordering::Relaxed),
            "hold_ns": self.hold_ns.load(Ordering::Relaxed),
            "max_wait_ns": self.max_wait_ns.load(Ordering::Relaxed),
            "max_hold_ns": self.max_hold_ns.load(Ordering::Relaxed),
        })
    }
}

struct FfiMetrics {
    registry: [LockTiming; OPERATION_CLASSES.len()],
    board: [LockTiming; OPERATION_CLASSES.len()],
}

impl Default for FfiMetrics {
    fn default() -> Self {
        Self {
            registry: std::array::from_fn(|_| LockTiming::default()),
            board: std::array::from_fn(|_| LockTiming::default()),
        }
    }
}

fn ffi_metrics() -> &'static FfiMetrics {
    static METRICS: OnceLock<FfiMetrics> = OnceLock::new();
    METRICS.get_or_init(FfiMetrics::default)
}

/// Boards are process-global, not thread-local: a native host may open a board
/// on one scheduler thread and call it from another. A BEAM NIF does exactly
/// that.
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// Recovers from poisoning rather than propagating it. A panic in one call must
/// not permanently disable the library for a long-lived host process.
fn with_registry<T>(class: OperationClass, action: impl FnOnce(&mut Registry) -> T) -> T {
    #[cfg(target_arch = "wasm32")]
    let _ = class;
    #[cfg(not(target_arch = "wasm32"))]
    let waiting = Instant::now();
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    #[cfg(not(target_arch = "wasm32"))]
    let acquired = Instant::now();
    let result = action(&mut guard);
    #[cfg(not(target_arch = "wasm32"))]
    ffi_metrics().registry[class as usize].record(
        acquired
            .duration_since(waiting)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64,
        acquired.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
    );
    result
}

fn board_handle(handle: u32, class: OperationClass) -> Option<Arc<Mutex<Board>>> {
    with_registry(class, |registry| registry.boards.get(&handle).cloned())
}

fn with_board<T>(
    handle: u32,
    class: OperationClass,
    action: impl FnOnce(&mut Board) -> T,
) -> Option<T> {
    let board = board_handle(handle, class)?;
    #[cfg(not(target_arch = "wasm32"))]
    let waiting = Instant::now();
    let mut guard = board
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    #[cfg(not(target_arch = "wasm32"))]
    let acquired = Instant::now();
    let result = action(&mut guard);
    #[cfg(not(target_arch = "wasm32"))]
    ffi_metrics().board[class as usize].record(
        acquired
            .duration_since(waiting)
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64,
        acquired.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
    );
    Some(result)
}

thread_local! {
    /// Result bytes for the most recent call *on this thread*. Per-thread so
    /// two concurrent callers cannot overwrite each other's output.
    static LAST: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn publish(bytes: Vec<u8>) {
    LAST.with(|slot| *slot.borrow_mut() = bytes);
}

fn guarded(action: impl FnOnce() -> u32) -> u32 {
    catch_unwind(AssertUnwindSafe(action)).unwrap_or(STATUS_PANIC)
}

/// # Safety
/// `ptr` must be null, or point to `len` initialised bytes that stay valid for
/// the duration of the call.
unsafe fn borrow_str<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).ok()
}

// -- memory ----------------------------------------------------------------

/// Allocate `len` bytes for the caller to write into.
///
/// Pair every call with [`kb_free`] using the *same* length. The allocation is
/// exact — a boxed slice, not a `Vec` with spare capacity — so the length the
/// caller passes back is the length that was allocated.
#[no_mangle]
pub extern "C" fn kb_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    let buffer = vec![0u8; len].into_boxed_slice();
    Box::into_raw(buffer).cast::<u8>()
}

/// # Safety
/// `ptr` must have come from [`kb_alloc`] with the same `len`, and must not be
/// used afterwards.
#[no_mangle]
pub unsafe extern "C" fn kb_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    // `slice_from_raw_parts_mut` builds the fat pointer directly. Going via
    // `slice::from_raw_parts_mut` would materialise a `&mut [u8]` first, which
    // asserts validity we are about to invalidate by freeing.
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
}

#[no_mangle]
pub extern "C" fn kb_abi_version() -> u32 {
    ABI_VERSION
}

#[no_mangle]
pub extern "C" fn kb_last_ptr() -> *const u8 {
    LAST.with(|slot| slot.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn kb_last_len() -> usize {
    LAST.with(|slot| slot.borrow().len())
}

/// Publish bounded lock telemetry as JSON.
///
/// Operation class is the only label. Handles, scopes and host-supplied values
/// are deliberately absent, so an embedded process cannot create unbounded
/// metric cardinality through this ABI.
#[no_mangle]
pub extern "C" fn kb_metrics() -> u32 {
    guarded(|| {
        let metrics = ffi_metrics();
        let mut registry = serde_json::Map::new();
        let mut board = serde_json::Map::new();
        for (name, class) in OPERATION_CLASSES {
            registry.insert(name.to_owned(), metrics.registry[class as usize].snapshot());
            board.insert(name.to_owned(), metrics.board[class as usize].snapshot());
        }
        match serde_json::to_vec(&serde_json::json!({
            "registry_lock": registry,
            "board_lock": board,
        })) {
            Ok(bytes) => {
                publish(bytes);
                STATUS_OK
            }
            Err(_) => STATUS_REFUSED,
        }
    })
}

// -- lifecycle -------------------------------------------------------------

/// Open a board in `scope` for `actor`. Returns a handle, or `0` on failure.
///
/// The engine does not decide whether this actor may open this scope — the host
/// does, before calling.
///
/// # Safety
/// `scope_ptr`/`scope_len` must describe valid UTF-8 bytes.
#[no_mangle]
pub unsafe extern "C" fn kb_open(scope_ptr: *const u8, scope_len: usize, actor: u64) -> u32 {
    let scope = match borrow_str(scope_ptr, scope_len) {
        Some(text) => text.to_owned(),
        None => return 0,
    };
    catch_unwind(AssertUnwindSafe(|| {
        with_registry(OperationClass::Lifecycle, |registry| {
            registry.next_handle += 1;
            let handle = registry.next_handle;
            registry
                .boards
                .insert(handle, Arc::new(Mutex::new(Board::open(&scope, actor))));
            handle
        })
    }))
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn kb_close(handle: u32) -> u32 {
    guarded(|| {
        with_registry(OperationClass::Lifecycle, |registry| {
            if registry.boards.remove(&handle).is_some() {
                STATUS_OK
            } else {
                STATUS_NO_BOARD
            }
        })
    })
}

// -- operations ------------------------------------------------------------

/// Execute a JSON command. On success, publishes the affected element id.
///
/// # Safety
/// `ptr`/`len` must describe valid UTF-8 bytes.
#[no_mangle]
pub unsafe extern "C" fn kb_exec(handle: u32, ptr: *const u8, len: usize, now_ms: f64) -> u32 {
    let Some(json) = borrow_str(ptr, len) else {
        return STATUS_BAD_INPUT;
    };
    let Ok(command) = serde_json::from_str::<Command>(json) else {
        return STATUS_BAD_INPUT;
    };
    // Time crosses as f64 because every host has it and wasm32 avoids i64/BigInt.
    let now = if now_ms.is_finite() && now_ms > 0.0 {
        now_ms as u64
    } else {
        0
    };

    guarded(|| {
        let Some(outcome) = with_board(handle, OperationClass::Exec, |board| {
            match board.exec(&command, now) {
                Ok(id) => {
                    publish(id.unwrap_or_default().into_bytes());
                    STATUS_OK
                }
                Err(_) => STATUS_REFUSED,
            }
        }) else {
            return STATUS_NO_BOARD;
        };
        outcome
    })
}

/// Merge a JSON array of operations from a peer.
///
/// # Safety
/// `ptr`/`len` must describe valid UTF-8 bytes.
#[no_mangle]
pub unsafe extern "C" fn kb_merge(handle: u32, ptr: *const u8, len: usize) -> u32 {
    let Some(json) = borrow_str(ptr, len) else {
        return STATUS_BAD_INPUT;
    };
    let Ok(ops) = serde_json::from_str::<Vec<StampedOp>>(json) else {
        return STATUS_BAD_INPUT;
    };

    guarded(|| {
        let Some(outcome) = with_board(handle, OperationClass::Merge, |board| {
            let changed = board.merge_ops(&ops);
            publish(changed.to_string().into_bytes());
            STATUS_OK
        }) else {
            return STATUS_NO_BOARD;
        };
        outcome
    })
}

/// Drain operations this replica produced but has not broadcast, as JSON.
#[no_mangle]
pub extern "C" fn kb_pending(handle: u32) -> u32 {
    guarded(|| {
        let Some(outcome) = with_board(handle, OperationClass::Pending, |board| {
            let pending = board.take_pending();
            match serde_json::to_vec(&pending) {
                Ok(bytes) => {
                    publish(bytes);
                    STATUS_OK
                }
                Err(_) => STATUS_REFUSED,
            }
        }) else {
            return STATUS_NO_BOARD;
        };
        outcome
    })
}

/// Merge a whole document as JSON — the join handshake.
///
/// Returns [`STATUS_REFUSED`] if the document belongs to another tenant, which
/// is a routing bug in the host rather than a recoverable condition.
///
/// # Safety
/// `ptr`/`len` must describe valid UTF-8 bytes.
#[no_mangle]
pub unsafe extern "C" fn kb_load(handle: u32, ptr: *const u8, len: usize) -> u32 {
    let Some(json) = borrow_str(ptr, len) else {
        return STATUS_BAD_INPUT;
    };
    let Ok(incoming) = serde_json::from_str::<kboard_core::document::Document>(json) else {
        return STATUS_BAD_INPUT;
    };

    guarded(|| {
        let Some(outcome) = with_board(handle, OperationClass::Load, |board| {
            match board.merge_document(&incoming) {
                Ok(changed) => {
                    publish(changed.to_string().into_bytes());
                    STATUS_OK
                }
                Err(_) => STATUS_REFUSED,
            }
        }) else {
            return STATUS_NO_BOARD;
        };
        outcome
    })
}

/// Reverse this actor's most recent change.
///
/// Publishes `"true"` when something was undone, `"false"` when the history is
/// empty. The reversal is an ordinary edit and appears in `kb_pending` like any
/// other, so a host broadcasts it without special handling.
#[no_mangle]
pub extern "C" fn kb_undo(handle: u32, now_ms: f64) -> u32 {
    step(handle, now_ms, true)
}

/// Reapply the most recently undone change.
#[no_mangle]
pub extern "C" fn kb_redo(handle: u32, now_ms: f64) -> u32 {
    step(handle, now_ms, false)
}

/// Publish whether undo and redo are currently available, as `"<undo>,<redo>"`.
/// One call rather than two so a host cannot render a half-updated toolbar.
#[no_mangle]
pub extern "C" fn kb_history(handle: u32) -> u32 {
    guarded(|| {
        let Some(outcome) = with_board(handle, OperationClass::History, |board| {
            publish(format!("{},{}", board.can_undo(), board.can_redo()).into_bytes());
            STATUS_OK
        }) else {
            return STATUS_NO_BOARD;
        };
        outcome
    })
}

fn step(handle: u32, now_ms: f64, backward: bool) -> u32 {
    let now = if now_ms.is_finite() && now_ms > 0.0 {
        now_ms as u64
    } else {
        0
    };

    guarded(|| {
        let Some(outcome) = with_board(handle, OperationClass::History, |board| {
            let moved = if backward {
                board.undo(now)
            } else {
                board.redo(now)
            };
            publish(moved.to_string().into_bytes());
            STATUS_OK
        }) else {
            return STATUS_NO_BOARD;
        };
        outcome
    })
}

/// Publish the render-ready scene as JSON, in paint order.
#[no_mangle]
pub extern "C" fn kb_scene(handle: u32) -> u32 {
    guarded(|| {
        let Some(outcome) =
            with_board(
                handle,
                OperationClass::Scene,
                |board| match serde_json::to_vec(&board.scene()) {
                    Ok(bytes) => {
                        publish(bytes);
                        STATUS_OK
                    }
                    Err(_) => STATUS_REFUSED,
                },
            )
        else {
            return STATUS_NO_BOARD;
        };
        outcome
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_core::clock::ActorId;
    use kboard_core::element::ElementId;

    fn call_exec(handle: u32, json: &str) -> u32 {
        unsafe { kb_exec(handle, json.as_ptr(), json.len(), 1_000.0) }
    }

    fn last_string() -> String {
        LAST.with(|slot| String::from_utf8(slot.borrow().clone()).unwrap())
    }

    fn open(scope: &str, actor: u64) -> u32 {
        unsafe { kb_open(scope.as_ptr(), scope.len(), actor) }
    }

    #[test]
    fn a_board_round_trips_through_the_abi() {
        let handle = open("tenant/board", 1);
        assert_ne!(handle, 0);

        let status = call_exec(
            handle,
            r#"{"cmd":"add","kind":"rectangle","x":1,"y":2,"w":30,"h":40,"stroke":255}"#,
        );
        assert_eq!(status, STATUS_OK);
        assert_eq!(last_string().len(), 32, "element id is returned as hex");

        assert_eq!(kb_scene(handle), STATUS_OK);
        let scene = last_string();
        assert!(scene.contains("\"kind\":\"rectangle\""));
        assert!(scene.contains("\"w\":30.0"));

        assert_eq!(kb_close(handle), STATUS_OK);
        assert_eq!(kb_close(handle), STATUS_NO_BOARD, "double close is refused");
    }

    #[test]
    fn two_handles_sync_through_pending_and_merge() {
        let alice = open("tenant/shared", 1);
        let bob = open("tenant/shared", 2);

        call_exec(
            alice,
            r#"{"cmd":"add","kind":"ellipse","x":0,"y":0,"w":5,"h":5}"#,
        );
        assert_eq!(kb_pending(alice), STATUS_OK);
        let ops = last_string();

        assert_eq!(unsafe { kb_merge(bob, ops.as_ptr(), ops.len()) }, STATUS_OK);
        assert_eq!(kb_scene(bob), STATUS_OK);
        assert!(
            last_string().contains("ellipse"),
            "the peer received the shape"
        );

        kb_close(alice);
        kb_close(bob);
    }

    #[test]
    fn a_busy_board_does_not_block_an_independent_handle() {
        use std::sync::mpsc;
        use std::time::Duration;

        let busy = open("tenant/busy", 11);
        let independent = open("tenant/independent", 12);
        let busy_board = board_handle(busy, OperationClass::Scene).unwrap();
        let busy_guard = busy_board.lock().unwrap();

        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            sent.send(kb_scene(independent)).unwrap();
        });
        assert_eq!(
            received.recv_timeout(Duration::from_millis(500)).unwrap(),
            STATUS_OK,
            "serialising another handle must not wait for the busy board"
        );

        drop(busy_guard);
        worker.join().unwrap();
        kb_close(busy);
        kb_close(independent);
    }

    #[test]
    fn close_detaches_new_calls_while_a_cloned_in_flight_handle_can_finish() {
        let handle = open("tenant/close-race", 21);
        let in_flight = board_handle(handle, OperationClass::Exec).unwrap();

        assert_eq!(kb_close(handle), STATUS_OK);
        assert_eq!(kb_scene(handle), STATUS_NO_BOARD);

        // A call that completed lookup before close owns this Arc. Close is a
        // linearization point for future lookup, not cancellation of code that
        // may already be inside host or allocator work.
        let mut board = in_flight.lock().unwrap();
        assert!(board
            .exec(
                &Command::Add {
                    kind: "rectangle".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                    stroke: 0,
                    fill: 0,
                    stroke_width: 1.0,
                },
                1_000,
            )
            .is_ok());
    }

    #[test]
    fn lock_metrics_are_bounded_by_operation_class() {
        let handle = open("tenant/metrics", 31);
        assert_eq!(kb_scene(handle), STATUS_OK);
        assert_eq!(kb_metrics(), STATUS_OK);
        let value: serde_json::Value = serde_json::from_str(&last_string()).unwrap();
        assert_eq!(value["registry_lock"].as_object().unwrap().len(), 7);
        assert_eq!(value["board_lock"].as_object().unwrap().len(), 7);
        assert!(value["board_lock"]["scene"]["calls"].as_u64().unwrap() >= 1);
        assert!(value["registry_lock"]["scene"]["calls"].as_u64().unwrap() >= 1);
        kb_close(handle);
    }

    #[test]
    fn malformed_input_is_refused_not_fatal() {
        let handle = open("t/b", 1);
        assert_eq!(call_exec(handle, "not json"), STATUS_BAD_INPUT);
        assert_eq!(call_exec(handle, r#"{"cmd":"nope"}"#), STATUS_BAD_INPUT);
        assert_eq!(
            call_exec(
                handle,
                r#"{"cmd":"add","kind":"unicorn","x":0,"y":0,"w":1,"h":1}"#
            ),
            STATUS_REFUSED
        );
        // Still usable afterwards — the point of trapping rather than aborting.
        assert_eq!(
            call_exec(
                handle,
                r#"{"cmd":"add","kind":"diamond","x":0,"y":0,"w":1,"h":1}"#
            ),
            STATUS_OK
        );
        kb_close(handle);
    }

    #[test]
    fn null_and_unknown_handles_are_refused() {
        assert_eq!(
            unsafe { kb_exec(999_999, std::ptr::null(), 0, 0.0) },
            STATUS_BAD_INPUT
        );
        assert_eq!(kb_scene(999_999), STATUS_NO_BOARD);
        assert_eq!(unsafe { kb_open(std::ptr::null(), 0, 1) }, 0);
    }

    #[test]
    fn allocation_round_trips() {
        let ptr = kb_alloc(64);
        assert!(!ptr.is_null());
        unsafe { kb_free(ptr, 64) };
        // Zero-length allocation is a no-op, and freeing null is safe.
        assert!(kb_alloc(0).is_null());
        unsafe { kb_free(std::ptr::null_mut(), 0) };
    }

    #[test]
    fn abi_version_is_exposed() {
        assert_eq!(kb_abi_version(), ABI_VERSION);
    }

    #[test]
    fn actor_above_u32_survives_the_abi() {
        let actor = u64::from(u32::MAX) + 42;
        let handle = open("tenant/large-actor", actor);
        assert_eq!(
            call_exec(
                handle,
                r#"{"cmd":"add","kind":"rectangle","x":1,"y":2,"w":3,"h":4,"stroke":255}"#,
            ),
            STATUS_OK
        );
        let id = ElementId::from_hex(&last_string()).unwrap();
        assert_eq!(id.actor(), ActorId(actor));
    }

    #[test]
    fn loading_a_document_from_another_tenant_is_refused() {
        let handle = open("tenant-a/board", 1);
        call_exec(
            handle,
            r#"{"cmd":"add","kind":"rectangle","x":0,"y":0,"w":1,"h":1}"#,
        );

        // A document that belongs to a different scope must never land here.
        let foreign = serde_json::to_string(&kboard_core::document::Document::new(
            kboard_core::document::ScopeId::new("tenant-b/board"),
        ))
        .unwrap();
        assert_eq!(
            unsafe { kb_load(handle, foreign.as_ptr(), foreign.len()) },
            STATUS_REFUSED
        );
        kb_close(handle);
    }

    #[test]
    fn loading_a_document_from_the_same_scope_succeeds() {
        let alice = open("tenant/shared", 1);
        let bob = open("tenant/shared", 2);

        call_exec(
            alice,
            r#"{"cmd":"add","kind":"diamond","x":3,"y":4,"w":5,"h":6}"#,
        );
        assert_eq!(kb_scene(alice), STATUS_OK);

        let document = with_board(alice, OperationClass::Load, |board| {
            serde_json::to_string(board.document()).unwrap()
        })
        .unwrap();
        assert_eq!(
            unsafe { kb_load(bob, document.as_ptr(), document.len()) },
            STATUS_OK
        );

        assert_eq!(kb_scene(bob), STATUS_OK);
        assert!(
            last_string().contains("diamond"),
            "join handshake carried the scene"
        );

        kb_close(alice);
        kb_close(bob);
    }
}
