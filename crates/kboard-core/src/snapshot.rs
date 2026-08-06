//! Snapshots: bounded join cost for unbounded history.
//!
//! An append-only operation log is the right durability model — it gives
//! idempotency, replay, and audit for free — but replaying it on every join is
//! O(history). A board that has accumulated tens of thousands of operations
//! then costs a large transfer and a slow render every time someone reconnects,
//! and reconnects are frequent on mobile.
//!
//! A snapshot is the materialised document plus the stamp it has absorbed
//! through. Hosts store `snapshot + tail`, so joining is O(snapshot + recent)
//! no matter how long the board has existed. Truncating the log behind a stored
//! snapshot is then safe.

use serde::{Deserialize, Serialize};

use crate::clock::Hlc;
use crate::document::{Document, MergeError, ScopeId};
use crate::op::{self, StampedOp};

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    document: Document,
    /// Highest stamp absorbed. Operations at or below this are already folded
    /// in; the host may truncate them.
    through: Option<Hlc>,
    /// How many operations this snapshot replaced. Hosts use it to decide when
    /// re-snapshotting is worth the write.
    absorbed: u64,
}

/// Immutable materialized state paired with the exact durable log sequence it
/// includes. The sequence is storage ordering, not the snapshot's HLC horizon.
#[derive(Clone, PartialEq, Debug)]
pub struct CapturedSnapshot {
    snapshot: Snapshot,
    through_sequence: u64,
}

impl CapturedSnapshot {
    pub const fn new(snapshot: Snapshot, through_sequence: u64) -> Self {
        Self {
            snapshot,
            through_sequence,
        }
    }

    pub const fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub const fn through_sequence(&self) -> u64 {
        self.through_sequence
    }

    pub const fn scope(&self) -> &ScopeId {
        self.snapshot.scope()
    }

    pub fn into_snapshot(self) -> Snapshot {
        self.snapshot
    }
}

impl Snapshot {
    pub fn empty(scope: ScopeId) -> Self {
        Self {
            document: Document::new(scope),
            through: None,
            absorbed: 0,
        }
    }

    /// Fold an operation log into a document.
    ///
    /// Order-independent: the log may arrive in any order and produce the same
    /// snapshot, which is what makes it safe to build one from a paginated read
    /// or several concurrent readers.
    pub fn materialize(scope: ScopeId, ops: &[StampedOp]) -> Self {
        let mut snapshot = Self::empty(scope);
        snapshot.absorb(ops);
        snapshot
    }

    /// Fold more operations into an existing snapshot.
    pub fn absorb(&mut self, ops: &[StampedOp]) -> usize {
        let changed = op::apply_all(&mut self.document, ops);
        // `through` advances to the highest stamp *seen*, not the highest that
        // changed something — a stale op is still absorbed and still truncatable.
        if let Some(highest) = ops.iter().map(|stamped| stamped.stamp).max() {
            self.through = Some(self.through.map_or(highest, |current| current.max(highest)));
        }
        self.absorbed += ops.len() as u64;
        changed
    }

    pub const fn document(&self) -> &Document {
        &self.document
    }

    pub fn into_document(self) -> Document {
        self.document
    }

    pub const fn through(&self) -> Option<Hlc> {
        self.through
    }

    pub const fn absorbed(&self) -> u64 {
        self.absorbed
    }

    pub const fn scope(&self) -> &ScopeId {
        self.document.scope()
    }

    /// Whether an operation is already folded in and may be dropped from a tail.
    pub fn covers(&self, stamped: &StampedOp) -> bool {
        self.through.is_some_and(|through| stamped.stamp <= through)
    }

    /// Merge a peer's snapshot.
    ///
    /// # Errors
    ///
    /// [`MergeError::ScopeMismatch`] if the snapshots belong to different
    /// tenants.
    pub fn merge(&mut self, other: &Self) -> Result<bool, MergeError> {
        let changed = self.document.merge(&other.document)?;
        if let Some(theirs) = other.through {
            self.through = Some(self.through.map_or(theirs, |ours| ours.max(theirs)));
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{ActorId, HlcGenerator};
    use crate::element::ElementId;
    use crate::op::upsert;
    use crate::prop::{PropKey, PropValue};

    fn scope() -> ScopeId {
        ScopeId::new("t/b")
    }

    fn sample_log() -> Vec<StampedOp> {
        let mut clock = HlcGenerator::new(ActorId(1));
        let mut ops = Vec::new();
        for index in 0..12u128 {
            ops.extend(upsert(
                ElementId(index),
                [
                    (PropKey::X, PropValue::Num(index as f64)),
                    (PropKey::Y, PropValue::Num(0.0)),
                ],
                &mut clock,
                1_000 + index as u64,
            ));
        }
        ops
    }

    #[test]
    fn snapshot_equals_full_replay() {
        let ops = sample_log();

        let snapshot = Snapshot::materialize(scope(), &ops);

        let mut replayed = Document::new(scope());
        op::apply_all(&mut replayed, &ops);

        assert_eq!(snapshot.document(), &replayed);
    }

    #[test]
    fn snapshot_is_independent_of_log_order() {
        let ops = sample_log();
        let mut reversed = ops.clone();
        reversed.reverse();

        assert_eq!(
            Snapshot::materialize(scope(), &ops).document(),
            Snapshot::materialize(scope(), &reversed).document()
        );
    }

    #[test]
    fn snapshot_plus_tail_equals_the_whole_log() {
        // This is the property that lets a host truncate. If it fails, joining
        // clients silently lose history.
        let ops = sample_log();
        let (head, tail) = ops.split_at(9);

        let mut compacted = Snapshot::materialize(scope(), head);
        compacted.absorb(tail);

        let whole = Snapshot::materialize(scope(), &ops);
        assert_eq!(compacted.document(), whole.document());
    }

    #[test]
    fn covers_identifies_truncatable_operations() {
        let ops = sample_log();
        let (head, tail) = ops.split_at(6);
        let snapshot = Snapshot::materialize(scope(), head);

        assert!(head.iter().all(|stamped| snapshot.covers(stamped)));
        assert!(tail.iter().all(|stamped| !snapshot.covers(stamped)));
    }

    #[test]
    fn absorbing_the_same_operations_twice_is_a_no_op() {
        let ops = sample_log();
        let mut snapshot = Snapshot::materialize(scope(), &ops);
        let before = snapshot.document().clone();

        assert_eq!(snapshot.absorb(&ops), 0);
        assert_eq!(snapshot.document(), &before);
    }

    #[test]
    fn cross_tenant_snapshot_merge_is_refused() {
        let mut mine = Snapshot::empty(ScopeId::new("tenant-a/b"));
        let theirs = Snapshot::empty(ScopeId::new("tenant-b/b"));
        assert!(mine.merge(&theirs).is_err());
    }
}
