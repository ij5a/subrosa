//! Schema and connection handling. One SQLite file kept local (never in a
//! synced folder) so the WAL/SHM sidecars don't corrupt under cloud sync.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::paths;

// The DDL is compatibility-critical: archives created by any past version
// must keep working byte-for-byte, so schema changes go through migrate() only.
// v2: porter-stemmed FTS (word forms match; identifiers pass through unchanged).
// v3: session_tags (auto-derived, read-only tags) + a one-time backfill.
// v4: scan_offset/scan_seq resume cursor on sessions — incremental transcript ingest.
const SCHEMA_VERSION: i64 = 4;
const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;

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
  scan_offset INTEGER NOT NULL DEFAULT 0,
  scan_seq    INTEGER NOT NULL DEFAULT 0,
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
  text, content='turns', content_rowid='id', tokenize='porter unicode61'
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
  title, hook, description, content='facts', content_rowid='id', tokenize='porter unicode61'
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

-- Auto-derived, read-only session tags (ns = 'tool' | 'ext' | 'topic'). `tag` holds
-- the whole "ns:value" for cheap = / LIKE filtering; `rank` is per-(session,ns) order,
-- 0 = strongest. Unrelated to the freeform facts.tags column. Recomputed per session
-- on ingest (delete-and-recompute), so no FK is needed for referential cleanup.
CREATE TABLE IF NOT EXISTS session_tags (
  session_id TEXT NOT NULL,
  ns         TEXT NOT NULL,
  tag        TEXT NOT NULL,
  rank       INTEGER NOT NULL,
  PRIMARY KEY (session_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_session_tags_tag ON session_tags(tag, session_id);
CREATE INDEX IF NOT EXISTS idx_sessions_last_ts ON sessions(last_ts);
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
    // Connection-local; the persistent WAL switch lives in the init batch.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    tune(&conn)?;
    // user_version current → everyday connects are pure reads. Creation and
    // upgrades retry: WAL conversion can BUSY right past the busy handler.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v == SCHEMA_VERSION {
            return Ok(conn);
        }
        match init_schema(&conn) {
            Ok(()) => return Ok(conn),
            Err(e) if is_busy(&e) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    migrate(conn)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::DatabaseBusy
                || f.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

/// Open the DB read-only — the fast path for per-prompt lookups that must not
/// write or contend for the write lock. Errors if the DB doesn't exist yet.
pub fn connect_readonly() -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        paths::db_path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    tune(&conn)?;
    Ok(conn)
}

/// Connection-local read-path tuning. mmap skips read() syscalls for FTS page
/// access (clamped to file size; the live DB is always on local disk), and
/// in-memory temp keeps the bm25 sort off the filesystem. Batch because
/// `PRAGMA mmap_size=` returns a row, which execute_batch discards.
fn tune(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA mmap_size=268435456; PRAGMA temp_store=MEMORY;")
}

/// Mirror Claude Code's projects-dir naming: every non-alphanumeric char becomes a dash.
pub fn encode_cwd(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The current project's memory/ dir, derived from the cwd — the default for
/// `subrosa fact` and `subrosa generate` so facts land in the project you're in.
/// The cwd can arrive via a symlink while Claude Code names the projects dir
/// after the resolved path, so try raw + resolved cwd (plus a sessions-table
/// match, which holds Claude Code's own encoding) and prefer a dir that exists.
pub fn current_memdir(conn: Option<&Connection>) -> PathBuf {
    let mut raws: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("PWD") {
        if !p.is_empty() {
            raws.push(p);
        }
    }
    if let Ok(p) = std::env::current_dir() {
        raws.push(p.to_string_lossy().into_owned());
    }
    let mut cwds: Vec<String> = Vec::new();
    for p in &raws {
        let resolved = fs::canonicalize(p)
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.clone());
        for q in [p.clone(), resolved] {
            if !cwds.contains(&q) {
                cwds.push(q);
            }
        }
    }
    let mut candidates: Vec<String> = Vec::new();
    if let Some(conn) = conn {
        // A sessions-table hit is authoritative — it's Claude Code's own encoding.
        for cw in &cwds {
            let hit: Option<Option<String>> = conn
                .query_row(
                    "SELECT project FROM sessions WHERE cwd=? ORDER BY last_ts DESC LIMIT 1",
                    [cw],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None);
            if let Some(Some(p)) = hit {
                if !p.is_empty() {
                    candidates.push(p);
                }
            }
        }
    }
    candidates.extend(cwds.iter().map(|c| encode_cwd(c)));
    let projects = paths::projects_dir();
    for enc in &candidates {
        let d = projects.join(enc).join("memory");
        if d.exists() {
            return d;
        }
    }
    let enc = candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| encode_cwd(raws.first().map(String::as_str).unwrap_or("")));
    projects.join(enc).join("memory")
}

