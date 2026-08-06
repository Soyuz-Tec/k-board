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
