//! Per-scope room state.
//!
//! A room is an operation log plus a snapshot of everything the log has already
//! absorbed. Joining clients receive the snapshot, not the history — which is
//! what keeps a reconnect cheap on a board that has existed for months.
//!
//! Note what is *not* here: no identity, no permissions, no tenancy rules. The
//! server decides who may open a scope before this module is reached, and the
//! engine below it decides nothing at all.

use kboard_core::document::ScopeId;
use kboard_core::op::StampedOp;
use kboard_core::snapshot::Snapshot;
use tokio::sync::broadcast;

/// How many operations may accumulate before the log is folded into the
/// snapshot and truncated. Small enough here to exercise the path in a demo;
/// a real deployment tunes this against write volume.
const COMPACT_AFTER: usize = 200;

/// Broadcast payload: the connection that produced it, and the JSON to relay.
/// The origin is carried so a sender does not receive its own echo.
pub type Fanout = (u64, String);

pub struct Room {
    snapshot: Snapshot,
    /// Operations not yet folded into the snapshot.
    tail: Vec<StampedOp>,
    sender: broadcast::Sender<Fanout>,
    compactions: u64,
}

impl Room {
    pub fn new(scope: ScopeId) -> Self {
        let (sender, _) = broadcast::channel(1024);
        Self {
            snapshot: Snapshot::empty(scope),
            tail: Vec::new(),
            sender,
            compactions: 0,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Fanout> {
        self.sender.subscribe()
    }

    /// The document a joining client should start from.
    ///
    /// Snapshot plus tail — never the raw history, which may have been
    /// truncated. `Snapshot::absorb` is idempotent, so folding the tail into a
    /// clone here is safe and cheap.
    pub fn join_state(&self) -> Snapshot {
        let mut current = self.snapshot.clone();
        current.absorb(&self.tail);
        current
    }

    /// Accept operations from a client. Returns how many changed the document.
    ///
    /// Operations are retained even when they change nothing: a stale write is
    /// still part of history until compaction folds it away.
    pub fn accept(&mut self, ops: Vec<StampedOp>) -> usize {
        let mut probe = self.snapshot.clone();
        probe.absorb(&self.tail);
        let changed = probe.absorb(&ops);

        self.tail.extend(ops);
        if self.tail.len() >= COMPACT_AFTER {
            self.compact();
        }
        changed
    }

    /// Fold the tail into the snapshot and drop it.
    ///
    /// Safe because `join_state` never reads the tail independently, and every
    /// connected client already holds everything the tail contains.
    fn compact(&mut self) {
        let folded = std::mem::take(&mut self.tail);
        self.snapshot.absorb(&folded);
        self.compactions += 1;
    }

    pub fn broadcast(&self, origin: u64, payload: String) {
        // An error means nobody is listening, which is not a failure.
        let _ = self.sender.send((origin, payload));
    }

    pub fn stats(&self) -> RoomStats {
        RoomStats {
            elements: self.join_state().document().live_count(),
            tail: self.tail.len(),
            absorbed: self.snapshot.absorbed(),
            compactions: self.compactions,
            subscribers: self.sender.receiver_count(),
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct RoomStats {
    pub elements: usize,
    pub tail: usize,
    pub absorbed: u64,
    pub compactions: u64,
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

    #[test]
    fn a_joining_client_sees_everything_accepted() {
        let mut room = Room::new(ScopeId::new("t/b"));
        room.accept(ops(1, 5));
        assert_eq!(room.join_state().document().live_count(), 5);
    }

    #[test]
    fn compaction_preserves_the_document() {
        let mut room = Room::new(ScopeId::new("t/b"));
        // Enough to cross the threshold and force at least one fold.
        room.accept(ops(1, 250));

        let stats = room.stats();
        assert!(stats.compactions >= 1, "the log should have been folded");
        assert!(stats.tail < COMPACT_AFTER);
        assert_eq!(
            room.join_state().document().live_count(),
            250,
            "compaction must not lose elements"
        );
    }

    #[test]
    fn replayed_operations_change_nothing() {
        let mut room = Room::new(ScopeId::new("t/b"));
        let batch = ops(1, 4);
        assert_eq!(room.accept(batch.clone()), 4);
        assert_eq!(
            room.accept(batch),
            0,
            "a retried batch is absorbed idempotently"
        );
        assert_eq!(room.join_state().document().live_count(), 4);
    }
}
