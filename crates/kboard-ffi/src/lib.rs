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
use std::sync::{Mutex, OnceLock};

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
pub const ABI_VERSION: u32 = 1;

#[derive(Default)]
struct Registry {
    boards: HashMap<u32, Board>,
    next_handle: u32,
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
fn with_registry<T>(action: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = registry().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    action(&mut guard)
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
    drop(Box::from_raw(std::slice::from_raw_parts_mut(ptr, len) as *mut [u8]));
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

// -- lifecycle -------------------------------------------------------------

/// Open a board in `scope` for `actor`. Returns a handle, or `0` on failure.
///
/// The engine does not decide whether this actor may open this scope — the host
/// does, before calling.
///
/// # Safety
/// `scope_ptr`/`scope_len` must describe valid UTF-8 bytes.
#[no_mangle]
pub unsafe extern "C" fn kb_open(scope_ptr: *const u8, scope_len: usize, actor: u32) -> u32 {
    let scope = match borrow_str(scope_ptr, scope_len) {
        Some(text) => text.to_owned(),
        None => return 0,
    };
    catch_unwind(AssertUnwindSafe(|| {
        with_registry(|registry| {
            registry.next_handle += 1;
            let handle = registry.next_handle;
            registry.boards.insert(handle, Board::open(&scope, u64::from(actor)));
            handle
        })
    }))
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn kb_close(handle: u32) -> u32 {
    guarded(|| {
        with_registry(|registry| {
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
    let now = if now_ms.is_finite() && now_ms > 0.0 { now_ms as u64 } else { 0 };

    guarded(|| {
        with_registry(|registry| {
            let Some(board) = registry.boards.get_mut(&handle) else {
                return STATUS_NO_BOARD;
            };
            match board.exec(&command, now) {
                Ok(id) => {
                    publish(id.unwrap_or_default().into_bytes());
                    STATUS_OK
                }
                Err(_) => STATUS_REFUSED,
            }
        })
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
        with_registry(|registry| {
            let Some(board) = registry.boards.get_mut(&handle) else {
                return STATUS_NO_BOARD;
            };
            let changed = board.merge_ops(&ops);
            publish(changed.to_string().into_bytes());
            STATUS_OK
        })
    })
}

/// Drain operations this replica produced but has not broadcast, as JSON.
#[no_mangle]
pub extern "C" fn kb_pending(handle: u32) -> u32 {
    guarded(|| {
        with_registry(|registry| {
            let Some(board) = registry.boards.get_mut(&handle) else {
                return STATUS_NO_BOARD;
            };
            let pending = board.take_pending();
            match serde_json::to_vec(&pending) {
                Ok(bytes) => {
                    publish(bytes);
                    STATUS_OK
                }
                Err(_) => STATUS_REFUSED,
            }
        })
    })
}

/// Publish the render-ready scene as JSON, in paint order.
#[no_mangle]
pub extern "C" fn kb_scene(handle: u32) -> u32 {
    guarded(|| {
        with_registry(|registry| {
            let Some(board) = registry.boards.get(&handle) else {
                return STATUS_NO_BOARD;
            };
            match serde_json::to_vec(&board.scene()) {
                Ok(bytes) => {
                    publish(bytes);
                    STATUS_OK
                }
                Err(_) => STATUS_REFUSED,
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_exec(handle: u32, json: &str) -> u32 {
        unsafe { kb_exec(handle, json.as_ptr(), json.len(), 1_000.0) }
    }

    fn last_string() -> String {
        LAST.with(|slot| String::from_utf8(slot.borrow().clone()).unwrap())
    }

    fn open(scope: &str, actor: u32) -> u32 {
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

        call_exec(alice, r#"{"cmd":"add","kind":"ellipse","x":0,"y":0,"w":5,"h":5}"#);
        assert_eq!(kb_pending(alice), STATUS_OK);
        let ops = last_string();

        assert_eq!(unsafe { kb_merge(bob, ops.as_ptr(), ops.len()) }, STATUS_OK);
        assert_eq!(kb_scene(bob), STATUS_OK);
        assert!(last_string().contains("ellipse"), "the peer received the shape");

        kb_close(alice);
        kb_close(bob);
    }

    #[test]
    fn malformed_input_is_refused_not_fatal() {
        let handle = open("t/b", 1);
        assert_eq!(call_exec(handle, "not json"), STATUS_BAD_INPUT);
        assert_eq!(call_exec(handle, r#"{"cmd":"nope"}"#), STATUS_BAD_INPUT);
        assert_eq!(
            call_exec(handle, r#"{"cmd":"add","kind":"unicorn","x":0,"y":0,"w":1,"h":1}"#),
            STATUS_REFUSED
        );
        // Still usable afterwards — the point of trapping rather than aborting.
        assert_eq!(
            call_exec(handle, r#"{"cmd":"add","kind":"diamond","x":0,"y":0,"w":1,"h":1}"#),
            STATUS_OK
        );
        kb_close(handle);
    }

    #[test]
    fn null_and_unknown_handles_are_refused() {
        assert_eq!(unsafe { kb_exec(999_999, std::ptr::null(), 0, 0.0) }, STATUS_BAD_INPUT);
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
}
