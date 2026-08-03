//! The safe half of the FFI crate.
//!
//! Everything a host actually wants to do lives here, in ordinary safe Rust
//! that can be unit-tested directly. `lib.rs` is then a thin unsafe shim that
//! only marshals bytes and traps panics.
//!
//! Splitting it this way keeps the unsafe surface small enough to audit by
//! reading it once.

use serde::{Deserialize, Serialize};

use kboard_core::clock::{ActorId, HlcGenerator};
use kboard_core::document::{Document, MergeError, ScopeId};
use kboard_core::element::ElementId;
use kboard_core::frac;
use kboard_core::op::{self, Op, StampedOp};
use kboard_core::prop::{ElementKind, Point, PropKey, PropValue};

/// A command from a host. JSON-tagged so the wire surface stays one string in,
/// one string out — the narrowest thing that works across every FFI host.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Add {
        kind: String,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        #[serde(default)]
        stroke: u32,
        #[serde(default)]
        fill: u32,
        #[serde(default = "default_stroke_width")]
        stroke_width: f64,
    },
    Stroke {
        points: Vec<[f64; 2]>,
        #[serde(default)]
        stroke: u32,
        #[serde(default = "default_stroke_width")]
        stroke_width: f64,
    },
    Move {
        id: String,
        x: f64,
        y: f64,
    },
    Resize {
        id: String,
        w: f64,
        h: f64,
    },
    Style {
        id: String,
        #[serde(default)]
        stroke: Option<u32>,
        #[serde(default)]
        fill: Option<u32>,
    },
    Delete {
        id: String,
    },
    Clear,
}

const fn default_stroke_width() -> f64 {
    2.0
}

/// One element, flattened for rendering. The client never walks the CRDT.
#[derive(Clone, Debug, Serialize)]
pub struct SceneItem {
    pub id: String,
    pub kind: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub stroke: u32,
    pub fill: u32,
    pub stroke_width: f64,
    pub z: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<[f64; 2]>>,
}

#[derive(Debug)]
pub enum BoardError {
    BadCommand(String),
    UnknownElement,
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadCommand(detail) => write!(formatter, "bad command: {detail}"),
            Self::UnknownElement => formatter.write_str("unknown element"),
        }
    }
}

// Public API: callers must be able to put this in a `Box<dyn Error>` or a
// `?` chain like any other error type.
impl std::error::Error for BoardError {}

/// One step of a reversal, stored without a stamp.
///
/// Undo is a *new* write of a prior value (ADR-0001), not a retraction of the
/// original one. It therefore needs a fresh stamp at the moment it is applied,
/// which is why nothing here carries the stamp the original edit had.
#[derive(Clone, Debug)]
enum Reversal {
    Set(ElementId, PropKey, PropValue),
    Delete(ElementId),
}

/// One reversible step: how to undo it, and how to put it back.
#[derive(Clone, Debug)]
struct Change {
    backward: Vec<Reversal>,
    forward: Vec<Reversal>,
}

/// How many steps a single actor may walk back.
///
/// Bounded because the stack holds prior property values, so an unbounded one
/// grows with editing rather than with board size — and ADR-0007 keeps it in
/// memory, where growth has nowhere to go.
const MAX_HISTORY: usize = 200;

/// A live board: the document, this replica's clock, and the operations that
/// have not yet been broadcast.
pub struct Board {
    document: Document,
    clock: HlcGenerator,
    pending: Vec<StampedOp>,
    /// Element ids are `(actor, counter)`, so they are globally unique without
    /// a random source. The engine stays deterministic and the host does not
    /// have to supply entropy across the FFI boundary.
    next_local: u64,
    /// This actor's own history. Never persisted (ADR-0007) and never
    /// populated by remote operations: undoing a collaborator's edit is not
    /// undo, it is editing their work, and it should take the same deliberate
    /// action as any other change.
    undone: Vec<Change>,
    redone: Vec<Change>,
}

impl Board {
    pub fn open(scope: &str, actor: u64) -> Self {
        Self {
            document: Document::new(ScopeId::new(scope)),
            clock: HlcGenerator::new(ActorId(actor)),
            pending: Vec::new(),
            next_local: 0,
            undone: Vec::new(),
            redone: Vec::new(),
        }
    }

