//! One bounded owner for SQLite.
//!
//! Room cells are concurrent, SQLite writes are not. This task is the explicit
//! scheduling boundary between those facts: cells submit owned requests to one
//! bounded FIFO and await typed outcomes without sharing a connection or lock.

use kboard_core::document::ScopeId;
use kboard_core::op::StampedOp;
use kboard_core::snapshot::CapturedSnapshot;
use kboard_store::{
    BatchAppend, BatchWrite, CheckpointHealth, RestoredScope, SnapshotCommit, SqliteStore,
};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

use crate::limits;
use crate::telemetry::{LatencyMetric, LatencySnapshot};

#[derive(Clone)]
pub struct StorageWriter {
    sender: mpsc::Sender<QueuedStorageCommand>,
    metrics: Arc<StorageMetrics>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    Overloaded,
    Unavailable,
    Deadline,
}

#[derive(Clone, Copy, Debug)]
pub struct StorageHealth {
    pub schema_version: u32,
    pub checkpoint: CheckpointHealth,
    pub metrics: StorageMetricsSnapshot,
    pub readable: bool,
    pub writable: bool,
}

#[derive(Default)]
struct StorageMetrics {
    queue_depth: AtomicU64,
    max_queue_depth: AtomicU64,
    overloads: AtomicU64,
    queue_wait: LatencyMetric,
    sql: LatencyMetric,
}

