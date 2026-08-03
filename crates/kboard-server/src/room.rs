//! Per-scope room state.
//!
//! A room holds one materialised document and a broadcast channel. That is all.
//!
//! An earlier version also kept an in-memory operation log and folded it into a
//! snapshot periodically. That was wrong twice over: the log grew without bound
//! (a memory leak on any long-lived board), and serving a join meant cloning the
//! whole document to fold the tail — O(document) work on *every incoming
//! message*. Absorbing directly makes accept O(operations) and join O(1).
//!
//! The operation log is not gone, it belongs somewhere else: durable storage
//! behind the engine's `OpLog` port, where truncation is a storage concern
//! rather than a RAM concern. See `docs/adr/0005-in-memory-room-state.md`.
//!
//! Note what is *not* here: no identity, no permissions, no tenancy rules. The
//! server decides who may open a scope before this module is reached, and the
//! engine below it decides nothing at all.

use std::time::Instant;

use kboard_core::document::{Document, ScopeId};
use kboard_core::op::StampedOp;
use kboard_core::snapshot::Snapshot;
use tokio::sync::broadcast;

use crate::limits;

/// Broadcast payload: the connection that produced it, and the JSON to relay.
/// The origin is carried so a sender does not receive its own echo.
pub type Fanout = (u64, String);

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The room is at its element ceiling. Further creation would let one board
    /// consume the process.
    RoomFull,
    /// More operations in one frame than a legitimate client sends.
    BatchTooLarge,
    /// The room accepted the batch but durable storage would not record it.
    /// Reported separately because it is a server fault, not a client one.
    NotDurable,
}

/// How many accepted operations may accrue before the room's scene is written
/// back as a snapshot. Small enough that a crash replays little; large enough
/// that ordinary drawing does not rewrite the whole document per stroke.
const SNAPSHOT_EVERY: u64 = 200;

pub struct Room {
    state: Snapshot,
    sender: broadcast::Sender<Fanout>,
    accepted: u64,
    since_snapshot: u64,
    last_active: Instant,
}

impl Room {
    pub fn new(scope: ScopeId) -> Self {
        Self::restored(scope.clone(), Snapshot::empty(scope))
    }

    /// A room rebuilt from durable storage.
    ///
    /// Indistinguishable from a fresh one afterwards: the snapshot already
    /// holds everything the log described, so nothing downstream needs to know
    /// whether this board is minutes or months old.
    pub fn restored(_scope: ScopeId, state: Snapshot) -> Self {
        let (sender, _) = broadcast::channel(1024);
        Self {
            state,
            sender,
            accepted: 0,
            since_snapshot: 0,
            last_active: Instant::now(),
        }
    }

    /// Whether enough has accrued to be worth writing the scene back.
    pub fn snapshot_due(&self) -> bool {
        self.since_snapshot >= SNAPSHOT_EVERY
    }

    /// The materialised scene, for writing back to durable storage.
    pub fn snapshot(&self) -> &Snapshot {
        &self.state
    }

    /// Records that the current scene has been persisted.
    pub fn mark_snapshotted(&mut self) {
        self.since_snapshot = 0;
    }

    /// A handle for sending without holding the server lock.
    ///
    /// Presence uses this. Cursor movement arrives tens of times a second per
    /// user, and routing it through the single server mutex would make every
    /// room contend on every other room's mouse.
    pub fn sender(&self) -> broadcast::Sender<Fanout> {
        self.sender.clone()
    }

    pub fn subscribe(&mut self) -> broadcast::Receiver<Fanout> {
        self.last_active = Instant::now();
        self.sender.subscribe()
    }

    /// The document a joining client starts from. No clone, no fold.
    pub fn document(&self) -> &Document {
        self.state.document()
    }

