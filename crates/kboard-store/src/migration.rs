//! Ordered SQLite schema migrations.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use kboard_core::ports::PortError;

use crate::backend;

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

const MIGRATION_1: &str = "
CREATE TABLE operations (
  scope   TEXT    NOT NULL,
  seq     INTEGER NOT NULL,
  payload TEXT    NOT NULL,
  PRIMARY KEY (scope, seq)
) WITHOUT ROWID;

CREATE TABLE snapshots (
  scope    TEXT    NOT NULL PRIMARY KEY,
  through  INTEGER NOT NULL,
  document TEXT    NOT NULL
) WITHOUT ROWID;
";

const MIGRATION_2: &str = "
CREATE TABLE batches (
  scope        TEXT    NOT NULL,
  replica      TEXT    NOT NULL,
  batch        TEXT    NOT NULL,
  payload_hash BLOB    NOT NULL,
  sequence     INTEGER NOT NULL,
  recorded_at  INTEGER NOT NULL,
  PRIMARY KEY (scope, replica, batch)
) WITHOUT ROWID;

CREATE INDEX batches_recorded_at ON batches(recorded_at);
";

pub fn migrate(connection: &mut Connection) -> Result<(), PortError> {
    adopt_legacy_schema(connection)?;
    let current = schema_version(connection)?;
    if current > CURRENT_SCHEMA_VERSION {
        return Err(PortError::Backend(format!(
            "database schema version {current} is newer than supported version {CURRENT_SCHEMA_VERSION}"
        )));
    }

    for version in (current + 1)..=CURRENT_SCHEMA_VERSION {
        let sql = match version {
            1 => MIGRATION_1,
            2 => MIGRATION_2,
            _ => unreachable!(),
        };
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        transaction.execute_batch(sql).map_err(backend)?;
        transaction
            .pragma_update(None, "user_version", version)
            .map_err(backend)?;
        transaction.commit().map_err(backend)?;
    }
    Ok(())
}

pub fn schema_version(connection: &Connection) -> Result<u32, PortError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .map_err(backend)
}

fn adopt_legacy_schema(connection: &Connection) -> Result<(), PortError> {
    if schema_version(connection)? != 0 {
        return Ok(());
    }
    let operations = table_exists(connection, "operations")?;
    let snapshots = table_exists(connection, "snapshots")?;
    let batches = table_exists(connection, "batches")?;
    let adopted = match (operations, snapshots, batches) {
        (false, false, false) => 0,
        (true, true, false) => 1,
        (true, true, true) => 2,
        _ => {
            return Err(PortError::Backend(
                "unversioned database has an unsupported partial schema".to_owned(),
            ));
        }
    };
    if adopted > 0 {
        connection
            .pragma_update(None, "user_version", adopted)
            .map_err(backend)?;
    }
    Ok(())
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool, PortError> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |_| Ok(true),
        )
        .optional()
        .map(|found| found.unwrap_or(false))
        .map_err(backend)
}