    pub fn scope(&self) -> &ScopeId {
        self.document.scope()
    }

    fn mint_id(&mut self) -> ElementId {
        self.next_local += 1;
        ElementId((u128::from(self.clock.actor().0) << 64) | u128::from(self.next_local))
    }

    fn record(&mut self, ops: Vec<StampedOp>) {
        op::apply_all(&mut self.document, &ops);
        self.pending.extend(ops);
    }

    /// Apply a local command and remember how to reverse it.
    ///
    /// New work discards the redo stack. Keeping it would let a user redo
    /// their way to a state that never existed — the redone edit would land on
    /// top of work done after it was undone.
    fn record_change(&mut self, ops: Vec<StampedOp>, change: Change) {
        self.record(ops);
        self.redone.clear();
        self.undone.push(change);
        if self.undone.len() > MAX_HISTORY {
            self.undone.remove(0);
        }
    }

    /// The current values of `keys`, for restoring later.
    ///
    /// A key with no current value is omitted rather than recorded as absent:
    /// the engine has no way to un-write a property, and every command that
    /// creates properties from nothing is reversed by deleting the element, so
    /// the stray values are never visible.
    fn capture(&self, element: ElementId, keys: &[PropKey]) -> Vec<Reversal> {
        let Some(current) = self.document.get(element) else {
            return Vec::new();
        };
        keys.iter()
            .filter_map(|key| {
                current
                    .get(key)
                    .map(|value| Reversal::Set(element, key.clone(), value.clone()))
            })
            .collect()
    }

    fn stamp(&mut self, reversals: &[Reversal], now_ms: u64) -> Vec<StampedOp> {
        reversals
            .iter()
            .map(|reversal| {
                let stamp = self.clock.tick(now_ms);
                match reversal {
                    Reversal::Set(element, key, value) => StampedOp::new(
                        stamp,
                        Op::Set {
                            element: *element,
                            key: key.clone(),
                            value: value.clone(),
                        },
                    ),
                    Reversal::Delete(element) => {
                        StampedOp::new(stamp, Op::Delete { element: *element })
                    }
                }
            })
            .collect()
    }

    pub fn can_undo(&self) -> bool {
        !self.undone.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redone.is_empty()
    }

    /// Reverse this actor's most recent change.
    ///
    /// The reversal is an ordinary edit carrying a fresh, higher stamp, so it
    /// converges like any other and wins over a concurrent change to the same
    /// property. That is deliberate: undo means "make it what it was", and a
    /// user who presses it expects the board to show what they remember, not
    /// to negotiate with a collaborator's later edit.
    pub fn undo(&mut self, now_ms: u64) -> bool {
        let Some(change) = self.undone.pop() else {
            return false;
        };
        let ops = self.stamp(&change.backward, now_ms);
        self.record(ops);
        self.redone.push(change);
        true
    }

    /// Reapply the most recently undone change.
    pub fn redo(&mut self, now_ms: u64) -> bool {
        let Some(change) = self.redone.pop() else {
            return false;
        };
        let ops = self.stamp(&change.forward, now_ms);
        self.record(ops);
        self.undone.push(change);
        true
    }

