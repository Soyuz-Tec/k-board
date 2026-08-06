//! One ordered, bounded command processor per active scope.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use kboard_core::clock::ActorId;
use kboard_core::document::ScopeId;
use kboard_core::op::StampedOp;
use kboard_store::BatchAppend;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Semaphore};

use crate::limits;
use crate::protocol::{
    V1ServerMessage, V2ServerMessage, Version, DEDUPE_RETENTION_MILLIS, VERSION_2,
};
use crate::room::{Fanout, Refused, Room, RoomStats};
use crate::storage_writer::{StorageError, StorageWriter};
use crate::telemetry::{correlation_fields, LatencyMetric};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Restoring,
    Ready,
    Draining,
    Failed,
    Stopped,
}

#[derive(Clone, Debug)]
pub struct CellStatus {
    pub lifecycle: LifecycleState,
    pub stats: Option<RoomStats>,
    pub idle_for: Duration,
    pub changed_at: Instant,
}

impl CellStatus {
    fn at(lifecycle: LifecycleState, room: Option<&Room>) -> Self {
        let now = Instant::now();
        Self {
            lifecycle,
            stats: room.map(Room::stats),
            idle_for: room.map_or(Duration::ZERO, |room| room.idle_for(now)),
            changed_at: now,
        }
    }
}

pub struct JoinReply {
    pub init: String,
    pub updates: broadcast::Receiver<Fanout>,
}

pub struct CommitRequest {
    pub origin: u64,
    pub version: Version,
    pub actor: ActorId,
    pub replica: Option<String>,
    pub batch: Option<String>,
    pub operations: Vec<StampedOp>,
    pub payload_hash: [u8; 32],
    pub recorded_at_millis: u64,
    pub v1_payload: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    Committed(u64),
    Duplicate(u64),
}

#[derive(Debug, PartialEq, Eq)]
pub enum CellError {
    Overloaded,
    Deadline,
    Unavailable,
    Draining,
    ResourceLimit,
    Refused(Refused),
}

enum RoomCommand {
    Join {
        version: Version,
        actor: ActorId,
        replica: Option<String>,
        reply: oneshot::Sender<Result<JoinReply, CellError>>,
    },
    Commit {
        request: CommitRequest,
        direct: mpsc::Sender<String>,
        reply: oneshot::Sender<Result<CommitOutcome, CellError>>,
    },
    Presence {
        event: Fanout,
    },
    Leave {
        event: Fanout,
    },
    Stats {
        reply: oneshot::Sender<Result<RoomStats, CellError>>,
    },
    Drain {
        reply: oneshot::Sender<Result<(), CellError>>,
    },
    #[cfg(test)]
    Pause {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    #[cfg(test)]
    Panic,
}

struct QueuedRoomCommand {
    enqueued_at: Instant,
    command: RoomCommand,
}

#[derive(Default)]
struct CellQueueMetrics {
    depth: AtomicU64,
    max_depth: AtomicU64,
    overloads: AtomicU64,
    wait: LatencyMetric,
}

impl CellQueueMetrics {
    fn add_to(&self, stats: &mut RoomStats) {
        stats.mailbox_depth = self.depth.load(Ordering::Relaxed);
        stats.mailbox_max_depth = self.max_depth.load(Ordering::Relaxed);
        stats.mailbox_overloads = self.overloads.load(Ordering::Relaxed);
        stats.mailbox_wait = self.wait.snapshot();
    }
}

#[derive(Clone)]
pub struct RoomCellHandle {
    id: u64,
    sender: mpsc::Sender<QueuedRoomCommand>,
    status: watch::Receiver<CellStatus>,
    finished: Arc<AtomicBool>,
    metrics: Arc<CellQueueMetrics>,
}

impl RoomCellHandle {
    pub fn spawn(
        id: u64,
        scope: ScopeId,
        storage: StorageWriter,
        restores: Arc<Semaphore>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel::<QueuedRoomCommand>(limits::CELL_MAILBOX_CAPACITY);
        let (status_sender, status) =
            watch::channel(CellStatus::at(LifecycleState::Restoring, None));
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = finished.clone();
        let metrics = Arc::new(CellQueueMetrics::default());
        let task_metrics = metrics.clone();
        tokio::spawn(async move {
            let task = run_cell(
                scope,
                storage,
                restores,
                receiver,
                status_sender.clone(),
                task_metrics,
            );
            match AssertUnwindSafe(task).catch_unwind().await {
                Ok(Ok(())) => {}
                Ok(Err(())) | Err(_) => {
                    status_sender.send_replace(CellStatus::at(LifecycleState::Failed, None));
                }
            }
            task_finished.store(true, Ordering::Release);
        });
        Self {
            id,
            sender,
            status,
            finished,
            metrics,
        }
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn status(&self) -> CellStatus {
        self.status.borrow().clone()
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub async fn join(
        &self,
        version: Version,
        actor: ActorId,
        replica: Option<String>,
    ) -> Result<JoinReply, CellError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(RoomCommand::Join {
            version,
            actor,
            replica,
            reply,
        })?;
        receive(response).await
    }

    pub async fn commit(
        &self,
        request: CommitRequest,
        direct: mpsc::Sender<String>,
    ) -> Result<CommitOutcome, CellError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(RoomCommand::Commit {
            request,
            direct,
            reply,
        })?;
        receive(response).await
    }

