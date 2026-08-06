//! # kboard-store
//!
//! SQLite implementations of the engine's [`OpLog`] and [`SnapshotStore`]
//! ports. This is where the operation log went when ADR-0005 took it out of the
//! room: durable storage, where truncation is a retention policy rather than a
//! heuristic over a growing `Vec`.
//!
//! Nothing here is reachable from `kboard-core`. The engine declares what it
//! needs and this crate supplies it, which is the same arrangement a host
//! platform uses when it plugs in its own Postgres — see `docs/adr/0007`.
//!
//! ## Scope is opaque here too
//!
//! The store keys everything by [`ScopeId`] and never parses it. Two tenants
//! are two different strings and nothing else; enforcing what that means is the
//! host's job, exactly as it is in the engine.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod migration;

use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use kboard_core::document::ScopeId;
use kboard_core::op::StampedOp;
use kboard_core::ports::{OpLog, PortError, SnapshotStore};
use kboard_core::snapshot::{CapturedSnapshot, Snapshot};
use rusqlite::{Connection, OptionalExtension};

/// Hard serialized-state ceiling for the standalone adapter. The server keeps
/// a lower materialized estimate; this is defense in depth for imported or
/// externally modified databases.
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_RESTORE_OPERATIONS: u64 = 1_000_000;

/// Outcome of an idempotent protocol batch append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchAppend {
    /// The operations and batch outcome were committed together.
    Committed(u64),
    /// This exact batch was committed earlier; no operations were appended.
    Duplicate(u64),
    /// The identity was reused with a different canonical payload hash.
    Conflict,
}

/// Inputs that must be committed atomically for one protocol-v2 batch.
pub struct BatchWrite<'a> {
    pub scope: &'a ScopeId,
    pub replica: &'a str,
    pub batch: &'a str,
    pub payload_hash: &'a [u8; 32],
    pub operations: &'a [StampedOp],
    pub recorded_at_millis: u64,
    pub retain_after_millis: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreSource {
    Empty,
    SnapshotAndTail,
    CompleteLogAfterSnapshotCorruption,
}

pub struct RestoredScope {
    pub captured: CapturedSnapshot,
    pub snapshot_sequence: u64,
    pub source: RestoreSource,
}

impl RestoredScope {
    pub const fn snapshot(&self) -> &Snapshot {
        self.captured.snapshot()
    }