impl StorageMetrics {
    fn snapshot(&self) -> StorageMetricsSnapshot {
        StorageMetricsSnapshot {
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            max_queue_depth: self.max_queue_depth.load(Ordering::Relaxed),
            overloads: self.overloads.load(Ordering::Relaxed),
            queue_wait: self.queue_wait.snapshot(),
            sql: self.sql.snapshot(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct StorageMetricsSnapshot {
    pub queue_depth: u64,
    pub max_queue_depth: u64,
    pub overloads: u64,
    pub queue_wait: LatencySnapshot,
    pub sql: LatencySnapshot,
}

struct QueuedStorageCommand {
    enqueued_at: Instant,
    lifecycle: Arc<AtomicU8>,
    command: StorageCommand,
}

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;

struct OwnedBatch {
    scope: ScopeId,
    replica: String,
    batch: String,
    payload_hash: [u8; 32],
    operations: Vec<StampedOp>,
    recorded_at_millis: u64,
    retain_after_millis: u64,
}

enum StorageCommand {
    Restore {
        scope: ScopeId,
        reply: oneshot::Sender<Result<RestoredScope, StorageError>>,
    },
    BatchOutcome {
        scope: ScopeId,
        replica: String,
        batch: String,
        payload_hash: [u8; 32],
        retain_after_millis: u64,
        reply: oneshot::Sender<Result<Option<BatchAppend>, StorageError>>,
    },
    Append {
        scope: ScopeId,
        operations: Vec<StampedOp>,
        reply: oneshot::Sender<Result<u64, StorageError>>,
    },
    AppendBatch {
        write: OwnedBatch,
        reply: oneshot::Sender<Result<BatchAppend, StorageError>>,
    },
    Snapshot {
        captured: CapturedSnapshot,
        reply: oneshot::Sender<Result<SnapshotCommit, StorageError>>,
    },
    Health {
        reply: oneshot::Sender<Result<StorageHealth, StorageError>>,
    },
    Flush {
        reply: oneshot::Sender<Result<CheckpointHealth, StorageError>>,
    },
    #[cfg(test)]
    Delay {
        duration: std::time::Duration,
        entered: oneshot::Sender<()>,
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
}

impl StorageWriter {
    pub fn start(mut store: SqliteStore) -> Self {
        let (sender, mut receiver) =
            mpsc::channel::<QueuedStorageCommand>(limits::STORAGE_MAILBOX_CAPACITY);
        let metrics = Arc::new(StorageMetrics::default());
        let task_metrics = metrics.clone();
        tokio::task::spawn_blocking(move || {
            while let Some(queued) = receiver.blocking_recv() {
                task_metrics.queue_depth.fetch_sub(1, Ordering::Relaxed);
                task_metrics.queue_wait.observe_since(queued.enqueued_at);
                if queued
                    .lifecycle
                    .compare_exchange(QUEUED, STARTED, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    continue;
                }
                let sql_started = Instant::now();
                let command = queued.command;
                match command {
                    StorageCommand::Restore { scope, reply } => {
                        let _ = reply
                            .send(store.restore(&scope).map_err(|_| StorageError::Unavailable));
                    }
                    StorageCommand::BatchOutcome {
                        scope,
                        replica,
                        batch,
                        payload_hash,
                        retain_after_millis,
                        reply,
                    } => {
                        let result = store
                            .batch_outcome(
                                &scope,
                                &replica,
                                &batch,
                                &payload_hash,
                                retain_after_millis,
                            )
                            .map_err(|_| StorageError::Unavailable);
                        let _ = reply.send(result);
                    }
                    StorageCommand::Append {
                        scope,
                        operations,
                        reply,
                    } => {
                        use kboard_core::ports::OpLog;
                        let _ = reply.send(
                            store
                                .append(&scope, &operations)
                                .map_err(|_| StorageError::Unavailable),
                        );
                    }
                    StorageCommand::AppendBatch { write, reply } => {
                        let _ = reply.send(
                            store
                                .append_batch(BatchWrite {
                                    scope: &write.scope,
                                    replica: &write.replica,
                                    batch: &write.batch,
                                    payload_hash: &write.payload_hash,
                                    operations: &write.operations,
                                    recorded_at_millis: write.recorded_at_millis,
                                    retain_after_millis: write.retain_after_millis,
                                })
                                .map_err(|_| StorageError::Unavailable),
                        );
                    }
                    StorageCommand::Snapshot { captured, reply } => {
                        let _ = reply.send(
                            store
                                .commit_snapshot(&captured)
                                .map_err(|_| StorageError::Unavailable),
                        );
                    }
                    StorageCommand::Health { reply } => {
                        let result = store
                            .readiness()
                            .and_then(|readiness| {
                                store.schema_version().and_then(|schema_version| {
                                    store.checkpoint_health().map(|checkpoint| StorageHealth {
                                        schema_version,
                                        checkpoint,
                                        metrics: task_metrics.snapshot(),
                                        readable: readiness.readable,
                                        writable: readiness.writable,
                                    })
                                })
                            })
                            .map_err(|_| StorageError::Unavailable);
                        let _ = reply.send(result);
                    }
                    StorageCommand::Flush { reply } => {
                        let result = store.flush().map_err(|_| StorageError::Unavailable);
                        let _ = reply.send(result);
                    }
                    #[cfg(test)]
                    StorageCommand::Delay {
                        duration,
                        entered,
                        reply,
                    } => {
                        let _ = entered.send(());
                        std::thread::sleep(duration);
                        let _ = reply.send(Ok(()));
                    }
                }
                task_metrics.sql.observe_since(sql_started);
            }
        });
        Self { sender, metrics }
    }

    #[cfg(test)]
    pub fn unavailable_for_test() -> Self {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        Self {
            sender,
            metrics: Arc::new(StorageMetrics::default()),
        }
    }

    pub async fn restore(&self, scope: ScopeId) -> Result<RestoredScope, StorageError> {
        let (reply, response) = oneshot::channel();
        let lifecycle = self.enqueue(StorageCommand::Restore { scope, reply })?;
        receive(lifecycle, response).await
    }

    pub async fn batch_outcome(
        &self,
        scope: ScopeId,
        replica: String,
        batch: String,
        payload_hash: [u8; 32],
        retain_after_millis: u64,
    ) -> Result<Option<BatchAppend>, StorageError> {
        let (reply, response) = oneshot::channel();
        let lifecycle = self.enqueue(StorageCommand::BatchOutcome {
            scope,
            replica,
            batch,
            payload_hash,
            retain_after_millis,
            reply,
        })?;
        receive(lifecycle, response).await
    }

    pub async fn append(
        &self,
        scope: ScopeId,
        operations: Vec<StampedOp>,
    ) -> Result<u64, StorageError> {
        let (reply, response) = oneshot::channel();
        let lifecycle = self.enqueue(StorageCommand::Append {
            scope,
            operations,
            reply,
        })?;
        receive(lifecycle, response).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn append_batch(
        &self,
        scope: ScopeId,
        replica: String,
        batch: String,
        payload_hash: [u8; 32],
        operations: Vec<StampedOp>,
        recorded_at_millis: u64,
        retain_after_millis: u64,
    ) -> Result<BatchAppend, StorageError> {
        let (reply, response) = oneshot::channel();
        let lifecycle = self.enqueue(StorageCommand::AppendBatch {
            write: OwnedBatch {
                scope,
                replica,
                batch,
                payload_hash,
                operations,
                recorded_at_millis,
                retain_after_millis,
            },
            reply,
        })?;
        receive(lifecycle, response).await
    }

    pub async fn snapshot(
        &self,
        captured: CapturedSnapshot,
    ) -> Result<SnapshotCommit, StorageError> {
        let (reply, response) = oneshot::channel();
        let lifecycle = self.enqueue(StorageCommand::Snapshot { captured, reply })?;
        receive(lifecycle, response).await
    }

    pub async fn health(&self) -> Result<StorageHealth, StorageError> {
        let (reply, response) = oneshot::channel();
        let lifecycle = self.enqueue(StorageCommand::Health { reply })?;
        receive(lifecycle, response).await
    }

    pub async fn flush(&self) -> Result<CheckpointHealth, StorageError> {
        let (reply, response) = oneshot::channel();
        let lifecycle = self.enqueue(StorageCommand::Flush { reply })?;
        receive(lifecycle, response).await
    }

    #[cfg(test)]
    async fn delay_for_test(
        &self,
        duration: std::time::Duration,
        entered: oneshot::Sender<()>,
    ) -> Result<(), StorageError> {
        let (reply, response) = oneshot::channel();
        let lifecycle = self.enqueue(StorageCommand::Delay {
            duration,
            entered,
            reply,
        })?;
        receive(lifecycle, response).await
    }

    fn enqueue(&self, command: StorageCommand) -> Result<Arc<AtomicU8>, StorageError> {
        let lifecycle = Arc::new(AtomicU8::new(QUEUED));
        let queued = QueuedStorageCommand {
            enqueued_at: Instant::now(),
            lifecycle: lifecycle.clone(),
            command,
        };
        let depth = self.metrics.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        match self.sender.try_send(queued) {
            Ok(()) => {
                self.metrics
                    .max_queue_depth
                    .fetch_max(depth, Ordering::Relaxed);
                Ok(lifecycle)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.metrics.queue_depth.fetch_sub(1, Ordering::Relaxed);
                self.metrics.overloads.fetch_add(1, Ordering::Relaxed);
                Err(StorageError::Overloaded)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.metrics.queue_depth.fetch_sub(1, Ordering::Relaxed);
                Err(StorageError::Unavailable)
            }
        }
    }
}

async fn receive<T>(
    lifecycle: Arc<AtomicU8>,
    response: oneshot::Receiver<Result<T, StorageError>>,
) -> Result<T, StorageError> {
    let mut response = Box::pin(response);
    let deadline = tokio::time::sleep(storage_deadline());
    tokio::pin!(deadline);
    tokio::select! {
        result = &mut response => result.map_err(|_| StorageError::Unavailable)?,
        () = &mut deadline => {
            if lifecycle
                .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Err(StorageError::Deadline);
            }
            response.await.map_err(|_| StorageError::Unavailable)?
        }
    }
}

#[cfg(not(test))]
fn storage_deadline() -> std::time::Duration {
    limits::STORAGE_DEADLINE
}

#[cfg(test)]
fn storage_deadline() -> std::time::Duration {
    std::time::Duration::from_millis(50)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_core::clock::{ActorId, HlcGenerator};
    use kboard_core::element::ElementId;
    use kboard_core::op::upsert;
    use kboard_core::prop::{PropKey, PropValue};

    #[tokio::test(flavor = "multi_thread")]
    async fn one_writer_preserves_fifo_sequence() {
        let writer = StorageWriter::start(SqliteStore::in_memory().unwrap());
        let scope = ScopeId::new("t:b");
        let mut clock = HlcGenerator::new(ActorId(1));
        let first = upsert(
            ElementId(1),
            [(PropKey::X, PropValue::Num(1.0))],
            &mut clock,
            1,
        );
        let second = upsert(
            ElementId(2),
            [(PropKey::X, PropValue::Num(2.0))],
            &mut clock,
            2,
        );

        assert_eq!(writer.append(scope.clone(), first).await.unwrap(), 1);
        assert_eq!(writer.append(scope, second).await.unwrap(), 2);
        let health = writer.health().await.unwrap();
        assert!(health.metrics.queue_wait.count >= 2);
        assert!(health.metrics.sql.count >= 2);
        assert!(health.metrics.max_queue_depth >= 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deadline_cancels_only_work_that_has_not_started() {
        let writer = StorageWriter::start(SqliteStore::in_memory().unwrap());
        let (entered, started) = oneshot::channel();
        let delayed = {
            let writer = writer.clone();
            tokio::spawn(async move {
                writer
                    .delay_for_test(std::time::Duration::from_millis(80), entered)
                    .await
            })
        };
        started.await.unwrap();

        let mut clock = HlcGenerator::new(ActorId(1));
        let operations = upsert(
            ElementId(1),
            [(PropKey::X, PropValue::Num(1.0))],
            &mut clock,
            1,
        );
        assert_eq!(
            writer.append(ScopeId::new("t:cancelled"), operations).await,
            Err(StorageError::Deadline),
        );
        assert_eq!(delayed.await.unwrap(), Ok(()));

        let restored = writer.restore(ScopeId::new("t:cancelled")).await.unwrap();
        assert_eq!(restored.document().total_count(), 0);
    }
}