#[cfg(unix)]
fn chmod(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn chmod(_path: &std::path::Path, _mode: u32) {}

/// Idempotent upgrades for DBs created by an older version: column adds and
/// the v2 stemmed-FTS rebuild.
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
    // v4: byte/line resume cursor for incremental ingest. Existing rows default to
    // (0, 0), so the first ingest after the upgrade re-reads from the top and resets
    // the cursor — no special backfill needed.
    if !cols.iter().any(|c| c == "scan_offset") {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN scan_offset INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !cols.iter().any(|c| c == "scan_seq") {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN scan_seq INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    // v2: swap both FTS indexes to the stemmed tokenizer. The CREATE statements
    // must match SCHEMA above (minus IF NOT EXISTS).
    upgrade_fts(
        conn,
        "turns_fts",
        "CREATE VIRTUAL TABLE turns_fts USING fts5(\
           text, content='turns', content_rowid='id', tokenize='porter unicode61');",
    )?;
    upgrade_fts(
        conn,
        "facts_fts",
        "CREATE VIRTUAL TABLE facts_fts USING fts5(\
           title, hook, description, content='facts', content_rowid='id', \
           tokenize='porter unicode61');",
    )?;
    // v3: derive tags for sessions that have none yet. Idempotent and resumable —
    // user_version only reaches 3 once this returns Ok, so an interrupted backfill
    // picks up the remainder on the next connect.
    crate::tags::backfill(conn)?;
    Ok(())
}

/// Rebuild an external-content FTS index whose tokenizer predates porter, in
/// one write transaction — the source table is the truth, the index is
/// disposable (~220ms at 50k turns). Crash mid-way rolls back to the old index.
fn upgrade_fts(conn: &Connection, table: &str, create_sql: &str) -> rusqlite::Result<()> {
    let ddl: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?",
            [table],
            |r| r.get(0),
        )
        .optional()?;
    let current = match ddl {
        Some(s) => s.contains("porter"),
        None => true, // absent: SCHEMA just created it with the current tokenizer
    };
    if current {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;\n\
         DROP TABLE {table};\n\
         {create_sql}\n\
         INSERT INTO {table}({table}) VALUES('rebuild');\n\
         COMMIT;"
    ))
}

/// Build the opt-in trigram FTS index on first `--fuzzy` search, plus triggers to keep it in
/// sync. Created outside the versioned schema so users who never run `--fuzzy` pay no storage or
/// ingest cost; recall never uses it (the porter table backs the per-prompt gate). Needs a
/// read-write connection. Returns true if it built the index this call (one-time, ~seconds on a
/// big archive).
pub fn ensure_trigram_index(conn: &Connection) -> rusqlite::Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='turns_fts_tri'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(false);
    }
    conn.execute_batch(
        "BEGIN IMMEDIATE;\n\
         CREATE VIRTUAL TABLE turns_fts_tri USING fts5(\
           text, content='turns', content_rowid='id', tokenize='trigram');\n\
         CREATE TRIGGER turns_tri_ai AFTER INSERT ON turns BEGIN\n\
           INSERT INTO turns_fts_tri(rowid, text) VALUES (new.id, new.text);\n\
         END;\n\
         CREATE TRIGGER turns_tri_ad AFTER DELETE ON turns BEGIN\n\
           INSERT INTO turns_fts_tri(turns_fts_tri, rowid, text) VALUES('delete', old.id, old.text);\n\
         END;\n\
         CREATE TRIGGER turns_tri_au AFTER UPDATE ON turns BEGIN\n\
           INSERT INTO turns_fts_tri(turns_fts_tri, rowid, text) VALUES('delete', old.id, old.text);\n\
           INSERT INTO turns_fts_tri(rowid, text) VALUES (new.id, new.text);\n\
         END;\n\
         INSERT INTO turns_fts_tri(turns_fts_tri) VALUES('rebuild');\n\
         COMMIT;",
    )?;
    Ok(true)
}

