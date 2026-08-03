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

use std::path::Path;

use kboard_core::document::ScopeId;
use kboard_core::op::StampedOp;
use kboard_core::ports::{OpLog, PortError, SnapshotStore};
use kboard_core::snapshot::Snapshot;
use rusqlite::{Connection, OptionalExtension};

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

    fn prepare(connection: Connection) -> Result<Self, PortError> {
        // WAL so a reader never blocks the writer: joins read while edits are
        // still committing. NORMAL rather than FULL because losing the last few
        // milliseconds of drawing to an OS crash is an acceptable trade for not
        // fsyncing on every stroke - and the operation log is idempotent, so a
        // client that retries closes the gap.
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;

                 CREATE TABLE IF NOT EXISTS operations (
                   scope     TEXT    NOT NULL,
                   seq       INTEGER NOT NULL,
                   payload   TEXT    NOT NULL,
                   PRIMARY KEY (scope, seq)
                 ) WITHOUT ROWID;

                 CREATE TABLE IF NOT EXISTS snapshots (
                   scope     TEXT    NOT NULL PRIMARY KEY,
                   through   INTEGER NOT NULL,
                   document  TEXT    NOT NULL
                 ) WITHOUT ROWID;",
            )
            .map_err(backend)?;
        Ok(Self { connection })
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
    pub fn restore(&self, scope: &ScopeId) -> Result<Snapshot, PortError> {
        let mut snapshot = self
            .load(scope)?
            .unwrap_or_else(|| Snapshot::empty(scope.clone()));
        // Zero when no snapshot row exists, which reads the whole log — the
        // correct behaviour for a board that has never been compacted.
        let after = self.through_seq(scope)?;
        let tail = self.read_since(scope, after)?;
        snapshot.absorb(&tail);
        Ok(snapshot)
    }

    /// Drop operations a stored snapshot has already absorbed.
    ///
    /// Deliberately explicit rather than automatic. Truncation is a retention
    /// decision (ADR-0005), and the engine cannot know whether a host is
    /// required to keep the history it is about to delete.
    ///
    /// # Errors
    ///
    /// [`PortError::Backend`] on a delete failure.
    pub fn truncate_absorbed(&mut self, scope: &ScopeId) -> Result<usize, PortError> {
        let Some(through) = self.stored_through(scope)? else {
            return Ok(0);
        };
        self.connection
            .execute(
                "DELETE FROM operations WHERE scope = ?1 AND seq <= ?2",
                rusqlite::params![scope.0, through],
            )
            .map_err(backend)
    }

    fn stored_through(&self, scope: &ScopeId) -> Result<Option<i64>, PortError> {
        self.connection
            .query_row(
                "SELECT through FROM snapshots WHERE scope = ?1",
                rusqlite::params![scope.0],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(backend)
    }

    fn through_seq(&self, scope: &ScopeId) -> Result<u64, PortError> {
        Ok(self.stored_through(scope)?.unwrap_or(0).max(0) as u64)
    }

    fn next_seq(&self, scope: &ScopeId) -> Result<i64, PortError> {
        self.connection
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM operations WHERE scope = ?1",
                rusqlite::params![scope.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(backend)
    }
}

impl OpLog for SqliteStore {
    fn append(&mut self, scope: &ScopeId, ops: &[StampedOp]) -> Result<u64, PortError> {
        if ops.is_empty() {
            return Ok(self.next_seq(scope)? as u64);
        }

        // One transaction for the batch. A partially written batch would leave
        // the log describing a state no replica ever held.
        let transaction = self.connection.transaction().map_err(backend)?;
        let mut seq = transaction
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM operations WHERE scope = ?1",
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
    fn load(&self, scope: &ScopeId) -> Result<Option<Snapshot>, PortError> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT document FROM snapshots WHERE scope = ?1",
                rusqlite::params![scope.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;

        stored
            .map(|document| {
                serde_json::from_str(&document)
                    .map_err(|error| PortError::Backend(error.to_string()))
            })
            .transpose()
    }

    fn store(&mut self, scope: &ScopeId, snapshot: &Snapshot) -> Result<(), PortError> {
        // Refusing a mismatched scope here as well as in the engine: a store is
        // the last place a routing bug can be caught before it becomes one
        // tenant's board filed under another's key.
        if snapshot.scope() != scope {
            return Err(PortError::Backend("snapshot scope mismatch".to_owned()));
        }

        let document = serde_json::to_string(snapshot)
            .map_err(|error| PortError::Backend(error.to_string()))?;
        // Records the highest log sequence written so far, which is what this
        // snapshot covers. The contract is that a caller appends the operations
        // first and stores the snapshot second; storing first would record a
        // sequence the snapshot has not actually absorbed and would make
        // `truncate_absorbed` delete live history.
        let through = self.next_seq(scope)?;

        self.connection
            .execute(
                "INSERT INTO snapshots (scope, through, document) VALUES (?1, ?2, ?3)
                 ON CONFLICT(scope) DO UPDATE SET through = excluded.through,
                                                  document = excluded.document",
                rusqlite::params![scope.0, through, document],
            )
            .map_err(backend)?;
        Ok(())
    }
}

fn backend(error: impl std::fmt::Display) -> PortError {
    PortError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kboard_core::clock::{ActorId, HlcGenerator};
    use kboard_core::element::ElementId;
    use kboard_core::op::upsert;
    use kboard_core::prop::{PropKey, PropValue};

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

        store.append(&scope(), head).unwrap();
        let mut folded = Snapshot::materialize(scope(), head);
        store.store(&scope(), &folded).unwrap();
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
    fn a_mismatched_snapshot_scope_is_refused() {
        let mut store = SqliteStore::in_memory().unwrap();
        let snapshot = Snapshot::empty(ScopeId::new("tenant-a/board"));
        assert!(store
            .store(&ScopeId::new("tenant-b/board"), &snapshot)
            .is_err());
    }

    #[test]
    fn truncation_keeps_the_restored_document_identical() {
        let mut store = SqliteStore::in_memory().unwrap();
        let all = ops(1, 8);
        store.append(&scope(), &all).unwrap();
        let snapshot = Snapshot::materialize(scope(), &all);
        store.store(&scope(), &snapshot).unwrap();

        let before = store.restore(&scope()).unwrap();
        let removed = store.truncate_absorbed(&scope()).unwrap();

        assert!(removed > 0, "absorbed operations should be collectable");
        assert_eq!(
            store.restore(&scope()).unwrap().document(),
            before.document(),
            "truncating what a snapshot already holds must change nothing"
        );
    }

    #[test]
    fn truncation_without_a_snapshot_removes_nothing() {
        let mut store = SqliteStore::in_memory().unwrap();
        store.append(&scope(), &ops(1, 4)).unwrap();

        // Nothing has absorbed these yet. Deleting them would be data loss.
        assert_eq!(store.truncate_absorbed(&scope()).unwrap(), 0);
        assert_eq!(store.count(&scope()).unwrap(), 4);
    }

    #[test]
    fn scopes_lists_everything_with_durable_state() {
        let mut store = SqliteStore::in_memory().unwrap();
        store.append(&ScopeId::new("t/a"), &ops(1, 1)).unwrap();
        store
            .store(&ScopeId::new("t/b"), &Snapshot::empty(ScopeId::new("t/b")))
            .unwrap();

        let scopes: Vec<String> = store.scopes().unwrap().into_iter().map(|s| s.0).collect();
        assert_eq!(scopes, vec!["t/a".to_owned(), "t/b".to_owned()]);
    }
}