    /// Execute a host command. Returns the affected element id, if any.
    pub fn exec(&mut self, command: &Command, now_ms: u64) -> Result<Option<String>, BoardError> {
        match command {
            Command::Add {
                kind,
                x,
                y,
                w,
                h,
                stroke,
                fill,
                stroke_width,
            } => {
                let element_kind = parse_kind(kind)?;
                let id = self.mint_id();
                let z = self.document.z_index_for_top();
                let props = vec![
                    (PropKey::Kind, PropValue::Kind(element_kind)),
                    (PropKey::X, PropValue::Num(*x)),
                    (PropKey::Y, PropValue::Num(*y)),
                    (PropKey::Width, PropValue::Num(*w)),
                    (PropKey::Height, PropValue::Num(*h)),
                    (PropKey::Stroke, PropValue::Color(*stroke)),
                    (PropKey::Fill, PropValue::Color(*fill)),
                    (PropKey::StrokeWidth, PropValue::Num(*stroke_width)),
                    (PropKey::ZIndex, PropValue::Text(z)),
                ];
                let ops = op::upsert(id, props.clone(), &mut self.clock, now_ms);
                self.record_change(ops, creation(id, props));
                Ok(Some(id.to_hex()))
            }

            Command::Stroke {
                points,
                stroke,
                stroke_width,
            } => {
                if points.is_empty() {
                    return Err(BoardError::BadCommand("empty stroke".into()));
                }
                let id = self.mint_id();
                let z = self.document.z_index_for_top();
                let path: Vec<Point> = points.iter().map(|[x, y]| Point::new(*x, *y)).collect();
                // Freehand geometry lives in `points`; x/y anchor the bounding
                // box so hit-testing and export do not have to scan the path.
                let (min_x, min_y) = path.iter().fold((f64::MAX, f64::MAX), |(mx, my), p| {
                    (mx.min(p.x), my.min(p.y))
                });
                let props = vec![
                    (PropKey::Kind, PropValue::Kind(ElementKind::Freedraw)),
                    (PropKey::X, PropValue::Num(min_x)),
                    (PropKey::Y, PropValue::Num(min_y)),
                    (PropKey::Points, PropValue::Points(path)),
                    (PropKey::Stroke, PropValue::Color(*stroke)),
                    (PropKey::StrokeWidth, PropValue::Num(*stroke_width)),
                    (PropKey::ZIndex, PropValue::Text(z)),
                ];
                let ops = op::upsert(id, props.clone(), &mut self.clock, now_ms);
                self.record_change(ops, creation(id, props));
                Ok(Some(id.to_hex()))
            }

            Command::Move { id, x, y } => {
                let element = self.require(id)?;
                let props = vec![
                    (PropKey::X, PropValue::Num(*x)),
                    (PropKey::Y, PropValue::Num(*y)),
                ];
                let backward = self.capture(element, &[PropKey::X, PropKey::Y]);
                let ops = op::upsert(element, props.clone(), &mut self.clock, now_ms);
                self.record_change(ops, mutation(element, backward, props));
                Ok(Some(id.clone()))
            }

            Command::Resize { id, w, h } => {
                let element = self.require(id)?;
                let props = vec![
                    (PropKey::Width, PropValue::Num(*w)),
                    (PropKey::Height, PropValue::Num(*h)),
                ];
                let backward = self.capture(element, &[PropKey::Width, PropKey::Height]);
                let ops = op::upsert(element, props.clone(), &mut self.clock, now_ms);
                self.record_change(ops, mutation(element, backward, props));
                Ok(Some(id.clone()))
            }

            Command::Style { id, stroke, fill } => {
                let element = self.require(id)?;
                let mut props = Vec::new();
                if let Some(colour) = stroke {
                    props.push((PropKey::Stroke, PropValue::Color(*colour)));
                }
                if let Some(colour) = fill {
                    props.push((PropKey::Fill, PropValue::Color(*colour)));
                }
                let keys: Vec<PropKey> = props.iter().map(|(key, _)| key.clone()).collect();
                let backward = self.capture(element, &keys);
                let ops = op::upsert(element, props.clone(), &mut self.clock, now_ms);
                self.record_change(ops, mutation(element, backward, props));
                Ok(Some(id.clone()))
            }

            Command::Delete { id } => {
                let element = self.require(id)?;
                let stamp = self.clock.tick(now_ms);
                let change = Change {
                    // Un-deleting restores the element whole: the engine keeps a
                    // tombstone rather than removing it, so every property is
                    // still there waiting.
                    backward: vec![Reversal::Set(
                        element,
                        PropKey::Deleted,
                        PropValue::Bool(false),
                    )],
                    forward: vec![Reversal::Delete(element)],
                };
                self.record_change(vec![StampedOp::new(stamp, Op::Delete { element })], change);
                Ok(Some(id.clone()))
            }

            Command::Clear => {
                // Expanded to explicit deletes at this replica; see op::clear.
                let ops = op::clear(&self.document, &mut self.clock, now_ms);
                let cleared: Vec<ElementId> = ops.iter().map(StampedOp::element).collect();
                let change = Change {
                    backward: cleared
                        .iter()
                        .map(|element| {
                            Reversal::Set(*element, PropKey::Deleted, PropValue::Bool(false))
                        })
                        .collect(),
                    forward: cleared.iter().copied().map(Reversal::Delete).collect(),
                };
                self.record_change(ops, change);
                Ok(None)
            }
        }
    }