/// Create the opt-in embeddings store on first `subrosa embed`. Outside the
/// versioned schema, same as the trigram index: nobody who skips semantic
/// search pays for it, and SCHEMA_VERSION stays put. `model` is part of the key
/// so switching models is a fresh backfill, never a migration. Needs a
/// read-write connection.
pub fn ensure_embeddings_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS turn_embeddings (\
           turn_id INTEGER NOT NULL,\
           model   TEXT NOT NULL,\
           dim     INTEGER NOT NULL,\
           vec     BLOB NOT NULL,\
           PRIMARY KEY (turn_id, model)\
         );",
    )
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
    let (y, mo, d) = crate::timeutil::civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}+00:00")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pre-v2 shape of the turns side: unstemmed FTS plus the insert trigger.
    const V1_DDL: &str = r#"
CREATE TABLE turns (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id   TEXT NOT NULL,
  seq          INTEGER NOT NULL,
  uuid TEXT, ts TEXT, role TEXT, text TEXT,
  is_meta INTEGER DEFAULT 0, is_sidechain INTEGER DEFAULT 0,
  project TEXT, cwd TEXT,
  UNIQUE(session_id, seq)
);
CREATE VIRTUAL TABLE turns_fts USING fts5(
  text, content='turns', content_rowid='id', tokenize='unicode61'
);
CREATE TRIGGER turns_ai AFTER INSERT ON turns BEGIN
  INSERT INTO turns_fts(rowid, text) VALUES (new.id, new.text);