    pub const fn document(&self) -> &kboard_core::document::Document {
        self.captured.snapshot().document()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotCommit {
    pub truncated_operations: u64,
    pub encoded_bytes: usize,
    pub encode_micros: u64,
    pub write_micros: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointHealth {
    pub busy: u32,
    pub log_frames: u32,
    pub checkpointed_frames: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadinessCheck {
    pub readable: bool,
    pub writable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DatabaseVerification {
    pub schema_version: u32,
    pub integrity_ok: bool,
    pub foreign_key_violations: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RestoreVerification {
    pub schema_version: u32,
    pub scopes: u64,
    pub restored_operations: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct BackupReport {
    pub bytes: u64,
    pub verification: DatabaseVerification,
}

/// A SQLite-backed durable store.
///
/// Holds one connection. The server serialises access behind the same lock that
/// guards its rooms, so concurrent writers are not a concern here — and making
/// this internally shareable would invite a second lock with its own ordering
/// rules for no benefit at the current shape.
pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    /// Open (or create) a store at `path`.
    ///
    /// # Errors
    ///
    /// [`PortError::Backend`] if the file cannot be opened or the schema
    /// cannot be applied.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PortError> {
        let connection = Connection::open(path).map_err(backend)?;
        Self::prepare(connection)
    }

    /// An in-memory store. For tests and for running the server without a
    /// durable file; it behaves identically and vanishes on close.
    ///
    /// # Errors
    ///
    /// [`PortError::Backend`] if the schema cannot be applied.
    pub fn in_memory() -> Result<Self, PortError> {
        let connection = Connection::open_in_memory().map_err(backend)?;
        Self::prepare(connection)
    }

    fn prepare(mut connection: Connection) -> Result<Self, PortError> {
        // WAL so a reader never blocks the writer: joins read while edits are
        // still committing. NORMAL rather than FULL because losing the last few
        // milliseconds of drawing to an OS crash is an acceptable trade for not
        // fsyncing on every stroke - and the operation log is idempotent, so a
        // client that retries closes the gap.
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(backend)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA wal_autocheckpoint = 1000;",
            )
            .map_err(backend)?;
        migration::migrate(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn schema_version(&self) -> Result<u32, PortError> {
        migration::schema_version(&self.connection)
    }

    /// A readiness probe that proves both a read transaction and immediate
    /// writer acquisition without changing application data.
    pub fn readiness(&self) -> Result<ReadinessCheck, PortError> {
        let integrity = self
            .connection
            .query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
            .map_err(backend)?;
        if integrity != "ok" {
            return Err(PortError::Backend("database quick check failed".to_owned()));
        }
        self.connection
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
            .map_err(backend)?;
        Ok(ReadinessCheck {
            readable: true,
            writable: true,
        })
    }

    /// Force committed WAL frames into the main database before shutdown.
    pub fn flush(&self) -> Result<CheckpointHealth, PortError> {
        self.connection
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
                Ok(CheckpointHealth {
                    busy: row.get::<_, i64>(0)?.max(0) as u32,
                    log_frames: row.get::<_, i64>(1)?.max(0) as u32,
                    checkpointed_frames: row.get::<_, i64>(2)?.max(0) as u32,
                })
            })
            .map_err(backend)
    }

    /// SQLite's online backup API captures a transactionally consistent image
    /// while the source remains available. The image is accepted only after a
    /// read-only integrity and schema check.
    pub fn online_backup(&self, path: impl AsRef<Path>) -> Result<BackupReport, PortError> {
        let path = path.as_ref();
        self.connection
            .backup(rusqlite::MAIN_DB, path, None)
            .map_err(backend)?;
        let verification = Self::verify_database(path)?;
        let bytes = std::fs::metadata(path).map_err(backend)?.len();
        Ok(BackupReport {
            bytes,
            verification,
        })
    }

    pub fn verify_database(path: impl AsRef<Path>) -> Result<DatabaseVerification, PortError> {
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(backend)?;
        let integrity = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .map_err(backend)?;
        let foreign_key_violations = connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(backend)?
            .max(0) as u64;
        let schema_version = migration::schema_version(&connection)?;
        if integrity != "ok" || foreign_key_violations != 0 {
            return Err(PortError::Backend(
                "database integrity verification failed".to_owned(),
            ));
        }
        Ok(DatabaseVerification {
            schema_version,
            integrity_ok: true,
            foreign_key_violations,
        })
    }

    /// Restore a backup into an in-memory database and rebuild every scope.
    /// The source backup is never opened writable.
    pub fn verify_restore(path: impl AsRef<Path>) -> Result<RestoreVerification, PortError> {
        let mut connection = Connection::open_in_memory().map_err(backend)?;
        connection
            .restore(
                rusqlite::MAIN_DB,
                path,
                None::<fn(rusqlite::backup::Progress)>,
            )
            .map_err(backend)?;
        let store = Self::prepare(connection)?;
        let schema_version = store.schema_version()?;
        let scopes = store.scopes()?;
        let mut restored_operations = 0_u64;
        for scope in &scopes {
            restored_operations = restored_operations
                .saturating_add(store.restore(scope)?.captured.through_sequence());
        }
        Ok(RestoreVerification {
            schema_version,
            scopes: scopes.len() as u64,
            restored_operations,
        })
    }

    /// Scopes that hold any durable state, so a restarting server knows what to
    /// restore without being told.
    ///
    /// # Errors
    ///
    /// [`PortError::Backend`] on a query failure.
    pub fn scopes(&self) -> Result<Vec<ScopeId>, PortError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT scope FROM snapshots
                 UNION
                 SELECT DISTINCT scope FROM operations
                 ORDER BY scope",
            )
            .map_err(backend)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend)?;

        let mut scopes = Vec::new();
        for row in rows {
            scopes.push(ScopeId::new(row.map_err(backend)?));
        }
        Ok(scopes)
    }

    /// Rebuild a scope's document: its snapshot, then everything after it.
    ///
    /// This is the whole point of the crate — the call a restarting server
    /// makes so a board survives the process that was holding it.
    ///
    /// # Errors
    ///
    /// [`PortError::Backend`] on a query or decode failure.
    pub fn restore(&self, scope: &ScopeId) -> Result<RestoredScope, PortError> {
        let stored = self.raw_snapshot(scope)?;
        match stored {
            None => {
                self.ensure_restore_operation_bound_since(scope, 0)?;
                let operations = self.read_since(scope, 0)?;
                ensure_restore_operation_bound(operations.len())?;
                let sequence = self.latest_sequence(scope)?;
                let source = if operations.is_empty() {
                    RestoreSource::Empty
                } else {
                    RestoreSource::SnapshotAndTail
                };
                Ok(RestoredScope {
                    captured: CapturedSnapshot::new(
                        Snapshot::materialize(scope.clone(), &operations),
                        sequence,
                    ),
                    snapshot_sequence: 0,
                    source,
                })
            }
            Some((through, document)) => {
                let decoded = serde_json::from_str::<Snapshot>(&document).map_err(|error| {
                    PortError::Backend(format!("snapshot decode failed: {error}"))
                });
                match decoded {
                    Ok(mut snapshot) if snapshot.scope() == scope => {
                        self.ensure_restore_operation_bound_since(scope, through)?;
                        let tail = self.read_since(scope, through)?;
                        ensure_restore_operation_bound(tail.len())?;
                        snapshot.absorb(&tail);
                        Ok(RestoredScope {
                            captured: CapturedSnapshot::new(
                                snapshot,
                                self.latest_sequence(scope)?.max(through),
                            ),
                            snapshot_sequence: through,
                            source: RestoreSource::SnapshotAndTail,
                        })
                    }
                    Ok(_) => Err(PortError::Backend("snapshot scope mismatch".to_owned())),
                    Err(decode_error) => {
                        let Some(sequence) = self.complete_log_sequence(scope, through)? else {
                            return Err(PortError::Backend(format!(
                                "{decode_error}; corrupt snapshot preserved; complete log recovery unavailable"
                            )));
                        };
                        self.ensure_restore_operation_bound_since(scope, 0)?;
                        let operations = self.read_since(scope, 0)?;
                        ensure_restore_operation_bound(operations.len())?;
                        Ok(RestoredScope {
                            captured: CapturedSnapshot::new(
                                Snapshot::materialize(scope.clone(), &operations),
                                sequence,
                            ),
                            snapshot_sequence: 0,
                            source: RestoreSource::CompleteLogAfterSnapshotCorruption,
                        })
                    }
                }
            }
        }
    }

    fn raw_snapshot(&self, scope: &ScopeId) -> Result<Option<(u64, String)>, PortError> {
        let size = self
            .connection
            .query_row(
                "SELECT LENGTH(document) FROM snapshots WHERE scope = ?1",
                rusqlite::params![scope.0],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(backend)?;
        if size.is_some_and(|bytes| bytes.max(0) as u64 > MAX_SNAPSHOT_BYTES as u64) {
            return Err(PortError::Backend(
                "snapshot exceeds the configured restore byte limit".to_owned(),
            ));
        }
        let snapshot = self
            .connection
            .query_row(
                "SELECT through, document FROM snapshots WHERE scope = ?1",
                rusqlite::params![scope.0],
                |row| {
                    let through = row.get::<_, i64>(0)?;
                    let document = row.get::<_, String>(1)?;
                    Ok((through.max(0) as u64, document))
                },
            )
            .optional()
            .map_err(backend)?;
        if snapshot
            .as_ref()
            .is_some_and(|(_, document)| document.len() > MAX_SNAPSHOT_BYTES)
        {
            return Err(PortError::Backend(
                "snapshot exceeds the configured restore byte limit".to_owned(),
            ));
        }
        Ok(snapshot)
    }

    fn ensure_restore_operation_bound_since(
        &self,
        scope: &ScopeId,
        after: u64,
    ) -> Result<(), PortError> {
        let after = to_sql_integer(after)?;
        let count = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE scope = ?1 AND seq > ?2",
                rusqlite::params![scope.0, after],
                |row| row.get::<_, i64>(0),
            )
            .map_err(backend)?;
        ensure_restore_operation_bound(count.max(0) as usize)
    }

    fn latest_sequence(&self, scope: &ScopeId) -> Result<u64, PortError> {
        self.connection
            .query_row(
                "SELECT MAX(value) FROM (
                   SELECT COALESCE(MAX(seq), 0) AS value FROM operations WHERE scope = ?1
                   UNION ALL
                   SELECT COALESCE(MAX(through), 0) AS value FROM snapshots WHERE scope = ?1
                 )",
                rusqlite::params![scope.0],
                |row| row.get::<_, i64>(0),
            )
            .map(|sequence| sequence.max(0) as u64)
            .map_err(backend)
    }