    fn require(&self, id: &str) -> Result<ElementId, BoardError> {
        let element = ElementId::from_hex(id).ok_or(BoardError::UnknownElement)?;
        if self.document.get(element).is_some() {
            Ok(element)
        } else {
            Err(BoardError::UnknownElement)
        }
    }

    /// Merge operations from a peer. Returns how many changed the document.
    pub fn merge_ops(&mut self, ops: &[StampedOp]) -> usize {
        // Advance the local clock past anything observed, so a subsequent local
        // edit is ordered after what it reacts to.
        if let Some(highest) = ops.iter().map(|stamped| stamped.stamp).max() {
            self.clock.observe(highest, highest.wall);
        }
        op::apply_all(&mut self.document, ops)
    }

    /// Merge a whole document — the join handshake.
    ///
    /// A server that has compacted its log cannot replay history it truncated,
    /// so it sends the materialised document instead. This is also where a
    /// misrouted board is caught: merging across scopes is refused outright.
    ///
    /// # Errors
    ///
    /// [`MergeError::ScopeMismatch`] if the incoming document belongs to a
    /// different tenant.
    pub fn merge_document(&mut self, incoming: &Document) -> Result<bool, MergeError> {
        if let Some(highest) = incoming.max_stamp() {
            self.clock.observe(highest, highest.wall);
        }
        self.document.merge(incoming)
    }

    /// Drain the operations this replica has produced but not yet sent.
    pub fn take_pending(&mut self) -> Vec<StampedOp> {
        std::mem::take(&mut self.pending)
    }

    /// Render-ready scene, in paint order.
    pub fn scene(&self) -> Vec<SceneItem> {
        self.document
            .ordered()
            .into_iter()
            .map(|element| SceneItem {
                id: element.id().to_hex(),
                kind: element.kind().map_or("rectangle", kind_name).to_owned(),
                x: element.x(),
                y: element.y(),
                w: element.width(),
                h: element.height(),
                stroke: colour(element.get(&PropKey::Stroke)),
                fill: colour(element.get(&PropKey::Fill)),
                stroke_width: element.num_or(PropKey::StrokeWidth, 2.0),
                z: element.z_index().to_owned(),
                points: element
                    .points()
                    .map(|path| path.iter().map(|p| [p.x, p.y]).collect()),
            })
            .collect()
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    /// Place a new element above everything currently visible.
    pub fn top_z(&self) -> String {
        let highest = self
            .document
            .ordered()
            .last()
            .map(|e| e.z_index().to_owned());
        frac::between(highest.as_deref().filter(|k| !k.is_empty()), None)
    }
}

/// Reversing a creation means deleting the element; reapplying it means
/// writing the properties back and lifting the tombstone.
fn creation(element: ElementId, props: Vec<(PropKey, PropValue)>) -> Change {
    let mut forward: Vec<Reversal> = props
        .into_iter()
        .map(|(key, value)| Reversal::Set(element, key, value))
        .collect();
    forward.push(Reversal::Set(
        element,
        PropKey::Deleted,
        PropValue::Bool(false),
    ));
    Change {
        backward: vec![Reversal::Delete(element)],
        forward,
    }
}

/// Reversing a property change means writing back what was there.
fn mutation(
    element: ElementId,
    backward: Vec<Reversal>,
    props: Vec<(PropKey, PropValue)>,
) -> Change {
    Change {
        backward,
        forward: props
            .into_iter()
            .map(|(key, value)| Reversal::Set(element, key, value))
            .collect(),
    }
}

fn colour(value: Option<&PropValue>) -> u32 {
    match value {
        Some(PropValue::Color(packed)) => *packed,
        _ => 0,
    }
}

const fn kind_name(kind: ElementKind) -> &'static str {
    match kind {
        ElementKind::Rectangle => "rectangle",
        ElementKind::Ellipse => "ellipse",
        ElementKind::Diamond => "diamond",
        ElementKind::Line => "line",
        ElementKind::Arrow => "arrow",
        ElementKind::Freedraw => "freedraw",
        ElementKind::Text => "text",
        ElementKind::Frame => "frame",
        ElementKind::Image => "image",
    }
}