    pub fn presence(&self, event: Fanout) -> Result<(), CellError> {
        self.enqueue(RoomCommand::Presence { event })
    }

    pub fn leave(&self, event: Fanout) {
        let _ = self.enqueue(RoomCommand::Leave { event });
    }

    pub async fn stats(&self) -> Result<RoomStats, CellError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(RoomCommand::Stats { reply })?;
        receive(response).await
    }

    pub async fn drain(&self) -> Result<(), CellError> {
        let (reply, response) = oneshot::channel();
        self.enqueue(RoomCommand::Drain { reply })?;
        receive(response).await
    }

    fn enqueue(&self, command: RoomCommand) -> Result<(), CellError> {
        let depth = self.metrics.depth.fetch_add(1, Ordering::Relaxed) + 1;
        let queued = QueuedRoomCommand {
            enqueued_at: Instant::now(),
            command,
        };
        match self.sender.try_send(queued) {
            Ok(()) => {
                self.metrics.max_depth.fetch_max(depth, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.depth.fetch_sub(1, Ordering::Relaxed);
                self.metrics.overloads.fetch_add(1, Ordering::Relaxed);
                Err(CellError::Overloaded)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.depth.fetch_sub(1, Ordering::Relaxed);
                match self.status().lifecycle {
                    LifecycleState::Draining | LifecycleState::Stopped => Err(CellError::Draining),
                    _ => Err(CellError::Unavailable),
                }
            }
        }
    }

    #[cfg(test)]
    async fn pause(&self) -> oneshot::Sender<()> {
        let (entered, entered_response) = oneshot::channel();
        let (release, release_response) = oneshot::channel();
        self.enqueue(RoomCommand::Pause {
            entered,
            release: release_response,
        })
        .unwrap();
        entered_response.await.unwrap();
        release
    }

    #[cfg(test)]
    fn panic_for_test(&self) {
        self.enqueue(RoomCommand::Panic).unwrap();
    }

    #[cfg(test)]
    pub fn stopped_for_test(id: u64) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let (_status_sender, status) =
            watch::channel(CellStatus::at(LifecycleState::Stopped, None));
        Self {
            id,
            sender,
            status,
            finished: Arc::new(AtomicBool::new(true)),
            metrics: Arc::new(CellQueueMetrics::default()),
        }
    }
}

async fn receive<T>(response: oneshot::Receiver<Result<T, CellError>>) -> Result<T, CellError> {
    tokio::time::timeout(limits::COMMAND_DEADLINE, response)
        .await
        .map_err(|_| CellError::Deadline)?
        .map_err(|_| CellError::Unavailable)?
}