    fn complete_log_sequence(
        &self,
        scope: &ScopeId,
        snapshot_through: u64,
    ) -> Result<Option<u64>, PortError> {
        let (minimum, maximum, count) = self
            .connection
            .query_row(
                "SELECT MIN(seq), MAX(seq), COUNT(*) FROM operations WHERE scope = ?1",
                rusqlite::params![scope.0],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(backend)?;
        match (minimum, maximum) {
            (None, None) if snapshot_through == 0 => Ok(Some(0)),
            (Some(1), Some(maximum))
                if count == maximum && maximum.max(0) as u64 >= snapshot_through =>
            {
                Ok(Some(maximum as u64))
            }
            _ => Ok(None),
        }
    }

    pub fn checkpoint_health(&self) -> Result<CheckpointHealth, PortError> {
        self.connection
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok(CheckpointHealth {
                    busy: row.get::<_, i64>(0)?.max(0) as u32,
                    // SQLite reports -1/-1 when the current database cannot
                    // use WAL (notably the test-only in-memory adapter). Health
                    // remains queryable and exposes that state as zero frames.
                    log_frames: row.get::<_, i64>(1)?.max(0) as u32,
                    checkpointed_frames: row.get::<_, i64>(2)?.max(0) as u32,
                })
            })
            .map_err(backend)
    }

    /// Store a captured snapshot and delete exactly its covered log prefix in
    /// one transaction. Encoding happens before the write transaction so its
    /// cost is observable and does not hold SQLite's writer lock.
    pub fn commit_snapshot(
        &mut self,
        captured: &CapturedSnapshot,
    ) -> Result<SnapshotCommit, PortError> {
        let encode_started = Instant::now();
        let document = serde_json::to_string(captured.snapshot())
            .map_err(|error| PortError::Backend(error.to_string()))?;
        let encoded_bytes = document.len();
        if encoded_bytes > MAX_SNAPSHOT_BYTES {
            return Err(PortError::Backend(
                "snapshot exceeds the configured storage byte limit".to_owned(),
            ));
        }
        let encode_micros = elapsed_micros(encode_started);
        let through = to_sql_integer(captured.through_sequence())?;

        let write_started = Instant::now();
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let available = transaction
            .query_row(
                "SELECT MAX(value) FROM (
                   SELECT COALESCE(MAX(seq), 0) AS value FROM operations WHERE scope = ?1
                   UNION ALL
                   SELECT COALESCE(MAX(through), 0) AS value FROM snapshots WHERE scope = ?1
                 )",
                rusqlite::params![captured.scope().0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(backend)?;
        if through > available {
            return Err(PortError::Backend(
                "snapshot coverage exceeds durable log sequence".to_owned(),
            ));
        }

        let current = transaction
            .query_row(
                "SELECT through FROM snapshots WHERE scope = ?1",
                rusqlite::params![captured.scope().0],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(backend)?;
        if current.is_some_and(|current| current > through) {
            transaction.commit().map_err(backend)?;
            return Ok(SnapshotCommit {
                truncated_operations: 0,
                encoded_bytes,
                encode_micros,
                write_micros: elapsed_micros(write_started),
            });
        }

        transaction
            .execute(
                "INSERT INTO snapshots (scope, through, document) VALUES (?1, ?2, ?3)
                 ON CONFLICT(scope) DO UPDATE SET through = excluded.through,
                                                   document = excluded.document",
                rusqlite::params![captured.scope().0, through, document],
            )
            .map_err(backend)?;
        let truncated = transaction
            .execute(
                "DELETE FROM operations WHERE scope = ?1 AND seq <= ?2",
                rusqlite::params![captured.scope().0, through],
            )
            .map_err(backend)?;
        transaction.commit().map_err(backend)?;

        Ok(SnapshotCommit {
            truncated_operations: truncated as u64,
            encoded_bytes,
            encode_micros,
            write_micros: elapsed_micros(write_started),
        })
    }

    /// Append a protocol-v2 batch and its deduplication outcome atomically.
    ///
    /// `retain_after_millis` is the inclusive lower retention bound. Records
    /// older than it are removed before resolving the identity. The caller
    /// supplies the canonical operation payload hash so this adapter remains
    /// independent of wire-protocol policy.
    pub fn append_batch(&mut self, write: BatchWrite<'_>) -> Result<BatchAppend, PortError> {
        let BatchWrite {
            scope,
            replica,
            batch,
            payload_hash,
            operations,
            recorded_at_millis,
            retain_after_millis,
        } = write;
        let transaction = self.connection.transaction().map_err(backend)?;
        transaction
            .execute(
                "DELETE FROM batches WHERE recorded_at < ?1",
                rusqlite::params![to_sql_integer(retain_after_millis)?],
            )
            .map_err(backend)?;

        let existing = transaction
            .query_row(
                "SELECT payload_hash, sequence FROM batches
                 WHERE scope = ?1 AND replica = ?2 AND batch = ?3",
                rusqlite::params![scope.0, replica, batch],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(backend)?;
        if let Some((stored_hash, sequence)) = existing {
            transaction.commit().map_err(backend)?;
            return if stored_hash == payload_hash {
                Ok(BatchAppend::Duplicate(sequence.max(0) as u64))
            } else {
                Ok(BatchAppend::Conflict)
            };
        }

        let mut sequence = transaction
            .query_row(
                "SELECT MAX(value) FROM (
                   SELECT COALESCE(MAX(seq), 0) AS value FROM operations WHERE scope = ?1
                   UNION ALL
                   SELECT COALESCE(MAX(through), 0) AS value FROM snapshots WHERE scope = ?1
                 )",
                rusqlite::params![scope.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(backend)?;
        {
            let mut statement = transaction
                .prepare("INSERT INTO operations (scope, seq, payload) VALUES (?1, ?2, ?3)")
                .map_err(backend)?;
            for op in operations {
                sequence += 1;
                let payload = serde_json::to_string(op)
                    .map_err(|error| PortError::Backend(error.to_string()))?;
                statement
                    .execute(rusqlite::params![scope.0, sequence, payload])
                    .map_err(backend)?;
            }
        }
        transaction
            .execute(
                "INSERT INTO batches
                 (scope, replica, batch, payload_hash, sequence, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    scope.0,
                    replica,
                    batch,
                    payload_hash.as_slice(),
                    sequence,
                    to_sql_integer(recorded_at_millis)?
                ],
            )
            .map_err(backend)?;
        transaction.commit().map_err(backend)?;
        Ok(BatchAppend::Committed(sequence.max(0) as u64))
    }

    /// Resolve a retained batch identity without appending. This lets callers
    /// return an earlier successful acknowledgement before applying present-day
    /// room capacity checks to a retry that already committed.
    pub fn batch_outcome(
        &mut self,
        scope: &ScopeId,
        replica: &str,
        batch: &str,
        payload_hash: &[u8; 32],
        retain_after_millis: u64,
    ) -> Result<Option<BatchAppend>, PortError> {
        self.connection
            .execute(
                "DELETE FROM batches WHERE recorded_at < ?1",
                rusqlite::params![to_sql_integer(retain_after_millis)?],
            )
            .map_err(backend)?;
        let existing = self
            .connection
            .query_row(
                "SELECT payload_hash, sequence FROM batches
                 WHERE scope = ?1 AND replica = ?2 AND batch = ?3",
                rusqlite::params![scope.0, replica, batch],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(backend)?;
        Ok(existing.map(|(stored_hash, sequence)| {
            if stored_hash == payload_hash {
                BatchAppend::Duplicate(sequence.max(0) as u64)
            } else {
                BatchAppend::Conflict
            }
        }))
    }
}

impl OpLog for SqliteStore {
    fn append(&mut self, scope: &ScopeId, ops: &[StampedOp]) -> Result<u64, PortError> {
        if ops.is_empty() {
            return self.latest_sequence(scope);
        }

        // One transaction for the batch. A partially written batch would leave
        // the log describing a state no replica ever held.
        let transaction = self.connection.transaction().map_err(backend)?;
        let mut seq = transaction
            .query_row(
                "SELECT MAX(value) FROM (
                   SELECT COALESCE(MAX(seq), 0) AS value FROM operations WHERE scope = ?1
                   UNION ALL
                   SELECT COALESCE(MAX(through), 0) AS value FROM snapshots WHERE scope = ?1
                 )",
                rusqlite::params![scope.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(backend)?;

        {
            let mut statement = transaction
                .prepare("INSERT INTO operations (scope, seq, payload) VALUES (?1, ?2, ?3)")
                .map_err(backend)?;
            for op in ops {
                seq += 1;
                let payload = serde_json::to_string(op)
                    .map_err(|error| PortError::Backend(error.to_string()))?;
                statement
                    .execute(rusqlite::params![scope.0, seq, payload])
                    .map_err(backend)?;
            }
        }

        transaction.commit().map_err(backend)?;
        Ok(seq as u64)
    }

    fn read_since(&self, scope: &ScopeId, after: u64) -> Result<Vec<StampedOp>, PortError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload FROM operations
                 WHERE scope = ?1 AND seq > ?2
                 ORDER BY seq",
            )
            .map_err(backend)?;
        let rows = statement
            .query_map(rusqlite::params![scope.0, after as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(backend)?;

        let mut ops = Vec::new();
        for row in rows {
            let payload = row.map_err(backend)?;
            ops.push(
                serde_json::from_str(&payload)
                    .map_err(|error| PortError::Backend(error.to_string()))?,
            );
        }
        Ok(ops)
    }

    fn count(&self, scope: &ScopeId) -> Result<u64, PortError> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE scope = ?1",
                rusqlite::params![scope.0],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as u64)
            .map_err(backend)
    }
}

impl SnapshotStore for SqliteStore {
    fn load(&self, scope: &ScopeId) -> Result<Option<CapturedSnapshot>, PortError> {
        let Some((through, document)) = self.raw_snapshot(scope)? else {
            return Ok(None);
        };
        let snapshot: Snapshot = serde_json::from_str(&document)
            .map_err(|error| PortError::Backend(error.to_string()))?;
        if snapshot.scope() != scope {
            return Err(PortError::Backend("snapshot scope mismatch".to_owned()));
        }
        Ok(Some(CapturedSnapshot::new(snapshot, through)))
    }

    fn store(&mut self, snapshot: &CapturedSnapshot) -> Result<u64, PortError> {
        self.commit_snapshot(snapshot)
            .map(|commit| commit.truncated_operations)
    }
}

fn backend(error: impl std::fmt::Display) -> PortError {
    PortError::Backend(error.to_string())
}

fn to_sql_integer(value: u64) -> Result<i64, PortError> {
    i64::try_from(value).map_err(|_| PortError::Backend("integer exceeds SQLite range".to_owned()))
}

fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}

fn ensure_restore_operation_bound(count: usize) -> Result<(), PortError> {
    if count as u64 > MAX_RESTORE_OPERATIONS {
        return Err(PortError::Backend(
            "operation log exceeds the configured restore limit".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_core::clock::{ActorId, HlcGenerator};
    use kboard_core::element::ElementId;
    use kboard_core::op::{upsert, Op};
    use kboard_core::prop::{PropKey, PropValue};
    use rusqlite::OpenFlags;

    fn scope() -> ScopeId {
        ScopeId::new("tenant-1/board-1")
    }

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

    fn delete_op(actor: u64, element: u128, at: u64) -> StampedOp {
        let mut clock = HlcGenerator::new(ActorId(actor));
        StampedOp::new(
            clock.tick(at),
            Op::Delete {
                element: ElementId(element),
            },
        )
    }

    fn table_count(store: &SqliteStore, table: &str) -> u64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        store
            .connection
            .query_row(&sql, [], |row| row.get::<_, i64>(0))
            .unwrap() as u64
    }

    fn database_from_fixture(sql: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let connection = Connection::open(file.path()).unwrap();
        connection.execute_batch(sql).unwrap();
        drop(connection);
        file
    }

    #[test]
    fn every_supported_schema_fixture_migrates_to_current() {
        for fixture in [
            include_str!("../fixtures/schema/v0.sql"),
            include_str!("../fixtures/schema/v1.sql"),
            include_str!("../fixtures/schema/v2.sql"),
            include_str!("../fixtures/schema/legacy-v1.sql"),
            include_str!("../fixtures/schema/legacy-v2.sql"),
        ] {
            let file = database_from_fixture(fixture);
            let store = SqliteStore::open(file.path()).unwrap();
            assert_eq!(
                store.schema_version().unwrap(),
                migration::CURRENT_SCHEMA_VERSION
            );
        }
    }

    #[test]
    fn a_v1_migration_preserves_operations_and_adds_batch_identity() {
        let file = database_from_fixture(include_str!("../fixtures/schema/v1.sql"));
        let operation = serde_json::to_string(&ops(1, 1)[0]).unwrap();
        let connection = Connection::open(file.path()).unwrap();
        connection
            .execute(
                "INSERT INTO operations(scope, seq, payload) VALUES (?1, 1, ?2)",
                rusqlite::params![scope().0, operation],
            )
            .unwrap();
        drop(connection);

        let mut store = SqliteStore::open(file.path()).unwrap();
        assert_eq!(store.read_since(&scope(), 0).unwrap().len(), 1);
        assert!(matches!(
            store
                .append_batch(BatchWrite {
                    scope: &scope(),
                    replica: "replica",
                    batch: "batch",
                    payload_hash: &[1; 32],
                    operations: &ops(2, 1),
                    recorded_at_millis: 10,
                    retain_after_millis: 0,
                })
                .unwrap(),
            BatchAppend::Committed(_)
        ));
    }

    #[test]
    fn a_future_schema_is_refused() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let connection = Connection::open(file.path()).unwrap();
        connection
            .pragma_update(None, "user_version", migration::CURRENT_SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);

        assert!(SqliteStore::open(file.path()).is_err());
    }

    #[test]
    fn an_unversioned_partial_schema_is_refused_without_adoption() {
        let file = database_from_fixture(
            "CREATE TABLE operations (
               scope TEXT NOT NULL,
               seq INTEGER NOT NULL,
               payload TEXT NOT NULL,
               PRIMARY KEY (scope, seq)
             ) WITHOUT ROWID;",
        );

        assert!(SqliteStore::open(file.path()).is_err());
        let connection = Connection::open(file.path()).unwrap();
        let version = migration::schema_version(&connection).unwrap();
        assert_eq!(version, 0, "a refused partial schema must not be adopted");
    }

    #[test]
    fn a_board_survives_being_closed_and_reopened() {
        // The whole point: this is what "a restart destroys every board" was.
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();

        {
            let mut store = SqliteStore::open(&path).unwrap();
            store.append(&scope(), &ops(1, 5)).unwrap();
        }

        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(
            reopened.restore(&scope()).unwrap().document().live_count(),
            5
        );
    }

    #[test]
    fn restore_equals_a_snapshot_plus_its_tail() {
        let mut store = SqliteStore::in_memory().unwrap();
        let all = ops(1, 10);
        let (head, tail) = all.split_at(6);

        let through = store.append(&scope(), head).unwrap();
        let mut folded = Snapshot::materialize(scope(), head);
        store
            .store(&CapturedSnapshot::new(folded.clone(), through))
            .unwrap();
        store.append(&scope(), tail).unwrap();

        folded.absorb(tail);
        assert_eq!(
            store.restore(&scope()).unwrap().document(),
            folded.document()
        );
    }

    #[test]
    fn sequences_continue_across_appends() {
        let mut store = SqliteStore::in_memory().unwrap();
        let first = store.append(&scope(), &ops(1, 3)).unwrap();
        let second = store.append(&scope(), &ops(2, 3)).unwrap();

        assert!(second > first, "a later batch must not reuse sequences");
        assert_eq!(store.count(&scope()).unwrap(), second);
        assert_eq!(store.read_since(&scope(), first).unwrap().len(), 3);
    }

    #[test]
    fn protocol_batches_are_idempotent_and_detect_identity_reuse() {
        let mut store = SqliteStore::in_memory().unwrap();
        let operations = ops(1, 2);
        let hash = [7; 32];
        let first = store
            .append_batch(BatchWrite {
                scope: &scope(),
                replica: "replica",
                batch: "batch",
                payload_hash: &hash,
                operations: &operations,
                recorded_at_millis: 10_000,
                retain_after_millis: 0,
            })
            .unwrap();
        let BatchAppend::Committed(sequence) = first else {
            panic!("first append must commit");
        };

        assert_eq!(
            store
                .append_batch(BatchWrite {
                    scope: &scope(),
                    replica: "replica",
                    batch: "batch",
                    payload_hash: &hash,
                    operations: &operations,
                    recorded_at_millis: 11_000,
                    retain_after_millis: 0,
                })
                .unwrap(),
            BatchAppend::Duplicate(sequence)
        );
        assert_eq!(store.count(&scope()).unwrap(), 2);
        assert_eq!(
            store
                .batch_outcome(&scope(), "replica", "batch", &[8; 32], 0)
                .unwrap(),
            Some(BatchAppend::Conflict)
        );
    }

    #[test]
    fn batch_outcomes_survive_reopening_the_database() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let hash = [9; 32];
        let sequence = {
            let mut store = SqliteStore::open(&path).unwrap();
            let operations = ops(1, 1);
            let BatchAppend::Committed(sequence) = store
                .append_batch(BatchWrite {
                    scope: &scope(),
                    replica: "replica",
                    batch: "batch",
                    payload_hash: &hash,
                    operations: &operations,
                    recorded_at_millis: 10_000,
                    retain_after_millis: 0,
                })
                .unwrap()
            else {
                panic!("first append must commit");
            };
            sequence
        };

        let mut reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(
            reopened
                .batch_outcome(&scope(), "replica", "batch", &hash, 0)
                .unwrap(),
            Some(BatchAppend::Duplicate(sequence))
        );
    }

    #[test]
    fn scopes_do_not_leak_into_each_other() {
        let mut store = SqliteStore::in_memory().unwrap();
        let mine = ScopeId::new("tenant-a/board");
        let theirs = ScopeId::new("tenant-b/board");

        store.append(&mine, &ops(1, 4)).unwrap();

        // A store keyed by an opaque scope is the last place a routing bug can
        // be caught before one tenant's board is filed under another's key.
        assert_eq!(store.count(&theirs).unwrap(), 0);
        assert!(store.read_since(&theirs, 0).unwrap().is_empty());
        assert_eq!(store.restore(&theirs).unwrap().document().live_count(), 0);
    }

    #[test]
    fn a_snapshot_cannot_claim_a_sequence_that_is_not_durable() {
        let mut store = SqliteStore::in_memory().unwrap();
        let snapshot = Snapshot::empty(ScopeId::new("tenant-a/board"));
        assert!(store.store(&CapturedSnapshot::new(snapshot, 1)).is_err());
    }

    #[test]
    fn truncation_keeps_the_restored_document_identical() {
        let mut store = SqliteStore::in_memory().unwrap();
        let all = ops(1, 8);
        let through = store.append(&scope(), &all).unwrap();
        let snapshot = Snapshot::materialize(scope(), &all);
        let before = snapshot.document().clone();
        let removed = store
            .store(&CapturedSnapshot::new(snapshot, through))
            .unwrap();

        assert!(removed > 0, "absorbed operations should be collectable");
        assert_eq!(
            store.restore(&scope()).unwrap().document(),
            &before,
            "truncating what a snapshot already holds must change nothing"
        );
    }

    #[test]
    fn snapshot_capture_at_n_keeps_later_appends_as_tail() {
        let mut store = SqliteStore::in_memory().unwrap();
        let head = ops(1, 4);
        let tail = ops(2, 3);
        let through = store.append(&scope(), &head).unwrap();
        let captured = CapturedSnapshot::new(Snapshot::materialize(scope(), &head), through);

        let latest = store.append(&scope(), &tail).unwrap();
        let committed = store.commit_snapshot(&captured).unwrap();

        assert_eq!(through, 4);
        assert_eq!(latest, 7);
        assert_eq!(committed.truncated_operations, through);
        assert_eq!(store.count(&scope()).unwrap(), tail.len() as u64);
        let expected =
            Snapshot::materialize(scope(), &head.into_iter().chain(tail).collect::<Vec<_>>());
        let restored = store.restore(&scope()).unwrap();
        assert_eq!(restored.captured.through_sequence(), latest);
        assert_eq!(restored.document(), expected.document());
    }

    #[test]
    fn an_older_snapshot_cannot_replace_a_newer_committed_snapshot() {
        let mut store = SqliteStore::in_memory().unwrap();
        let first = ops(1, 3);
        let first_sequence = store.append(&scope(), &first).unwrap();
        let older = CapturedSnapshot::new(Snapshot::materialize(scope(), &first), first_sequence);
        let second = ops(2, 2);
        let latest = store.append(&scope(), &second).unwrap();
        let all = first.iter().chain(&second).cloned().collect::<Vec<_>>();
        let newer = CapturedSnapshot::new(Snapshot::materialize(scope(), &all), latest);
        store.commit_snapshot(&newer).unwrap();

        let stale_commit = store.commit_snapshot(&older).unwrap();

        assert_eq!(stale_commit.truncated_operations, 0);
        let restored = store.restore(&scope()).unwrap();
        assert_eq!(restored.snapshot_sequence, latest);
        assert_eq!(restored.document(), newer.snapshot().document());
    }

    #[test]
    fn append_sequence_continues_after_complete_log_compaction() {
        let mut store = SqliteStore::in_memory().unwrap();
        let first = ops(1, 5);
        let through = store.append(&scope(), &first).unwrap();
        store
            .commit_snapshot(&CapturedSnapshot::new(
                Snapshot::materialize(scope(), &first),
                through,
            ))
            .unwrap();
        assert_eq!(store.count(&scope()).unwrap(), 0);

        let next = store.append(&scope(), &ops(2, 1)).unwrap();

        assert_eq!(next, through + 1);
        assert_eq!(store.read_since(&scope(), through).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_snapshot_recovers_only_from_a_complete_log_and_preserves_evidence() {
        let mut store = SqliteStore::in_memory().unwrap();
        let operations = ops(1, 4);
        let through = store.append(&scope(), &operations).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO snapshots(scope, through, document) VALUES (?1, ?2, ?3)",
                rusqlite::params![scope().0, through, "{corrupt"],
            )
            .unwrap();

        let restored = store.restore(&scope()).unwrap();

        assert_eq!(
            restored.source,
            RestoreSource::CompleteLogAfterSnapshotCorruption
        );
        assert_eq!(restored.captured.through_sequence(), through);
        assert_eq!(restored.document().live_count(), operations.len());
        assert_eq!(store.raw_snapshot(&scope()).unwrap().unwrap().1, "{corrupt");
    }

    #[test]
    fn corrupt_compacted_snapshot_fails_closed_and_preserves_evidence() {
        let mut store = SqliteStore::in_memory().unwrap();
        let operations = ops(1, 4);
        let through = store.append(&scope(), &operations).unwrap();
        store
            .commit_snapshot(&CapturedSnapshot::new(
                Snapshot::materialize(scope(), &operations),
                through,
            ))
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE snapshots SET document = ?2 WHERE scope = ?1",
                rusqlite::params![scope().0, "{corrupt"],
            )
            .unwrap();

        assert!(store.restore(&scope()).is_err());
        assert_eq!(store.raw_snapshot(&scope()).unwrap().unwrap().1, "{corrupt");
        assert_eq!(store.count(&scope()).unwrap(), 0);
    }

    #[test]
    fn checkpoint_health_is_queryable() {
        let mut store = SqliteStore::in_memory().unwrap();
        store.append(&scope(), &ops(1, 1)).unwrap();

        let health = store.checkpoint_health().unwrap();

        assert_eq!(health.busy, 0);
        assert!(health.checkpointed_frames <= health.log_frames);
    }

    #[test]
    fn readiness_proves_read_and_write_transactions_without_data_changes() {
        let mut store = SqliteStore::in_memory().unwrap();
        store.append(&scope(), &ops(1, 1)).unwrap();
        let before = store.count(&scope()).unwrap();

        assert_eq!(
            store.readiness().unwrap(),
            ReadinessCheck {
                readable: true,
                writable: true
            }
        );
        assert_eq!(store.count(&scope()).unwrap(), before);
    }

    #[test]
    fn online_backup_is_integrity_checked_and_logically_restorable() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite3");
        let backup = directory.path().join("backup.sqlite3");
        let mut store = SqliteStore::open(&source).unwrap();
        let through = store.append(&scope(), &ops(1, 3)).unwrap();

        let report = store.online_backup(&backup).unwrap();
        let restored = SqliteStore::verify_restore(&backup).unwrap();

        assert!(report.bytes > 0);
        assert!(report.verification.integrity_ok);
        assert_eq!(report.verification.foreign_key_violations, 0);
        assert_eq!(restored.scopes, 1);
        assert_eq!(restored.restored_operations, through);
    }

    #[test]
    fn a_read_only_database_refuses_an_append_without_partial_state() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        drop(SqliteStore::open(&path).unwrap());
        let connection =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut store = SqliteStore { connection };

        assert!(store.append(&scope(), &ops(1, 1)).is_err());
        assert_eq!(store.count(&scope()).unwrap(), 0);
    }

    #[test]
    fn a_busy_database_refuses_an_append_without_partial_state() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let mut store = SqliteStore::open(&path).unwrap();
        store
            .connection
            .busy_timeout(Duration::from_millis(0))
            .unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        assert!(store.append(&scope(), &ops(1, 2)).is_err());
        assert_eq!(store.count(&scope()).unwrap(), 0);
        blocker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn a_full_database_refuses_an_append_without_partial_state() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let mut store = SqliteStore::open(&path).unwrap();
        let pages = store
            .connection
            .pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0))
            .unwrap();
        store
            .connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        let mut clock = HlcGenerator::new(ActorId(1));
        let large = upsert(
            ElementId(1),
            [(PropKey::Text, PropValue::Text("x".repeat(64 * 1024)))],
            &mut clock,
            1_000,
        );

        assert!(store.append(&scope(), &large).is_err());
        assert_eq!(store.count(&scope()).unwrap(), 0);
    }

    #[test]
    fn batch_retention_expires_identity_records_without_deleting_operations() {
        let mut store = SqliteStore::in_memory().unwrap();
        let old_hash = [1; 32];
        let new_hash = [2; 32];
        store
            .append_batch(BatchWrite {
                scope: &scope(),
                replica: "r",
                batch: "old",
                payload_hash: &old_hash,
                operations: &ops(1, 1),
                recorded_at_millis: 100,
                retain_after_millis: 0,
            })
            .unwrap();
        store
            .append_batch(BatchWrite {
                scope: &scope(),
                replica: "r",
                batch: "new",
                payload_hash: &new_hash,
                operations: &ops(2, 1),
                recorded_at_millis: 200,
                retain_after_millis: 150,
            })
            .unwrap();

        assert_eq!(
            store
                .batch_outcome(&scope(), "r", "old", &old_hash, 150)
                .unwrap(),
            None
        );
        assert!(matches!(
            store
                .batch_outcome(&scope(), "r", "new", &new_hash, 150)
                .unwrap(),
            Some(BatchAppend::Duplicate(_))
        ));
        assert_eq!(table_count(&store, "batches"), 1);
        assert_eq!(store.count(&scope()).unwrap(), 2);
    }

    #[test]
    fn tombstones_survive_snapshot_truncation_and_restore() {
        let mut store = SqliteStore::in_memory().unwrap();
        let mut operations = ops(1, 1);
        operations.push(delete_op(1, 0, 2_000));
        let through = store.append(&scope(), &operations).unwrap();
        let snapshot = Snapshot::materialize(scope(), &operations);
        assert_eq!(snapshot.document().live_count(), 0);
        assert_eq!(snapshot.document().total_count(), 1);

        store
            .commit_snapshot(&CapturedSnapshot::new(snapshot, through))
            .unwrap();
        let restored = store.restore(&scope()).unwrap();

        assert_eq!(store.count(&scope()).unwrap(), 0);
        assert_eq!(restored.document().live_count(), 0);
        assert_eq!(restored.document().total_count(), 1);
    }

    #[test]
    fn repeated_snapshot_cycles_bound_the_operation_log() {
        let mut store = SqliteStore::in_memory().unwrap();
        let mut all = Vec::new();
        let mut previous_sequence = 0;

        for actor in 1..=12 {
            let batch = ops(actor, 25);
            all.extend(batch.iter().cloned());
            let sequence = store.append(&scope(), &batch).unwrap();
            assert!(sequence > previous_sequence);
            store
                .commit_snapshot(&CapturedSnapshot::new(
                    Snapshot::materialize(scope(), &all),
                    sequence,
                ))
                .unwrap();
            assert_eq!(store.count(&scope()).unwrap(), 0);
            assert_eq!(table_count(&store, "snapshots"), 1);
            previous_sequence = sequence;
        }

        let restored = store.restore(&scope()).unwrap();
        assert_eq!(restored.captured.through_sequence(), previous_sequence);
        assert_eq!(
            restored.document(),
            Snapshot::materialize(scope(), &all).document()
        );
    }

    #[test]
    fn log_rows_remain_without_a_committed_snapshot() {
        let mut store = SqliteStore::in_memory().unwrap();
        store.append(&scope(), &ops(1, 4)).unwrap();

        // Nothing has absorbed these yet. Deleting them would be data loss.
        assert_eq!(store.count(&scope()).unwrap(), 4);
    }

    #[test]
    fn scopes_lists_everything_with_durable_state() {
        let mut store = SqliteStore::in_memory().unwrap();
        store.append(&ScopeId::new("t/a"), &ops(1, 1)).unwrap();
        store
            .store(&CapturedSnapshot::new(
                Snapshot::empty(ScopeId::new("t/b")),
                0,
            ))
            .unwrap();

        let scopes: Vec<String> = store.scopes().unwrap().into_iter().map(|s| s.0).collect();
        assert_eq!(scopes, vec!["t/a".to_owned(), "t/b".to_owned()]);
    }
}
