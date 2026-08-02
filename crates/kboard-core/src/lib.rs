//! # kboard-core
//!
//! The convergent document engine behind k-board: a multi-tenant collaborative
//! canvas that is meant to work equally well as its own product and as a
//! component inside someone else's platform.
//!
//! ## The two design commitments
//!
//! **Conflict resolution is per property, not per element.** Editors that merge
//! whole elements lose one of two concurrent edits whenever two people touch
//! the same shape — one drags it, the other recolours it, and one of those
//! edits vanishes with no conflict signal. Every property here is an
//! independently convergent register, so both survive.
//!
//! **The engine holds no ambient authority.** It cannot read a clock, resolve
//! an identity, decide a permission, or reach a database. Each of those is a
//! [`ports`] trait the host implements. That is what lets the same compiled
//! artifact run inside a host's process and as a standalone service, without a
//! second implementation of the rule that must never disagree: the merge.
//!
//! ## Delivery
//!
//! One crate, four targets: `wasm32` for browsers, `cdylib` with a C ABI for
//! foreign runtimes (BEAM, JVM, CPython, Node, .NET), `staticlib` for hosts
//! that link statically, and a native binary for the standalone server. Client
//! and server therefore run *the same machine code* for merge, which removes
//! the largest source of bugs in collaborative editors.
//!
//! ## Example
//!
//! ```
//! use kboard_core::clock::{ActorId, HlcGenerator};
//! use kboard_core::document::{Document, ScopeId};
//! use kboard_core::element::ElementId;
//! use kboard_core::op::{apply_all, upsert};
//! use kboard_core::prop::{ElementKind, PropKey, PropValue};
//!
//! // Two people, one board, no coordination.
//! let scope = ScopeId::new("tenant-42/board-7");
//! let mut alice_clock = HlcGenerator::new(ActorId(1));
//! let mut bob_clock = HlcGenerator::new(ActorId(2));
//!
//! let mut alice = Document::new(scope.clone());
//! apply_all(&mut alice, &upsert(
//!     ElementId(1),
//!     [(PropKey::Kind, PropValue::Kind(ElementKind::Rectangle)),
//!      (PropKey::X, PropValue::Num(0.0))],
//!     &mut alice_clock, 1_000,
//! ));
//!
//! let mut bob = alice.clone();
//!
//! // Alice drags it. Bob recolours it. Neither has seen the other.
//! apply_all(&mut alice, &upsert(
//!     ElementId(1), [(PropKey::X, PropValue::Num(250.0))], &mut alice_clock, 1_100));
//! apply_all(&mut bob, &upsert(
//!     ElementId(1), [(PropKey::Stroke, PropValue::Color(0xFF0000FF))], &mut bob_clock, 1_100));
//!
//! alice.merge(&bob).unwrap();
//! bob.merge(&alice).unwrap();
//!
//! assert_eq!(alice, bob);                                    // they converge
//! let shape = alice.get(ElementId(1)).unwrap();
//! assert_eq!(shape.num(PropKey::X), Some(250.0));            // the drag survived
//! assert_eq!(shape.get(&PropKey::Stroke),
//!            Some(&PropValue::Color(0xFF0000FF)));           // so did the recolour
//! ```

// An embeddable engine is linked into other people's processes. A memory-safety
// bug here is their outage, under their SLA. No unsafe code, ever.
#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod clock;
pub mod document;
pub mod element;
pub mod frac;
pub mod lww;
pub mod op;
pub mod ports;
pub mod prop;
pub mod snapshot;

pub use clock::{ActorId, Hlc, HlcGenerator};
pub use document::{Document, MergeError, ScopeId};
pub use element::{Element, ElementId};
pub use op::{Op, StampedOp};
pub use prop::{ElementKind, Point, PropKey, PropValue};
pub use snapshot::Snapshot;

/// Crate version, surfaced so hosts can assert the wire format they compiled
/// against — the WASM and native artifacts must always be the same build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
