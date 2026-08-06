PRAGMA user_version = 2;

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
