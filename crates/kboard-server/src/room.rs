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

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kboard_core::clock::ActorId;
use kboard_core::document::{Document, ScopeId};
use kboard_core::op::StampedOp;
use kboard_core::snapshot::{CapturedSnapshot, Snapshot};
use tokio::sync::broadcast;

use crate::limits;
use crate::telemetry::{LatencyMetric, LatencySnapshot};

/// Version-specific broadcast payloads for one accepted room event.
///
/// The origin is carried so a sender does not receive its own echo. Keeping
/// both encodings in one ordered event lets v1 and v2 peers share a room
/// without parsing and reserializing inside each connection task.
#[derive(Clone, Debug)]
pub struct Fanout {
    pub origin: u64,
    pub v1: String,
    pub v2: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// A newly materialised element does not belong to the authenticated
    /// replica actor.
    ActorMismatch,
    /// A protocol batch identity was reused with different operations.
    BatchConflict,
    /// The room is at its element ceiling. Further creation would let one board
    /// consume the process.
    RoomFull,
    /// More operations in one frame than a legitimate client sends.
    BatchTooLarge,
    /// At least one property value is outside the engine's semantic bounds.
    /// The entire batch is refused so no peer or log observes a partial edit.
    InvalidOperation,
}

/// A whole batch that has passed the room's current semantic and capacity
/// checks. Only this module can construct one, keeping mutation downstream of
/// successful validation.
pub struct PreparedBatch<'a> {
    operations: &'a [StampedOp],
}

impl PreparedBatch<'_> {
    #[cfg(test)]
    pub const fn operations(&self) -> &[StampedOp] {
        self.operations
    }
}

/// How many accepted operations may accrue before the room's scene is written
/// back as a snapshot. Small enough that a crash replays little; large enough
/// that ordinary drawing does not rewrite the whole document per stroke.
const SNAPSHOT_EVERY: u64 = 200;
const SNAPSHOT_RETRY_BASE: Duration = Duration::from_secs(1);
const SNAPSHOT_RETRY_MAX: Duration = Duration::from_secs(60);

pub struct Room {
    state: Snapshot,
    sender: broadcast::Sender<Fanout>,
    accepted: u64,
    durable_sequence: u64,
    last_snapshotted_sequence: u64,
    snapshot_failures: u32,
    snapshot_retry_at: Option<Instant>,
    snapshot_capture_micros: u64,
    snapshot_encode_micros: u64,
    snapshot_write_micros: u64,
    snapshot_truncated_operations: u64,
    payload_bytes: usize,
    last_active: Instant,
    restore_latency: LatencyMetric,
    validation_latency: LatencyMetric,
    append_latency: LatencyMetric,
    apply_latency: LatencyMetric,
    ack_latency: LatencyMetric,
    broadcast_latency: LatencyMetric,
}

impl Room {
    #[cfg(test)]
    pub fn new(scope: ScopeId) -> Self {
        Self::restored(
            scope.clone(),
            CapturedSnapshot::new(Snapshot::empty(scope), 0),
            0,
        )
    }

    /// A room rebuilt from durable storage.
    ///
    /// Indistinguishable from a fresh one afterwards: the snapshot already
    /// holds everything the log described, so nothing downstream needs to know
    /// whether this board is minutes or months old.
    pub fn restored(_scope: ScopeId, captured: CapturedSnapshot, snapshot_sequence: u64) -> Self {
        let (sender, _) = broadcast::channel(1024);
        let durable_sequence = captured.through_sequence();
        let state = captured.into_snapshot();
        let payload_bytes = state.document().estimated_payload_bytes();
        Self {
            state,
            sender,
            accepted: 0,
            durable_sequence,
            last_snapshotted_sequence: snapshot_sequence,
            snapshot_failures: 0,
            snapshot_retry_at: None,
            snapshot_capture_micros: 0,
            snapshot_encode_micros: 0,
            snapshot_write_micros: 0,
            snapshot_truncated_operations: 0,
            payload_bytes,
            last_active: Instant::now(),
            restore_latency: LatencyMetric::default(),
            validation_latency: LatencyMetric::default(),
            append_latency: LatencyMetric::default(),
            apply_latency: LatencyMetric::default(),
            ack_latency: LatencyMetric::default(),
            broadcast_latency: LatencyMetric::default(),
        }
    }

    pub fn record_restore(&self, duration: Duration) {
        self.restore_latency.observe(duration);
    }

    pub fn record_validation(&self, duration: Duration) {
        self.validation_latency.observe(duration);
    }

    pub fn record_append(&self, duration: Duration) {
        self.append_latency.observe(duration);
    }