END;
"#;

    fn count_match(conn: &Connection, term: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM turns_fts WHERE turns_fts MATCH ?",
            [format!("\"{term}\"")],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn migrate_rebuilds_fts_with_porter() {
        let p = std::env::temp_dir().join(format!("subrosa-mig-{}.db", std::process::id()));
        let _ = fs::remove_file(&p);
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(V1_DDL).unwrap();
        conn.execute(
            "INSERT INTO turns(session_id, seq, text) VALUES('s1', 0, 'we deployed the cache service')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        assert_eq!(count_match(&conn, "deploy"), 0, "v1 index must not stem");

        init_schema(&conn).unwrap();

        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let ddl: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='turns_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(ddl.contains("porter"), "tokenizer not upgraded: {ddl}");
        assert_eq!(
            count_match(&conn, "deploy"),
            1,
            "stemmed match after migration"
        );

        // The untouched triggers still sync the rebuilt index.
        conn.execute(
            "INSERT INTO turns(session_id, seq, text) VALUES('s1', 1, 'deploying again tomorrow')",
            [],
        )
        .unwrap();
        assert_eq!(count_match(&conn, "deploy"), 2);

        // Re-running is a no-op (porter already present → no rebuild path).
        init_schema(&conn).unwrap();
        assert_eq!(count_match(&conn, "deploy"), 2);
        drop(conn);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn migrate_v3_backfills_session_tags() {
        let p = std::env::temp_dir().join(format!("subrosa-tagmig-{}.db", std::process::id()));
        let _ = fs::remove_file(&p);
        let conn = Connection::open(&p).unwrap();
        // Seed a session + turn, then pretend the archive predates v3 (no tags yet).
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("INSERT INTO sessions(session_id) VALUES('s1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO turns(session_id, seq, text) \
             VALUES('s1', 0, '\u{2699} Bash ran kubectl, cache-prod rollout TICKET-123')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        let n0: i64 = conn
            .query_row("SELECT count(*) FROM session_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n0, 0, "no tags before the upgrade");

        init_schema(&conn).unwrap();

        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION, "version bumped to current");
        let total: i64 = conn
            .query_row(
                "SELECT count(*) FROM session_tags WHERE session_id='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(total > 0, "backfill derived tags");
        let bash: i64 = conn
            .query_row(
                "SELECT count(*) FROM session_tags WHERE session_id='s1' AND tag='tool:bash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bash, 1, "tool:bash derived from the marker");

        // Resumable + idempotent: a second pass changes nothing (s1 already tagged).
        init_schema(&conn).unwrap();
        let total2: i64 = conn
            .query_row(
                "SELECT count(*) FROM session_tags WHERE session_id='s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, total2, "re-running backfill is a no-op");
        drop(conn);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn trigram_index_substring_matches_and_stays_synced() {
        let p = std::env::temp_dir().join(format!("subrosa-tri-{}.db", std::process::id()));
        let _ = fs::remove_file(&p);
        let conn = Connection::open(&p).unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO turns(session_id, seq, text) VALUES('s1', 0, 'pgbouncer kept pinning the writer')",
            [],
        )
        .unwrap();

        // Built on demand, idempotent.
        assert!(ensure_trigram_index(&conn).unwrap(), "first call builds");
        assert!(
            !ensure_trigram_index(&conn).unwrap(),
            "second call is a no-op"
        );

        // Porter (exact) can't match the substring 'bouncer'; trigram (fuzzy) can — the whole point.
        let q = "\"bouncer\"";
        let porter: i64 = conn
            .query_row(
                "SELECT count(*) FROM turns_fts WHERE turns_fts MATCH ?",
                [q],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(porter, 0, "porter must not substring-match");
        let tri: i64 = conn
            .query_row(
                "SELECT count(*) FROM turns_fts_tri WHERE turns_fts_tri MATCH ?",
                [q],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tri, 1, "trigram substring-matches pgbouncer");

        // The triggers keep the trigram index synced with later inserts.
        conn.execute(
            "INSERT INTO turns(session_id, seq, text) VALUES('s1', 1, 'another pgbouncer note')",
            [],
        )
        .unwrap();
        let tri2: i64 = conn
            .query_row(
                "SELECT count(*) FROM turns_fts_tri WHERE turns_fts_tri MATCH ?",
                [q],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            tri2, 2,
            "trigger synced the new turn into the trigram index"
        );

        drop(conn);
        let _ = fs::remove_file(&p);
    }

    /// The first-ever call races the hooks: the background indexer creates this
    /// table while a session is still writing turns. It works because the call
    /// arrives in autocommit — `connect()` leaves no transaction open — so the
    /// create is a write of its own and the busy handler waits the other writer
    /// out. What must never happen is calling this from inside an already-open
    /// read transaction: promoting one comes back SQLITE_BUSY at once, with
    /// busy_timeout never getting a say. This test is the guard on that.
    /// Only the first call writes at all; once the table is there it's a read.
    #[test]
    fn creating_the_embeddings_table_waits_for_a_writer_instead_of_failing() {
        let p = std::env::temp_dir().join(format!("subrosa-emb-busy-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", p.display()));
        }
        let conn = Connection::open(&p).unwrap();
        init_schema(&conn).unwrap();
        conn.busy_timeout(Duration::from_secs(10)).unwrap();

        // A second connection holding an open write transaction, exactly like a
        // hook mid-ingest.
        let holder = Connection::open(&p).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        holder
            .execute(
                "INSERT INTO turns(session_id, seq, text) VALUES('s1', 0, 'mid-ingest')",
                [],
            )
            .unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            holder.execute_batch("COMMIT").unwrap();
        });

        ensure_embeddings_table(&conn).expect("must wait out the writer, not return BUSY");
        writer.join().unwrap();
        // Idempotent, and from here on it's the cheap read path.
        ensure_embeddings_table(&conn).unwrap();

        drop(conn);
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(format!("{}{suffix}", p.display()));
        }
    }
}