async fn run_cell(
    scope: ScopeId,
    storage: StorageWriter,
    restores: Arc<Semaphore>,
    mut receiver: mpsc::Receiver<QueuedRoomCommand>,
    status: watch::Sender<CellStatus>,
    queue_metrics: Arc<CellQueueMetrics>,
) -> Result<(), ()> {
    let restore_started = Instant::now();
    let permit = restores.acquire_owned().await.map_err(|_| ())?;
    let restored = storage.restore(scope.clone()).await.map_err(|_| ())?;
    drop(permit);
    if restored.document().estimated_payload_bytes() > limits::MAX_ROOM_PAYLOAD_BYTES {
        return Err(());
    }
    let mut room = Room::restored(scope.clone(), restored.captured, restored.snapshot_sequence);
    room.record_restore(restore_started.elapsed());
    status.send_replace(CellStatus::at(LifecycleState::Ready, Some(&room)));
    let mut publish = tokio::time::interval(limits::SWEEP_INTERVAL);
    publish.tick().await;

    loop {
        tokio::select! {
            biased;
            queued = receiver.recv() => {
                let Some(queued) = queued else {
                    status.send_replace(CellStatus::at(LifecycleState::Stopped, Some(&room)));
                    return Ok(());
                };
                queue_metrics.depth.fetch_sub(1, Ordering::Relaxed);
                queue_metrics.wait.observe_since(queued.enqueued_at);
                if handle_command(queued.command, &scope, &storage, &mut room, &status, &queue_metrics).await? {
                    receiver.close();
                    while let Some(queued) = receiver.recv().await {
                        queue_metrics.depth.fetch_sub(1, Ordering::Relaxed);
                        queue_metrics.wait.observe_since(queued.enqueued_at);
                        reject_during_drain(queued.command);
                    }
                    status.send_replace(CellStatus::at(LifecycleState::Stopped, Some(&room)));
                    return Ok(());
                }
            }
            _ = publish.tick() => {
                status.send_replace(CellStatus::at(LifecycleState::Ready, Some(&room)));
            }
        }
    }
}

async fn handle_command(
    command: RoomCommand,
    scope: &ScopeId,
    storage: &StorageWriter,
    room: &mut Room,
    status: &watch::Sender<CellStatus>,
    queue_metrics: &CellQueueMetrics,
) -> Result<bool, ()> {
    match command {
        RoomCommand::Join {
            version,
            actor,
            replica,
            reply,
        } => {
            let init = match version {
                Version::V1 => serde_json::to_string(&V1ServerMessage::Init {
                    actor: actor.0,
                    doc: room.document(),
                }),
                Version::V2 => serde_json::to_string(&V2ServerMessage::Init {
                    version: VERSION_2,
                    actor: actor.0,
                    replica: replica.as_deref().unwrap_or_default(),
                    doc: room.document(),
                }),
            };
            let result = init.map_err(|_| CellError::Unavailable).and_then(|init| {
                if init.len() > limits::MAX_SERIALIZED_DOCUMENT_BYTES {
                    Err(CellError::ResourceLimit)
                } else {
                    Ok(JoinReply {
                        init,
                        updates: room.subscribe(),
                    })
                }
            });
            let _ = reply.send(result);
        }
        RoomCommand::Commit {
            request,
            direct,
            reply,
        } => {
            let result = commit(scope, storage, room, &request, &direct).await;
            if let Err(error) = &result {
                let correlation = correlation_fields(
                    scope,
                    request.replica.as_deref(),
                    request.batch.as_deref(),
                    None,
                );
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "event": "room_commit_refused",
                        "correlation": correlation,
                        "outcome": format!("{error:?}"),
                    })
                );
            }
            let committed = matches!(result, Ok(CommitOutcome::Committed(_)));
            let _ = reply.send(result);
            if committed && room.snapshot_due() {
                persist_snapshot(storage, room).await;
            }
        }
        RoomCommand::Presence { event } | RoomCommand::Leave { event } => {
            room.touch();
            room.broadcast(event.origin, event.v1, event.v2);
        }
        RoomCommand::Stats { reply } => {
            let mut stats = room.stats();
            queue_metrics.add_to(&mut stats);
            let _ = reply.send(Ok(stats));
        }
        RoomCommand::Drain { reply } => {
            status.send_replace(CellStatus::at(LifecycleState::Draining, Some(room)));
            let _ = reply.send(Ok(()));
            return Ok(true);
        }
        #[cfg(test)]
        RoomCommand::Pause { entered, release } => {
            let _ = entered.send(());
            let _ = release.await;
        }
        #[cfg(test)]
        RoomCommand::Panic => panic!("injected room-cell panic"),
    }
    status.send_replace(CellStatus::at(LifecycleState::Ready, Some(room)));
    Ok(false)
}

