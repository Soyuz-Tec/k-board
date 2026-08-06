//! Scope-to-cell lifecycle directory.
//!
//! The directory owns handles and lifecycle tombstones, never documents. Its
//! lock is held only for lookup/insertion/removal; restore and cell commands
//! always execute after the guard is gone.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kboard_core::document::ScopeId;
use tokio::sync::{RwLock, Semaphore};

use crate::limits;
use crate::room_cell::{LifecycleState, RoomCellHandle};
use crate::security::scope_correlation;
use crate::storage_writer::StorageWriter;
use crate::telemetry::{LatencyMetric, LatencySnapshot};

#[derive(Clone)]
pub struct ScopeDirectory {
    entries: Arc<RwLock<HashMap<String, Entry>>>,
    storage: StorageWriter,
    restores: Arc<Semaphore>,
    next_cell: Arc<AtomicU64>,
    metrics: Arc<DirectoryMetrics>,
}

#[derive(Default)]
struct DirectoryMetrics {
    lookup: LatencyMetric,
    cold_opens: AtomicU64,
}

struct Entry {
    handle: RoomCellHandle,
    inserted_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    Capacity,
    RestoreCapacity,
    FailedCapacity,
    Failed,
    Draining,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct DirectoryStats {
    pub entries: usize,
    pub restoring: usize,
    pub ready: usize,
    pub draining: usize,
    pub failed: usize,
    pub stopped: usize,
    pub restore_permits_available: usize,
    pub cold_opens: u64,
    pub lookup: LatencySnapshot,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct DrainReport {
    pub total: usize,
    pub drained: usize,
    pub incomplete_scope_correlations: Vec<String>,
}

impl ScopeDirectory {
    pub fn new(storage: StorageWriter) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            storage,
            restores: Arc::new(Semaphore::new(limits::MAX_CONCURRENT_RESTORES)),
            next_cell: Arc::new(AtomicU64::new(0)),
            metrics: Arc::new(DirectoryMetrics::default()),
        }
    }

    /// Return the unique owner for `scope`, coalescing concurrent cold opens.
    pub async fn open(&self, scope: &str) -> Result<RoomCellHandle, OpenError> {
        let started = Instant::now();
        let outcome = self.open_measured(scope).await;
        self.metrics.lookup.observe_since(started);
        outcome
    }

    async fn open_measured(&self, scope: &str) -> Result<RoomCellHandle, OpenError> {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get(scope) {
            let status = entry.handle.status();
            match status.lifecycle {
                LifecycleState::Restoring | LifecycleState::Ready => {
                    return Ok(entry.handle.clone());
                }
                LifecycleState::Draining | LifecycleState::Stopped => {
                    return Err(OpenError::Draining);
                }
                LifecycleState::Failed => {
                    if !(entry.handle.is_finished()
                        && status.changed_at.elapsed() >= limits::FAILED_RESTORE_RETRY)
                    {
                        return Err(OpenError::Failed);
                    }
                }
            }
        }

        // A failed tombstone whose backoff elapsed can be replaced only while
        // this write guard is held, so concurrent retrying joins still create
        // exactly one new restoring cell.
        if entries
            .get(scope)
            .is_some_and(|entry| entry.handle.status().lifecycle == LifecycleState::Failed)
        {
            entries.remove(scope);
        }

        let counts = count_entries(&entries, self.restores.available_permits());
        if entries.len() >= limits::MAX_ROOMS {
            return Err(OpenError::Capacity);
        }
        if counts.restoring >= limits::MAX_RESTORING_ROOMS {
            return Err(OpenError::RestoreCapacity);
        }
        // Every restoring entry can become a failed tombstone. Reserve that
        // failure slot at admission time so a simultaneous restore outage can
        // never overshoot the independent failed-entry ceiling.
        if counts.failed.saturating_add(counts.restoring) >= limits::MAX_FAILED_ROOMS {
            return Err(OpenError::FailedCapacity);
        }

        let id = self.next_cell.fetch_add(1, Ordering::Relaxed) + 1;
        let handle = RoomCellHandle::spawn(
            id,
            ScopeId::new(scope),
            self.storage.clone(),
            self.restores.clone(),
        );
        self.metrics.cold_opens.fetch_add(1, Ordering::Relaxed);
        entries.insert(
            scope.to_owned(),
            Entry {
                handle: handle.clone(),
                inserted_at: Instant::now(),
            },
        );
        Ok(handle)
    }

    /// Read-only lookup. Polling stats never restores or creates a scope.
    pub async fn existing(&self, scope: &str) -> Option<RoomCellHandle> {
        self.entries
            .read()
            .await
            .get(scope)
            .map(|entry| entry.handle.clone())
    }

    pub async fn stats(&self) -> DirectoryStats {
        let entries = self.entries.read().await;
        let mut stats = count_entries(&entries, self.restores.available_permits());
        stats.cold_opens = self.metrics.cold_opens.load(Ordering::Relaxed);
        stats.lookup = self.metrics.lookup.snapshot();
        stats
    }