    pub fn record_apply(&self, duration: Duration) {
        self.apply_latency.observe(duration);
    }

    pub fn record_ack(&self, duration: Duration) {
        self.ack_latency.observe(duration);
    }

    pub fn record_broadcast(&self, duration: Duration) {
        self.broadcast_latency.observe(duration);
    }

    /// Whether enough has accrued to be worth writing the scene back.
    pub fn snapshot_due(&self) -> bool {
        self.snapshot_due_at(Instant::now())
    }

    pub fn snapshot_due_at(&self, now: Instant) -> bool {
        self.durable_sequence
            .saturating_sub(self.last_snapshotted_sequence)
            >= SNAPSHOT_EVERY
            && self
                .snapshot_retry_at
                .is_none_or(|retry_at| now >= retry_at)
    }

    /// The materialised scene, for writing back to durable storage.
    pub fn capture_snapshot(&self) -> CapturedSnapshot {
        CapturedSnapshot::new(self.state.clone(), self.durable_sequence)
    }

    /// Records that the current scene has been persisted.
    pub fn mark_snapshotted(
        &mut self,
        through_sequence: u64,
        capture_micros: u64,
        encode_micros: u64,
        write_micros: u64,
        truncated_operations: u64,
    ) {
        self.last_snapshotted_sequence = self
            .last_snapshotted_sequence
            .max(through_sequence.min(self.durable_sequence));
        self.snapshot_failures = 0;
        self.snapshot_retry_at = None;
        self.snapshot_capture_micros = capture_micros;
        self.snapshot_encode_micros = encode_micros;
        self.snapshot_write_micros = write_micros;
        self.snapshot_truncated_operations = truncated_operations;
    }

    pub fn mark_snapshot_failed(&mut self, now: Instant, capture_micros: u64) {
        self.snapshot_failures = self.snapshot_failures.saturating_add(1);
        let shift = self.snapshot_failures.saturating_sub(1).min(6);
        let delay = SNAPSHOT_RETRY_BASE
            .checked_mul(1_u32 << shift)
            .unwrap_or(SNAPSHOT_RETRY_MAX)
            .min(SNAPSHOT_RETRY_MAX);
        self.snapshot_retry_at = now.checked_add(delay);
        self.snapshot_capture_micros = capture_micros;
    }

    pub fn subscribe(&mut self) -> broadcast::Receiver<Fanout> {
        self.last_active = Instant::now();
        self.sender.subscribe()
    }

    pub fn touch(&mut self) {
        self.last_active = Instant::now();
    }

    /// The document a joining client starts from. No clone, no fold.
    pub fn document(&self) -> &Document {
        self.state.document()
    }