async fn commit(
    scope: &ScopeId,
    storage: &StorageWriter,
    room: &mut Room,
    request: &CommitRequest,
    direct: &mpsc::Sender<String>,
) -> Result<CommitOutcome, CellError> {
    if request.version == Version::V2 {
        let replica = request.replica.as_ref().ok_or(CellError::Unavailable)?;
        let batch = request.batch.as_ref().ok_or(CellError::Unavailable)?;
        let retained_after = request
            .recorded_at_millis
            .saturating_sub(DEDUPE_RETENTION_MILLIS);
        let append_started = Instant::now();
        let stored_outcome = storage
            .batch_outcome(
                scope.clone(),
                replica.clone(),
                batch.clone(),
                request.payload_hash,
                retained_after,
            )
            .await;
        room.record_append(append_started.elapsed());
        match stored_outcome.map_err(storage_error)? {
            Some(BatchAppend::Duplicate(sequence)) => {
                let ack_started = Instant::now();
                send_ack(direct, batch, sequence);
                room.record_ack(ack_started.elapsed());
                return Ok(CommitOutcome::Duplicate(sequence));
            }
            Some(BatchAppend::Conflict) => {
                return Err(CellError::Refused(Refused::BatchConflict));
            }
            Some(BatchAppend::Committed(_)) => unreachable!(),
            None => {}
        }
    }

    let validation_started = Instant::now();
    let prepared_result = match request.version {
        Version::V1 => room.prepare(&request.operations),
        Version::V2 => room.prepare_for_actor(&request.operations, request.actor),
    };
    room.record_validation(validation_started.elapsed());
    let prepared = prepared_result.map_err(CellError::Refused)?;

    let append_started = Instant::now();
    let sequence = match request.version {
        Version::V1 => storage
            .append(scope.clone(), request.operations.clone())
            .await
            .map_err(storage_error)?,
        Version::V2 => {
            let replica = request.replica.as_ref().ok_or(CellError::Unavailable)?;
            let batch = request.batch.as_ref().ok_or(CellError::Unavailable)?;
            let retained_after = request
                .recorded_at_millis
                .saturating_sub(DEDUPE_RETENTION_MILLIS);
            match storage
                .append_batch(
                    scope.clone(),
                    replica.clone(),
                    batch.clone(),
                    request.payload_hash,
                    request.operations.clone(),
                    request.recorded_at_millis,
                    retained_after,
                )
                .await
                .map_err(storage_error)?
            {
                BatchAppend::Committed(sequence) => sequence,
                BatchAppend::Duplicate(sequence) => {
                    room.record_append(append_started.elapsed());
                    let ack_started = Instant::now();
                    send_ack(direct, batch, sequence);
                    room.record_ack(ack_started.elapsed());
                    return Ok(CommitOutcome::Duplicate(sequence));
                }
                BatchAppend::Conflict => {
                    return Err(CellError::Refused(Refused::BatchConflict));
                }
            }
        }
    };
    room.record_append(append_started.elapsed());

    let apply_started = Instant::now();
    room.apply_persisted(prepared, sequence);
    room.record_apply(apply_started.elapsed());
    let v2 = serde_json::to_string(&V2ServerMessage::Ops {
        version: VERSION_2,
        sequence,
        ops: &request.operations,
    })
    .map_err(|_| CellError::Unavailable)?;
    if let Some(batch) = request.batch.as_deref() {
        let ack_started = Instant::now();
        send_ack(direct, batch, sequence);
        room.record_ack(ack_started.elapsed());
    }
    let broadcast_started = Instant::now();
    room.broadcast(request.origin, request.v1_payload.clone(), v2);
    room.record_broadcast(broadcast_started.elapsed());
    Ok(CommitOutcome::Committed(sequence))
}

fn send_ack(direct: &mpsc::Sender<String>, batch: &str, sequence: u64) {
    if let Ok(payload) = serde_json::to_string(&V2ServerMessage::Ack { batch, sequence }) {
        let _ = direct.try_send(payload);
    }
}

fn storage_error(error: StorageError) -> CellError {
    match error {
        StorageError::Overloaded => CellError::Overloaded,
        StorageError::Unavailable | StorageError::Deadline => CellError::Unavailable,
    }
}

async fn persist_snapshot(storage: &StorageWriter, room: &mut Room) {
    let capture_started = Instant::now();
    let captured = room.capture_snapshot();
    let capture_micros = capture_started
        .elapsed()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX);
    match storage.snapshot(captured.clone()).await {
        Ok(committed) => room.mark_snapshotted(
            captured.through_sequence(),
            capture_micros,
            committed.encode_micros,
            committed.write_micros,
            committed.truncated_operations,
        ),
        Err(_) => room.mark_snapshot_failed(Instant::now(), capture_micros),
    }
}