fn parse_kind(name: &str) -> Result<ElementKind, BoardError> {
    Ok(match name {
        "rectangle" => ElementKind::Rectangle,
        "ellipse" => ElementKind::Ellipse,
        "diamond" => ElementKind::Diamond,
        "line" => ElementKind::Line,
        "arrow" => ElementKind::Arrow,
        "freedraw" => ElementKind::Freedraw,
        "text" => ElementKind::Text,
        "frame" => ElementKind::Frame,
        other => return Err(BoardError::BadCommand(format!("unknown kind {other}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_rect(board: &mut Board, x: f64) -> String {
        board
            .exec(
                &Command::Add {
                    kind: "rectangle".into(),
                    x,
                    y: 0.0,
                    w: 10.0,
                    h: 10.0,
                    stroke: 0xFF,
                    fill: 0,
                    stroke_width: 2.0,
                },
                1_000,
            )
            .unwrap()
            .unwrap()
    }

    #[test]
    fn element_ids_are_unique_per_actor_without_randomness() {
        let mut alice = Board::open("t/b", 1);
        let mut bob = Board::open("t/b", 2);

        let from_alice = add_rect(&mut alice, 0.0);
        let from_bob = add_rect(&mut bob, 0.0);

        assert_ne!(from_alice, from_bob, "two replicas must not collide");
    }

    #[test]
    fn two_boards_converge_through_pending_exchange() {
        let mut alice = Board::open("t/b", 1);
        let mut bob = Board::open("t/b", 2);

        let id = add_rect(&mut alice, 5.0);
        let alice_ops = alice.take_pending();
        bob.merge_ops(&alice_ops);

        // Now each edits a different property, concurrently.
        alice
            .exec(
                &Command::Move {
                    id: id.clone(),
                    x: 99.0,
                    y: 0.0,
                },
                1_100,
            )
            .unwrap();
        bob.exec(
            &Command::Style {
                id: id.clone(),
                stroke: Some(0xABCDEF00),
                fill: None,
            },
            1_100,
        )
        .unwrap();

        let a_ops = alice.take_pending();
        let b_ops = bob.take_pending();
        alice.merge_ops(&b_ops);
        bob.merge_ops(&a_ops);

        assert_eq!(alice.document(), bob.document(), "replicas converge");
        let item = &alice.scene()[0];
        assert_eq!(item.x, 99.0, "the move survived");
        assert_eq!(item.stroke, 0xABCDEF00, "the restyle survived");
    }

    #[test]
    fn scene_is_returned_in_paint_order() {
        let mut board = Board::open("t/b", 1);
        add_rect(&mut board, 0.0);
        add_rect(&mut board, 1.0);
        add_rect(&mut board, 2.0);

        let scene = board.scene();
        assert_eq!(scene.len(), 3);
        assert!(scene[0].z < scene[1].z && scene[1].z < scene[2].z);
    }

    #[test]
    fn clear_removes_everything_this_replica_can_see() {
        let mut board = Board::open("t/b", 1);
        add_rect(&mut board, 0.0);
        add_rect(&mut board, 1.0);
        board.exec(&Command::Clear, 2_000).unwrap();
        assert!(board.scene().is_empty());
    }

    #[test]
    fn commands_against_unknown_elements_are_refused() {
        let mut board = Board::open("t/b", 1);
        let missing = board.exec(
            &Command::Move {
                id: "00".repeat(16),
                x: 0.0,
                y: 0.0,
            },
            1_000,
        );
        assert!(matches!(missing, Err(BoardError::UnknownElement)));
    }

    #[test]
    fn freehand_strokes_keep_their_path() {
        let mut board = Board::open("t/b", 1);
        board
            .exec(
                &Command::Stroke {
                    points: vec![[0.0, 0.0], [5.0, 9.0], [10.0, 2.0]],
                    stroke: 0xFF,
                    stroke_width: 3.0,
                },
                1_000,
            )
            .unwrap();

        let item = &board.scene()[0];
        assert_eq!(item.kind, "freedraw");
        assert_eq!(item.points.as_ref().unwrap().len(), 3);
        assert_eq!(item.x, 0.0, "bounding origin tracks the path minimum");
    }

    #[test]
    fn undo_removes_a_created_shape_and_redo_brings_it_back() {
        let mut board = Board::open("t/b", 1);
        let id = add_rect(&mut board, 5.0);

        assert!(board.can_undo() && !board.can_redo());
        assert!(board.undo(2_000));
        assert!(board.scene().is_empty(), "the shape is gone");

        assert!(board.can_redo());
        assert!(board.redo(3_000));
        assert_eq!(board.scene().len(), 1);
        assert_eq!(board.scene()[0].id, id, "the same element returns");
    }

    #[test]
    fn undo_restores_the_previous_value_rather_than_erasing_the_property() {
        let mut board = Board::open("t/b", 1);
        let id = add_rect(&mut board, 5.0);
        board
            .exec(
                &Command::Move {
                    id: id.clone(),
                    x: 250.0,
                    y: 40.0,
                },
                2_000,
            )
            .unwrap();
        assert_eq!(board.scene()[0].x, 250.0);

        board.undo(3_000);
        assert_eq!(board.scene()[0].x, 5.0, "back to where it was");
        board.redo(4_000);
        assert_eq!(board.scene()[0].x, 250.0);
    }

    #[test]
    fn undoing_a_delete_restores_the_whole_element() {
        let mut board = Board::open("t/b", 1);
        let id = add_rect(&mut board, 7.0);
        board
            .exec(&Command::Delete { id: id.clone() }, 2_000)
            .unwrap();
        assert!(board.scene().is_empty());

        board.undo(3_000);
        let restored = &board.scene()[0];
        // The engine tombstones rather than removes, so every property is
        // still there and lifting the flag brings the shape back intact.
        assert_eq!(restored.id, id);
        assert_eq!(restored.x, 7.0);
        assert_eq!(restored.kind, "rectangle");
    }

    #[test]
    fn undoing_a_clear_restores_every_element_it_removed() {
        let mut board = Board::open("t/b", 1);
        add_rect(&mut board, 1.0);
        add_rect(&mut board, 2.0);
        board.exec(&Command::Clear, 2_000).unwrap();
        assert!(board.scene().is_empty());

        board.undo(3_000);
        assert_eq!(board.scene().len(), 2, "a clear is one step, not two");
    }

    #[test]
    fn new_work_discards_the_redo_stack() {
        let mut board = Board::open("t/b", 1);
        add_rect(&mut board, 1.0);
        board.undo(2_000);
        assert!(board.can_redo());

        add_rect(&mut board, 9.0);
        // Redoing now would land the old edit on top of work done after it was
        // undone, producing a state that never existed.
        assert!(!board.can_redo());
    }

    #[test]
    fn undo_is_an_ordinary_operation_that_peers_receive() {
        let mut alice = Board::open("t/b", 1);
        let mut bob = Board::open("t/b", 2);

        add_rect(&mut alice, 5.0);
        bob.merge_ops(&alice.take_pending());
        assert_eq!(bob.scene().len(), 1);

        alice.undo(2_000);
        let reversal = alice.take_pending();
        assert!(!reversal.is_empty(), "undo must be broadcast like any edit");

        bob.merge_ops(&reversal);
        assert!(bob.scene().is_empty(), "the peer sees the undo");
        assert_eq!(alice.document(), bob.document());
    }

    #[test]
    fn undo_does_nothing_on_an_empty_history() {
        let mut board = Board::open("t/b", 1);
        assert!(!board.undo(1_000));
        assert!(!board.redo(1_000));
        assert!(!board.can_undo() && !board.can_redo());
    }

    #[test]
    fn a_remote_operation_never_enters_local_history() {
        let mut alice = Board::open("t/b", 1);
        let mut bob = Board::open("t/b", 2);
        add_rect(&mut alice, 5.0);

        bob.merge_ops(&alice.take_pending());
        // Undoing a collaborator's edit is editing their work, not undo.
        assert!(!bob.can_undo());
    }

    #[test]
    fn history_is_bounded() {
        let mut board = Board::open("t/b", 1);
        for step in 0..(MAX_HISTORY + 20) {
            add_rect(&mut board, step as f64);
        }
        assert_eq!(
            board.undone.len(),
            MAX_HISTORY,
            "the stack must not grow without bound"
        );
    }
}