    /// Validate a whole batch without mutating room state.
    ///
    /// # Errors
    ///
    /// A refusal is atomic: counters, activity, snapshot scheduling and the
    /// materialised document remain unchanged. Keeping this separate from
    /// [`Room::apply`] lets the caller persist before making the edit visible.
    pub fn prepare<'a>(&self, ops: &'a [StampedOp]) -> Result<PreparedBatch<'a>, Refused> {
        self.prepare_with_limits(
            ops,
            limits::MAX_ELEMENTS_PER_ROOM,
            limits::MAX_ROOM_PAYLOAD_BYTES,
        )
    }

    /// Validate a protocol-v2 batch and bind newly materialised element IDs to
    /// the authenticated replica actor. Existing elements retain their creator
    /// prefix and remain editable by collaborators.
    pub fn prepare_for_actor<'a>(
        &self,
        ops: &'a [StampedOp],
        actor: ActorId,
    ) -> Result<PreparedBatch<'a>, Refused> {
        if ops.iter().any(|stamped| {
            let element = stamped.element();
            self.state.document().get(element).is_none() && element.actor() != actor
        }) {
            return Err(Refused::ActorMismatch);
        }
        self.prepare(ops)
    }

    fn prepare_with_limits<'a>(
        &self,
        ops: &'a [StampedOp],
        element_limit: usize,
        payload_limit: usize,
    ) -> Result<PreparedBatch<'a>, Refused> {
        if ops.len() > limits::MAX_OPS_PER_FRAME {
            return Err(Refused::BatchTooLarge);
        }

        if ops.iter().any(|stamped| !stamped.is_valid()) {
            return Err(Refused::InvalidOperation);
        }

        // Deletes of unknown ids create tombstones too, so every operation can
        // materialise an element. Count each unknown id once across the batch.
        let new_elements = ops
            .iter()
            .map(StampedOp::element)
            .filter(|id| self.state.document().get(*id).is_none())
            .collect::<BTreeSet<_>>()
            .len();
        let projected_total = self
            .state
            .document()
            .total_count()
            .saturating_add(new_elements);
        if projected_total > element_limit {
            return Err(Refused::RoomFull);
        }

        let incoming_bytes = ops.iter().fold(0_usize, |bytes, operation| {
            bytes.saturating_add(operation.estimated_bytes())
        });
        if self.payload_bytes.saturating_add(incoming_bytes) > payload_limit {
            return Err(Refused::RoomFull);
        }

        Ok(PreparedBatch { operations: ops })
    }

    /// Apply a previously validated batch.
    ///
    /// The room must not change between [`Room::prepare`] and this call. The
    /// room cell's single command loop provides that ownership.
    pub fn apply(&mut self, prepared: PreparedBatch<'_>) -> usize {
        let ops = prepared.operations;
        self.last_active = Instant::now();
        self.accepted += ops.len() as u64;
        let changed = self.state.absorb(ops);
        self.payload_bytes = self.state.document().estimated_payload_bytes();
        changed
    }

    pub fn apply_persisted(&mut self, prepared: PreparedBatch<'_>, sequence: u64) -> usize {
        let changed = self.apply(prepared);
        self.durable_sequence = self.durable_sequence.max(sequence);
        changed
    }

    /// Persist a prepared batch before applying it. The callback is the only
    /// path to mutation, making append-failure atomicity directly testable.
    #[cfg(test)]
    pub fn commit<E>(
        &mut self,
        prepared: PreparedBatch<'_>,
        persist: impl FnOnce(&[StampedOp]) -> Result<u64, E>,
    ) -> Result<(usize, u64), E> {
        let sequence = persist(prepared.operations())?;
        let changed = self.apply(prepared);
        self.durable_sequence = self.durable_sequence.max(sequence);
        Ok((changed, sequence))
    }

    pub fn broadcast(&self, origin: u64, v1: String, v2: String) {
        // An error means nobody is listening, which is not a failure.
        let _ = self.sender.send(Fanout { origin, v1, v2 });
    }

    /// Whether this room can be reclaimed: nobody connected, and quiet for
    /// longer than the retention window.
    #[cfg(test)]
    pub fn is_reclaimable(&self, now: Instant) -> bool {
        self.sender.receiver_count() == 0
            && now.duration_since(self.last_active) > limits::ROOM_IDLE_TTL
    }

    pub fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_active)
    }

    pub fn stats(&self) -> RoomStats {
        RoomStats {
            elements: self.state.document().live_count(),
            tombstones: self.state.document().total_count() - self.state.document().live_count(),
            accepted: self.accepted,
            subscribers: self.sender.receiver_count(),
            snapshot_failures: self.snapshot_failures,
            snapshot_capture_micros: self.snapshot_capture_micros,
            snapshot_encode_micros: self.snapshot_encode_micros,
            snapshot_write_micros: self.snapshot_write_micros,
            snapshot_truncated_operations: self.snapshot_truncated_operations,
            payload_bytes: self.payload_bytes,
            restore_latency: self.restore_latency.snapshot(),
            validation_latency: self.validation_latency.snapshot(),
            append_latency: self.append_latency.snapshot(),
            apply_latency: self.apply_latency.snapshot(),
            ack_latency: self.ack_latency.snapshot(),
            broadcast_latency: self.broadcast_latency.snapshot(),
            mailbox_depth: 0,
            mailbox_max_depth: 0,
            mailbox_overloads: 0,
            mailbox_wait: LatencySnapshot::default(),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RoomStats {
    pub elements: usize,
    pub tombstones: usize,
    pub accepted: u64,
    pub subscribers: usize,
    pub snapshot_failures: u32,
    pub snapshot_capture_micros: u64,
    pub snapshot_encode_micros: u64,
    pub snapshot_write_micros: u64,
    pub snapshot_truncated_operations: u64,
    pub payload_bytes: usize,
    pub restore_latency: LatencySnapshot,
    pub validation_latency: LatencySnapshot,
    pub append_latency: LatencySnapshot,
    pub apply_latency: LatencySnapshot,
    pub ack_latency: LatencySnapshot,
    pub broadcast_latency: LatencySnapshot,
    pub mailbox_depth: u64,
    pub mailbox_max_depth: u64,
    pub mailbox_overloads: u64,
    pub mailbox_wait: LatencySnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_core::clock::{ActorId, HlcGenerator};
    use kboard_core::element::ElementId;
    use kboard_core::op::{upsert, Op};
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
        let batch = ops(1, 5);
        let prepared = room.prepare(&batch).unwrap();
        room.apply(prepared);
        assert_eq!(room.document().live_count(), 5);
    }

    #[test]
    fn replayed_operations_change_nothing() {
        let mut room = room();
        let batch = ops(1, 4);
        let prepared = room.prepare(&batch).unwrap();
        assert_eq!(room.apply(prepared), 4);
        let prepared = room.prepare(&batch).unwrap();
        assert_eq!(
            room.apply(prepared),
            0,
            "a retried batch is absorbed idempotently"
        );
        assert_eq!(room.document().live_count(), 4);
    }

    #[test]
    fn an_oversized_batch_is_refused_whole() {
        let room = room();
        let huge = ops(1, (limits::MAX_OPS_PER_FRAME + 1) as u128);
        assert_eq!(room.prepare(&huge).err(), Some(Refused::BatchTooLarge));
        assert_eq!(
            room.document().live_count(),
            0,
            "a refused batch must not partially apply"
        );
    }

    #[test]
    fn projected_element_limit_cannot_be_overshot_by_one_batch() {
        let mut room = room();
        let existing = ops(1, 2);
        let prepared = room.prepare(&existing).unwrap();
        room.apply(prepared);

        let overflow = ops(2, 2)
            .into_iter()
            .map(|mut stamped| {
                let shifted = ElementId(stamped.element().0 + 100);
                stamped.op = match stamped.op {
                    Op::Set { key, value, .. } => Op::Set {
                        element: shifted,
                        key,
                        value,
                    },
                    Op::Delete { .. } => unreachable!(),
                };
                stamped
            })
            .collect::<Vec<_>>();

        assert_eq!(
            room.prepare_with_limits(&overflow, 3, limits::MAX_ROOM_PAYLOAD_BYTES)
                .err(),
            Some(Refused::RoomFull)
        );
        assert_eq!(room.document().total_count(), 2);
        assert_eq!(room.stats().accepted, existing.len() as u64);
    }

    #[test]
    fn an_invalid_value_refuses_the_whole_batch_without_side_effects() {
        let mut invalid = ops(1, 2);
        invalid.push(StampedOp::new(
            invalid.last().unwrap().stamp,
            Op::Set {
                element: ElementId(99),
                key: PropKey::Text,
                value: PropValue::Text("x".repeat(kboard_core::prop::MAX_TEXT_BYTES + 1)),
            },
        ));
        let room = room();

        assert_eq!(
            room.prepare(&invalid).err(),
            Some(Refused::InvalidOperation)
        );
        assert_eq!(room.document().total_count(), 0);
        assert_eq!(room.stats().accepted, 0);
        assert!(!room.snapshot_due());
    }

    #[test]
    fn aggregate_payload_budget_refuses_before_mutation() {
        let mut clock = HlcGenerator::new(ActorId(1));
        let operation = upsert(
            ElementId(1),
            [(PropKey::Text, PropValue::Text("bounded".repeat(32)))],
            &mut clock,
            1,
        );
        let room = room();

        assert_eq!(
            room.prepare_with_limits(&operation, limits::MAX_ELEMENTS_PER_ROOM, 64)
                .err(),
            Some(Refused::RoomFull)
        );
        assert_eq!(room.stats().payload_bytes, 0);
    }

    #[test]
    fn new_element_ids_are_actor_bound_but_existing_elements_remain_collaborative() {
        let owner = ActorId(7);
        let collaborator = ActorId(8);
        let element = ElementId(((owner.0 as u128) << 64) | 1);
        let mut owner_clock = HlcGenerator::new(owner);
        let created = upsert(
            element,
            [(PropKey::X, PropValue::Num(1.0))],
            &mut owner_clock,
            1_000,
        );
        let mut room = room();
        let prepared = room.prepare_for_actor(&created, owner).unwrap();
        room.apply(prepared);

        let mut collaborator_clock = HlcGenerator::new(collaborator);
        let edit = upsert(
            element,
            [(PropKey::X, PropValue::Num(2.0))],
            &mut collaborator_clock,
            2_000,
        );
        assert!(room.prepare_for_actor(&edit, collaborator).is_ok());

        let forged_new = upsert(
            ElementId(((owner.0 as u128) << 64) | 2),
            [(PropKey::X, PropValue::Num(3.0))],
            &mut collaborator_clock,
            3_000,
        );
        assert_eq!(
            room.prepare_for_actor(&forged_new, collaborator).err(),
            Some(Refused::ActorMismatch)
        );
    }

    #[test]
    fn preparing_a_valid_batch_does_not_mutate_until_apply() {
        let mut room = room();
        let batch = ops(1, 2);
        let prepared = room.prepare(&batch).unwrap();

        assert_eq!(room.document().total_count(), 0);
        assert_eq!(room.stats().accepted, 0);

        room.apply(prepared);
        assert_eq!(room.document().total_count(), 2);
        assert_eq!(room.stats().accepted, 2);
    }

    #[test]
    fn append_failure_cannot_mutate_or_advance_a_room() {
        let operations = ops(1, 1);
        let mut room = room();
        let before = room.document().clone();
        let prepared = room.prepare(&operations).unwrap();

        let result = room.commit(prepared, |_ops| Err::<u64, _>("disk full"));

        assert_eq!(result, Err("disk full"));
        assert_eq!(room.document(), &before);
        assert_eq!(room.stats().accepted, 0);
        assert_eq!(room.capture_snapshot().through_sequence(), 0);
    }

    #[test]
    fn snapshot_capture_keeps_exact_sequence_and_failure_backoff() {
        let operations = ops(1, SNAPSHOT_EVERY as u128);
        let mut room = room();
        let prepared = room.prepare(&operations).unwrap();
        room.commit(prepared, |_| Ok::<u64, ()>(SNAPSHOT_EVERY))
            .unwrap();

        let captured = room.capture_snapshot();
        assert_eq!(captured.through_sequence(), SNAPSHOT_EVERY);
        let failed_at = Instant::now();
        room.mark_snapshot_failed(failed_at, 11);
        assert!(!room.snapshot_due_at(failed_at));
        assert!(room.snapshot_due_at(failed_at + SNAPSHOT_RETRY_BASE));

        room.mark_snapshotted(SNAPSHOT_EVERY, 11, 12, 13, SNAPSHOT_EVERY);
        assert!(!room.snapshot_due());
        let stats = room.stats();
        assert_eq!(stats.snapshot_failures, 0);
        assert_eq!(stats.snapshot_capture_micros, 11);
        assert_eq!(stats.snapshot_encode_micros, 12);
        assert_eq!(stats.snapshot_write_micros, 13);
    }

    #[test]
    fn generated_batch_shapes_agree_with_the_validation_model() {
        for mask in 0_u64..64 {
            let mut clock = HlcGenerator::new(ActorId(1));
            let batch = (0..6)
                .map(|index| {
                    let valid = mask & (1 << index) != 0;
                    StampedOp::new(
                        clock.tick(1_000 + index),
                        Op::Set {
                            element: ElementId(u128::from(index % 3)),
                            key: PropKey::X,
                            value: if valid {
                                PropValue::Num(index as f64)
                            } else {
                                PropValue::Text(index.to_string())
                            },
                        },
                    )
                })
                .collect::<Vec<_>>();
            let expected_valid = batch.iter().all(StampedOp::is_valid);
            let room = room();
            let outcome = room.prepare_with_limits(&batch, 2, limits::MAX_ROOM_PAYLOAD_BYTES);

            if !expected_valid {
                assert_eq!(outcome.err(), Some(Refused::InvalidOperation));
                assert_eq!(room.document().total_count(), 0);
            } else {
                assert_eq!(outcome.err(), Some(Refused::RoomFull));
                assert_eq!(room.document().total_count(), 0);
            }
        }

        let mut room = room();
        let within_limit = ops(1, 2);
        let prepared = room
            .prepare_with_limits(&within_limit, 2, limits::MAX_ROOM_PAYLOAD_BYTES)
            .expect("the model permits two distinct valid ids");
        room.apply(prepared);
        assert_eq!(room.document().total_count(), 2);
    }

    #[test]
    fn stats_report_tombstones_separately() {
        let mut room = room();
        let created = ops(1, 3);
        let prepared = room.prepare(&created).unwrap();
        room.apply(prepared);
        let mut clock = HlcGenerator::new(ActorId(9));
        let stamp = clock.tick(9_000);
        let deleted = [StampedOp::new(
            stamp,
            kboard_core::op::Op::Delete {
                element: ElementId(0),
            },
        )];
        let prepared = room.prepare(&deleted).unwrap();
        room.apply(prepared);

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

    #[tokio::test]
    async fn a_lagging_subscriber_is_bounded_and_forced_to_resync() {
        let mut room = room();
        let mut subscriber = room.subscribe();
        for index in 0..1_100 {
            room.broadcast(index, "v1".to_owned(), "v2".to_owned());
        }

        assert!(matches!(
            subscriber.recv().await,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
        ));
    }
}