fn reject_during_drain(command: RoomCommand) {
    match command {
        RoomCommand::Join { reply, .. } => {
            let _ = reply.send(Err(CellError::Draining));
        }
        RoomCommand::Commit { reply, .. } => {
            let _ = reply.send(Err(CellError::Draining));
        }
        RoomCommand::Stats { reply } => {
            let _ = reply.send(Err(CellError::Draining));
        }
        RoomCommand::Drain { reply } => {
            let _ = reply.send(Ok(()));
        }
        RoomCommand::Presence { .. } | RoomCommand::Leave { .. } => {}
        #[cfg(test)]
        RoomCommand::Pause { entered, .. } => {
            let _ = entered.send(());
        }
        #[cfg(test)]
        RoomCommand::Panic => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_core::clock::HlcGenerator;
    use kboard_core::element::ElementId;
    use kboard_core::op::upsert;
    use kboard_core::prop::{PropKey, PropValue};
    use kboard_store::SqliteStore;

    fn spawn(id: u64, scope: &str) -> RoomCellHandle {
        RoomCellHandle::spawn(
            id,
            ScopeId::new(scope),
            StorageWriter::start(SqliteStore::in_memory().unwrap()),
            Arc::new(Semaphore::new(1)),
        )
    }

    async fn ready(cell: &RoomCellHandle) {
        for _ in 0..1_000 {
            if cell.status().lifecycle == LifecycleState::Ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("cell did not become ready");
    }

    fn operation(actor: u64, element: u128, at: u64) -> Vec<StampedOp> {
        let mut clock = HlcGenerator::new(ActorId(actor));
        upsert(
            ElementId(element),
            [(PropKey::X, PropValue::Num(element as f64))],
            &mut clock,
            at,
        )
    }

    fn request(operations: Vec<StampedOp>, origin: u64) -> CommitRequest {
        CommitRequest {
            origin,
            version: Version::V1,
            actor: ActorId(origin),
            replica: None,
            batch: None,
            payload_hash: [0; 32],
            recorded_at_millis: origin,
            v1_payload: serde_json::to_string(&V1ServerMessage::Ops { ops: &operations }).unwrap(),
            operations,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn same_scope_commits_follow_mailbox_order() {
        let cell = spawn(1, "t:ordered");
        ready(&cell).await;
        let (direct, _) = mpsc::channel(4);
        let first = cell
            .commit(request(operation(1, 1, 1), 1), direct.clone())
            .await
            .unwrap();
        let second = cell
            .commit(request(operation(1, 2, 2), 1), direct)
            .await
            .unwrap();

        assert_eq!(first, CommitOutcome::Committed(1));
        assert_eq!(second, CommitOutcome::Committed(2));
        let stats = cell.stats().await.unwrap();
        assert_eq!(stats.accepted, 2);
        assert_eq!(stats.validation_latency.count, 2);
        assert_eq!(stats.append_latency.count, 2);
        assert_eq!(stats.apply_latency.count, 2);
        assert_eq!(stats.broadcast_latency.count, 2);
        assert!(stats.mailbox_wait.count >= 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_identity_and_sequences_are_isolated_by_scope() {
        let writer = StorageWriter::start(SqliteStore::in_memory().unwrap());
        let restores = Arc::new(Semaphore::new(2));
        let left =
            RoomCellHandle::spawn(1, ScopeId::new("t:left"), writer.clone(), restores.clone());
        let right = RoomCellHandle::spawn(2, ScopeId::new("t:right"), writer, restores);
        ready(&left).await;
        ready(&right).await;
        let actor = ActorId(7);
        let operations = operation(actor.0, (u128::from(actor.0) << 64) | 1, 1);
        let request = |origin| CommitRequest {
            origin,
            version: Version::V2,
            actor,
            replica: Some("00112233445566778899aabbccddeeff".to_owned()),
            batch: Some("ffeeddccbbaa99887766554433221100".to_owned()),
            operations: operations.clone(),
            payload_hash: [9; 32],
            recorded_at_millis: 1,
            v1_payload: serde_json::to_string(&V1ServerMessage::Ops { ops: &operations }).unwrap(),
        };
        let (direct, mut acks) = mpsc::channel(8);

        assert_eq!(
            left.commit(request(1), direct.clone()).await.unwrap(),
            CommitOutcome::Committed(1)
        );
        assert_eq!(
            right.commit(request(2), direct.clone()).await.unwrap(),
            CommitOutcome::Committed(1)
        );
        assert_eq!(
            left.commit(request(1), direct).await.unwrap(),
            CommitOutcome::Duplicate(1)
        );
        let left_stats = left.stats().await.unwrap();
        assert_eq!(left_stats.accepted, 1);
        assert!(left_stats.ack_latency.count >= 2);
        assert_eq!(right.stats().await.unwrap().accepted, 1);
        assert!(acks.recv().await.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hot_full_mailbox_does_not_block_a_cold_scope() {
        let hot = spawn(1, "t:hot");
        let cold = spawn(2, "t:cold");
        ready(&hot).await;
        ready(&cold).await;
        let release = hot.pause().await;
        for index in 0..limits::CELL_MAILBOX_CAPACITY {
            hot.presence(Fanout {
                origin: index as u64,
                v1: "v1".to_owned(),
                v2: "v2".to_owned(),
            })
            .unwrap();
        }
        assert_eq!(
            hot.presence(Fanout {
                origin: 999,
                v1: "v1".to_owned(),
                v2: "v2".to_owned(),
            }),
            Err(CellError::Overloaded)
        );

        let cold_stats = tokio::time::timeout(Duration::from_secs(1), cold.stats()).await;
        assert!(matches!(cold_stats, Ok(Ok(_))));
        let _ = release.send(());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_legitimate_sixty_four_command_burst_drains_within_the_deadline() {
        let cell = spawn(1, "t:burst");
        ready(&cell).await;
        let release = cell.pause().await;
        for index in 0..(limits::CELL_MAILBOX_CAPACITY - 1) {
            cell.presence(Fanout {
                origin: index as u64,
                v1: "v1".to_owned(),
                v2: "v2".to_owned(),
            })
            .unwrap();
        }
        let (reply, response) = oneshot::channel();
        cell.enqueue(RoomCommand::Stats { reply }).unwrap();

        let started = Instant::now();
        let _ = release.send(());
        assert!(matches!(
            tokio::time::timeout(limits::COMMAND_DEADLINE, response).await,
            Ok(Ok(Ok(_)))
        ));
        eprintln!("room_cell_burst_64_us={}", started.elapsed().as_micros());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_response_receivers_do_not_stop_the_cell() {
        let cell = spawn(1, "t:dropped");
        ready(&cell).await;
        let (reply, response) = oneshot::channel();
        drop(response);
        cell.enqueue(RoomCommand::Stats { reply }).unwrap();
        tokio::task::yield_now().await;
        assert!(cell.stats().await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drain_rejects_work_queued_after_the_handshake() {
        let cell = spawn(1, "t:drain");
        ready(&cell).await;
        let release = cell.pause().await;
        let draining = {
            let cell = cell.clone();
            tokio::spawn(async move { cell.drain().await })
        };
        tokio::task::yield_now().await;
        let queued_stats = {
            let cell = cell.clone();
            tokio::spawn(async move { cell.stats().await })
        };
        let _ = release.send(());

        assert_eq!(draining.await.unwrap(), Ok(()));
        assert!(matches!(
            queued_stats.await.unwrap(),
            Err(CellError::Draining)
        ));
        for _ in 0..1_000 {
            if cell.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(cell.is_finished());
        assert_eq!(cell.status().lifecycle, LifecycleState::Stopped);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drain_finishes_earlier_durable_work_and_releases_broadcast_state() {
        let cell = spawn(1, "t:durable-drain");
        ready(&cell).await;
        let mut updates = cell
            .join(Version::V1, ActorId(1), None)
            .await
            .unwrap()
            .updates;
        let release = cell.pause().await;
        let (direct, _) = mpsc::channel(4);

        let (commit_reply, commit_response) = oneshot::channel();
        cell.enqueue(RoomCommand::Commit {
            request: request(operation(1, 1, 1), 1),
            direct,
            reply: commit_reply,
        })
        .unwrap();
        let (drain_reply, drain_response) = oneshot::channel();
        cell.enqueue(RoomCommand::Drain { reply: drain_reply })
            .unwrap();
        let _ = release.send(());

        assert_eq!(
            commit_response.await.unwrap(),
            Ok(CommitOutcome::Committed(1))
        );
        assert_eq!(drain_response.await.unwrap(), Ok(()));
        loop {
            match updates.recv().await {
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    panic!("one update cannot lag the receiver")
                }
            }
        }
        for _ in 0..1_000 {
            if cell.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(cell.is_finished());
        assert_eq!(cell.status().stats.unwrap().accepted, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_cell_is_published_as_failed() {
        let cell = spawn(1, "t:panic");
        ready(&cell).await;
        cell.panic_for_test();
        for _ in 0..1_000 {
            if cell.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(cell.is_finished());
        assert_eq!(cell.status().lifecycle, LifecycleState::Failed);
    }
}