    pub async fn sweep_once(&self) -> usize {
        let snapshot = self
            .entries
            .read()
            .await
            .iter()
            .map(|(scope, entry)| (scope.clone(), entry.handle.clone(), entry.inserted_at))
            .collect::<Vec<_>>();

        for (_, handle, _) in &snapshot {
            let status = handle.status();
            if status.lifecycle == LifecycleState::Ready
                && status
                    .stats
                    .as_ref()
                    .is_some_and(|stats| stats.subscribers == 0)
                && status.idle_for >= limits::ROOM_IDLE_TTL
            {
                let _ = handle.drain().await;
            }
        }

        let removable = snapshot
            .into_iter()
            .filter_map(|(scope, handle, inserted_at)| {
                let status = handle.status();
                let terminal = status.lifecycle == LifecycleState::Stopped && handle.is_finished();
                let expired_failure = status.lifecycle == LifecycleState::Failed
                    && handle.is_finished()
                    && inserted_at.elapsed() >= limits::FAILED_ENTRY_TTL;
                (terminal || expired_failure).then_some((scope, handle.id()))
            })
            .collect::<Vec<_>>();

        let mut entries = self.entries.write().await;
        let before = entries.len();
        for (scope, id) in removable {
            if entries
                .get(&scope)
                .is_some_and(|entry| entry.handle.id() == id)
            {
                entries.remove(&scope);
            }
        }
        before - entries.len()
    }

    pub fn spawn_sweeper(&self) {
        let directory = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(limits::SWEEP_INTERVAL);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let _ = directory.sweep_once().await;
            }
        });
    }

    pub async fn drain_all_bounded(&self, timeout: Duration) -> DrainReport {
        let handles = self
            .entries
            .read()
            .await
            .iter()
            .map(|(scope, entry)| (scope.clone(), entry.handle.clone(), entry.handle.status()))
            .collect::<Vec<_>>();
        let mut report = DrainReport {
            total: handles.len(),
            ..DrainReport::default()
        };
        let deadline = Instant::now() + timeout;
        for (scope, handle, initial) in handles {
            if matches!(
                initial.lifecycle,
                LifecycleState::Restoring | LifecycleState::Ready
            ) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero()
                    || !matches!(
                        tokio::time::timeout(remaining, handle.drain()).await,
                        Ok(Ok(()))
                    )
                {
                    report
                        .incomplete_scope_correlations
                        .push(scope_correlation(&scope));
                    continue;
                }
            }
            report.drained += 1;
        }
        report
    }
}

fn count_entries(
    entries: &HashMap<String, Entry>,
    restore_permits_available: usize,
) -> DirectoryStats {
    let mut stats = DirectoryStats {
        entries: entries.len(),
        restore_permits_available,
        ..DirectoryStats::default()
    };
    for entry in entries.values() {
        match entry.handle.status().lifecycle {
            LifecycleState::Restoring => stats.restoring += 1,
            LifecycleState::Ready => stats.ready += 1,
            LifecycleState::Draining => stats.draining += 1,
            LifecycleState::Failed => stats.failed += 1,
            LifecycleState::Stopped => stats.stopped += 1,
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_store::SqliteStore;

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_cold_opens_coalesce_to_one_cell() {
        let directory =
            ScopeDirectory::new(StorageWriter::start(SqliteStore::in_memory().unwrap()));
        let (left, right) = tokio::join!(directory.open("t:b"), directory.open("t:b"));
        assert_eq!(left.unwrap().id(), right.unwrap().id());
        assert_eq!(directory.stats().await.entries, 1);
    }

    async fn wait_for(cell: &RoomCellHandle, expected: LifecycleState) {
        for _ in 0..1_000 {
            if cell.status().lifecycle == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("cell did not reach {expected:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_restore_is_a_tombstone_instead_of_an_empty_replacement() {
        let directory = ScopeDirectory::new(StorageWriter::unavailable_for_test());
        let cell = directory.open("t:failed").await.unwrap();
        wait_for(&cell, LifecycleState::Failed).await;

        assert_eq!(
            directory.open("t:failed").await.err(),
            Some(OpenError::Failed)
        );
        let stats = directory.stats().await;
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.ready, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejoin_during_drain_never_creates_a_second_owner() {
        let directory =
            ScopeDirectory::new(StorageWriter::start(SqliteStore::in_memory().unwrap()));
        let cell = directory.open("t:drain").await.unwrap();
        wait_for(&cell, LifecycleState::Ready).await;
        cell.drain().await.unwrap();

        assert_eq!(
            directory.open("t:drain").await.err(),
            Some(OpenError::Draining)
        );
        wait_for(&cell, LifecycleState::Stopped).await;
        for _ in 0..1_000 {
            if cell.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(directory.sweep_once().await, 1);
        let replacement = directory.open("t:drain").await.unwrap();
        assert_ne!(cell.id(), replacement.id());
    }

    #[test]
    fn ten_thousand_directory_slots_have_a_small_inline_metadata_ceiling() {
        let bytes = std::mem::size_of::<Entry>().saturating_mul(10_000);
        assert!(bytes < 4 * 1024 * 1024, "entry metadata uses {bytes} bytes");
        assert_eq!(
            limits::MAX_ROOMS,
            10_000,
            "the declared capacity is part of the calculation"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn churn_at_the_room_limit_leaves_no_inactive_entries_or_tasks() {
        let directory =
            ScopeDirectory::new(StorageWriter::start(SqliteStore::in_memory().unwrap()));
        {
            let mut entries = directory.entries.write().await;
            for index in 0..limits::MAX_ROOMS {
                entries.insert(
                    format!("t:{index}"),
                    Entry {
                        handle: RoomCellHandle::stopped_for_test(index as u64 + 1),
                        inserted_at: Instant::now(),
                    },
                );
            }
        }
        let at_limit = directory.stats().await;
        assert_eq!(at_limit.entries, limits::MAX_ROOMS);
        assert_eq!(at_limit.stopped, limits::MAX_ROOMS);
        assert_eq!(directory.sweep_once().await, limits::MAX_ROOMS);
        assert_eq!(directory.stats().await.entries, 0);
    }
}