    /// Accept operations from a client.
    ///
    /// # Errors
    ///
    /// [`Refused::BatchTooLarge`] or [`Refused::RoomFull`] when a limit would be
    /// crossed. Both are refusals of the whole batch: applying part of it would
    /// leave peers holding operations this room rejected, which is divergence.
    pub fn accept(&mut self, ops: &[StampedOp]) -> Result<usize, Refused> {
        if ops.len() > limits::MAX_OPS_PER_FRAME {
            return Err(Refused::BatchTooLarge);
        }
        // Checked before absorbing rather than after, so the ceiling is a real
        // ceiling and not a threshold that one oversized batch can overshoot.
        if self.state.document().total_count() >= limits::MAX_ELEMENTS_PER_ROOM {
            return Err(Refused::RoomFull);
        }

        self.last_active = Instant::now();
        self.accepted += ops.len() as u64;
        self.since_snapshot += ops.len() as u64;
        Ok(self.state.absorb(ops))
    }

    pub fn broadcast(&self, origin: u64, payload: String) {
        // An error means nobody is listening, which is not a failure.
        let _ = self.sender.send((origin, payload));
    }

    /// Whether this room can be reclaimed: nobody connected, and quiet for
    /// longer than the retention window.
    pub fn is_reclaimable(&self, now: Instant) -> bool {
        self.sender.receiver_count() == 0
            && now.duration_since(self.last_active) > limits::ROOM_IDLE_TTL
    }

    pub fn stats(&self) -> RoomStats {
        RoomStats {
            elements: self.state.document().live_count(),
            tombstones: self.state.document().total_count() - self.state.document().live_count(),
            accepted: self.accepted,
            subscribers: self.sender.receiver_count(),
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct RoomStats {
    pub elements: usize,
    pub tombstones: usize,
    pub accepted: u64,
    pub subscribers: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_core::clock::{ActorId, HlcGenerator};
    use kboard_core::element::ElementId;
    use kboard_core::op::upsert;
    use kboard_core::prop::{PropKey, PropValue};

    fn ops(actor: u64, count: u128) -> Vec<StampedOp> {
        let mut clock = HlcGenerator::new(ActorId(actor));
        (0..count)
            .flat_map(|index| {
                upsert(
                    ElementId(index),
                    [(PropKey::X, PropValue::Num(index as f64))],
                    &mut clock,
                    1_000 + index as u64,
                )
            })
            .collect()
    }

    fn room() -> Room {
        Room::new(ScopeId::new("t/b"))
    }

    #[test]
    fn a_joining_client_sees_everything_accepted() {
        let mut room = room();
        room.accept(&ops(1, 5)).unwrap();
        assert_eq!(room.document().live_count(), 5);
    }

    #[test]
    fn replayed_operations_change_nothing() {
        let mut room = room();
        let batch = ops(1, 4);
        assert_eq!(room.accept(&batch).unwrap(), 4);
        assert_eq!(
            room.accept(&batch).unwrap(),
            0,
            "a retried batch is absorbed idempotently"
        );
        assert_eq!(room.document().live_count(), 4);
    }

    #[test]
    fn an_oversized_batch_is_refused_whole() {
        let mut room = room();
        let huge = ops(1, (limits::MAX_OPS_PER_FRAME + 1) as u128);
        assert_eq!(room.accept(&huge), Err(Refused::BatchTooLarge));
        assert_eq!(
            room.document().live_count(),
            0,
            "a refused batch must not partially apply"
        );
    }

    #[test]
    fn stats_report_tombstones_separately() {
        let mut room = room();
        room.accept(&ops(1, 3)).unwrap();
        let mut clock = HlcGenerator::new(ActorId(9));
        let stamp = clock.tick(9_000);
        room.accept(&[StampedOp::new(
            stamp,
            kboard_core::op::Op::Delete {
                element: ElementId(0),
            },
        )])
        .unwrap();

        let stats = room.stats();
        assert_eq!(stats.elements, 2);
        assert_eq!(stats.tombstones, 1);
    }

    #[test]
    fn an_occupied_room_is_never_reclaimed() {
        let mut room = room();
        let _subscriber = room.subscribe();
        // Far beyond the TTL, but somebody is still connected.
        let distant_future = Instant::now() + limits::ROOM_IDLE_TTL * 10;
        assert!(!room.is_reclaimable(distant_future));
    }

    #[test]
    fn an_empty_idle_room_is_reclaimable() {
        let room = room();
        assert!(
            !room.is_reclaimable(Instant::now()),
            "still within the window"
        );
        assert!(room.is_reclaimable(Instant::now() + limits::ROOM_IDLE_TTL * 2));
    }
}
