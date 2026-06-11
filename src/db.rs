//! Schema and connection handling. One SQLite file kept local (never in a
//! synced folder) so the WAL/SHM sidecars don't corrupt under cloud sync.

use std::fs;
use std::time::Duration;

use rusqlite::Connection;

use crate::paths;

// The DDL matches the original Python implementation byte-for-byte where it
// matters: a DB created by either implementation works with the other.
const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS sessions (
  session_id  TEXT PRIMARY KEY,
  file_path   TEXT,
  project     TEXT,
  cwd         TEXT,
  first_ts    TEXT,
  last_ts     TEXT,
  num_turns   INTEGER DEFAULT 0,
  last_seq    INTEGER DEFAULT -1,
  file_size   INTEGER DEFAULT 0,
  checkpointed_seq INTEGER DEFAULT -1,
  ingested_at TEXT
);

CREATE TABLE IF NOT EXISTS turns (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id   TEXT NOT NULL,
  seq          INTEGER NOT NULL,
  uuid         TEXT,
  ts           TEXT,
  role         TEXT,
  text         TEXT,
  is_meta      INTEGER DEFAULT 0,
  is_sidechain INTEGER DEFAULT 0,
  project      TEXT,
  cwd          TEXT,
  UNIQUE(session_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_turns_ts      ON turns(ts);
CREATE INDEX IF NOT EXISTS idx_turns_project ON turns(project);

-- External-content FTS5 (the turns table is the source of truth; FTS holds only the index).
CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
  text, content='turns', content_rowid='id', tokenize='unicode61'
);

-- Keep the FTS index in sync. On INSERT OR IGNORE conflicts the AFTER INSERT trigger does
-- not fire (no row inserted), so re-ingesting a transcript never duplicates the index.
CREATE TRIGGER IF NOT EXISTS turns_ai AFTER INSERT ON turns BEGIN
  INSERT INTO turns_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS turns_ad AFTER DELETE ON turns BEGIN
  INSERT INTO turns_fts(turns_fts, rowid, text) VALUES('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS turns_au AFTER UPDATE ON turns BEGIN
  INSERT INTO turns_fts(turns_fts, rowid, text) VALUES('delete', old.id, old.text);
  INSERT INTO turns_fts(rowid, text) VALUES (new.id, new.text);
END;

-- Curated facts. MEMORY.md is generated from a byte-budgeted view of this table, so the
-- always-loaded index can never overflow. Corrections soft-delete (superseded_at/status), never rm.
CREATE TABLE IF NOT EXISTS facts (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  project       TEXT NOT NULL,
  name          TEXT,
  type          TEXT,
  title         TEXT,
  hook          TEXT,
  leaf_path     TEXT,
  description   TEXT,
  tags          TEXT,
  index_seq     INTEGER,
  pinned        INTEGER DEFAULT 0,
  status        TEXT DEFAULT 'active',
  hits          INTEGER DEFAULT 0,
  created_at    TEXT,
  updated_at    TEXT,
  last_used_at  TEXT,
  superseded_at TEXT,
  superseded_by INTEGER,
  origin_session TEXT,
  UNIQUE(project, leaf_path)
);
CREATE INDEX IF NOT EXISTS idx_facts_project ON facts(project, status, superseded_at);

CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(
  title, hook, description, content='facts', content_rowid='id', tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS facts_ai AFTER INSERT ON facts BEGIN
  INSERT INTO facts_fts(rowid, title, hook, description) VALUES (new.id, new.title, new.hook, new.description);
END;
CREATE TRIGGER IF NOT EXISTS facts_ad AFTER DELETE ON facts BEGIN
  INSERT INTO facts_fts(facts_fts, rowid, title, hook, description) VALUES('delete', old.id, old.title, old.hook, old.description);
END;
CREATE TRIGGER IF NOT EXISTS facts_au AFTER UPDATE ON facts BEGIN
  INSERT INTO facts_fts(facts_fts, rowid, title, hook, description) VALUES('delete', old.id, old.title, old.hook, old.description);
  INSERT INTO facts_fts(rowid, title, hook, description) VALUES (new.id, new.title, new.hook, new.description);
END;
"#;

/// Open the DB read-write: creates the schema, tightens perms, applies migrations.
pub fn connect() -> rusqlite::Result<Connection> {
    let p = paths::db_path();
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
        // The DB holds transcript text (can include pasted secrets). Owner-only; the 0700 dir
        // also covers the -wal/-shm sidecars and the hook log.
        chmod(parent, 0o700);
    }
    let conn = Connection::open(&p)?;
    chmod(&p, 0o600);
    conn.busy_timeout(Duration::from_millis(30_000))?;
    conn.execute_batch(SCHEMA)?;
    migrate(&conn)?;
    conn.pragma_update(None, "user_version", 1)?;
    Ok(conn)
}

#[cfg(unix)]
fn chmod(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn chmod(_path: &std::path::Path, _mode: u32) {}

/// Idempotent column adds for DBs created before a column existed.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    if !cols.iter().any(|c| c == "checkpointed_seq") {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN checkpointed_seq INTEGER DEFAULT -1",
            [],
        )?;
    }
    Ok(())
}

/// ISO-8601 UTC timestamp with seconds precision, e.g. 2026-06-12T06:20:02+00:00.
/// Hand-rolled because std has no date formatting and we keep the dep tree small.
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}+00:00")
}

// Howard Hinnant's days-to-civil algorithm (public domain).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
