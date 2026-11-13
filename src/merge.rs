//! `subrosa merge`: copy the sessions and turns of another machine from a
//! plain snapshot into the live archive, so search and recall find them.

use std::collections::HashSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusqlite::types::Value;
use rusqlite::{params, Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::{backup, db, paths, redact, tags};

const BATCH: usize = 2_000;

pub fn run(file: PathBuf) -> ExitCode {
    match merge_file(&paths::expanduser(&file)) {
        Ok((sessions, turns, skipped)) => {
            println!(
                "[subrosa] merge: +{sessions} session(s), +{turns} turn(s), {skipped} skipped (archived on this machine)"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[subrosa] merge: {e}");
            ExitCode::from(1)
        }
    }
}

fn merge_file(file: &Path) -> Result<(i64, i64, i64), Box<dyn Error>> {
    if !backup::is_plaintext(file) {
        return Err(format!(
            "{} is not a SQLite snapshot. For an encrypted mirror, run `subrosa restore` first.",
            file.display()
        )
        .into());
    }
    // immutable=1 keeps SQLite from writing -wal and -shm files next to the
    // snapshot, which often sits in a synced folder.
    let uri = format!(
        "file:{}?immutable=1",
        file.to_string_lossy()
            .replace('%', "%25")
            .replace('?', "%3f")
            .replace('#', "%23")
    );
    let src = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    // The snapshot comes from another machine, so its schema is not trusted.
    src.execute_batch("PRAGMA trusted_schema=OFF;")?;
    let conn = db::connect()?;
    merge(&conn, &src)
}

/// Returns (sessions added, turns added, sessions skipped).
fn merge(conn: &Connection, src: &Connection) -> Result<(i64, i64, i64), Box<dyn Error>> {
    let version: i64 = src.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > db::SCHEMA_VERSION {
        return Err(format!(
            "snapshot schema version {version} is newer than this binary supports ({})",
            db::SCHEMA_VERSION
        )
        .into());
    }
    // A session this machine archived from a transcript is never touched. Old
    // archives hold such rows with scan_seq = 0, so file_path is the marker.
    // A forgotten id counts as local, so a merge never brings it back.
    let local: HashSet<String> = conn
        .prepare(
            "SELECT session_id FROM sessions WHERE file_path IS NOT NULL \
             UNION SELECT session_id FROM forgotten_sessions",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;

    let (mut sessions, mut turns, mut skipped) = (0i64, 0i64, 0i64);
    let mut keep: HashSet<String> = HashSet::new();
    {
        let mut stmt = src.prepare(
            "SELECT session_id, project, cwd FROM sessions s \
             WHERE EXISTS (SELECT 1 FROM turns t WHERE t.session_id = s.session_id)",
        )?;
        let mut rows = stmt.query([])?;
        // Immediate: take the write lock at BEGIN, where busy_timeout applies.
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO sessions(session_id, project, cwd, ingested_at) \
                 VALUES (?,?,?,?)",
            )?;
            while let Some(r) = rows.next()? {
                let sid: String = r.get(0)?;
                if local.contains(&sid) {
                    skipped += 1;
                    continue;
                }
                sessions += ins.execute(params![
                    sid,
                    r.get::<_, Value>(1)?,
                    r.get::<_, Value>(2)?,
                    db::now_iso()
                ])? as i64;
                keep.insert(sid);
            }
        }
        tx.commit()?;
    }

    {
        let mut stmt = src.prepare(
            "SELECT session_id, seq, uuid, ts, role, text, is_meta, is_sidechain, project, cwd \
             FROM turns ORDER BY id",
        )?;
        let mut rows = stmt.query([])?;
        let mut done = false;
        while !done {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            {
                // The guard skips a session that a hook archived here after
                // the merge started.
                let mut ins = tx.prepare(
                    "INSERT OR IGNORE INTO turns\
                     (session_id,seq,uuid,ts,role,text,is_meta,is_sidechain,project,cwd) \
                     SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10 WHERE EXISTS \
                     (SELECT 1 FROM sessions WHERE session_id=?1 AND file_path IS NULL)",
                )?;
                for _ in 0..BATCH {
                    let Some(r) = rows.next()? else {
                        done = true;
                        break;
                    };
                    let sid: String = r.get(0)?;
                    if !keep.contains(&sid) {
                        continue;
                    }
                    // The other machine may run an older redaction list.
                    let text: Option<String> = r.get(5)?;
                    let text = text.map(|t| redact::redact(&t).into_owned());
                    turns += ins.execute(params![
                        sid,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Value>(2)?,
                        r.get::<_, Value>(3)?,
                        r.get::<_, Value>(4)?,
                        text,
                        r.get::<_, Value>(6)?,
                        r.get::<_, Value>(7)?,
                        r.get::<_, Value>(8)?,
                        r.get::<_, Value>(9)?
                    ])? as i64;
                }
            }
            tx.commit()?;
        }
    }

    // Merged sessions have no transcript here, so their summary columns and
    // tags come from the stored turns.
    let stale: Vec<String> = conn
        .prepare(
            "SELECT session_id FROM sessions s WHERE file_path IS NULL AND COALESCE(num_turns, 0) != \
             (SELECT count(*) FROM turns t WHERE t.session_id = s.session_id)",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for sid in &stale {
        conn.execute(
            "UPDATE sessions SET (num_turns, last_seq, first_ts, last_ts) = \
             (SELECT count(*), COALESCE(max(seq), -1), min(ts), max(ts) FROM turns WHERE session_id=?1) \
             WHERE session_id=?1 AND file_path IS NULL",
            [sid],
        )?;
        if let Err(e) = tags::derive_tags(conn, sid) {
            eprintln!("[subrosa] tag derivation {sid}: {e}");
        }
    }
    Ok((sessions, turns, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest;
    use std::fs;

    // The v0.1.0 table shape (addbc37), which stamps user_version = 1.
    const V1_DDL: &str = r#"
PRAGMA journal_mode=WAL;
CREATE TABLE sessions (
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
CREATE TABLE turns (
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
CREATE VIRTUAL TABLE turns_fts USING fts5(
  text, content='turns', content_rowid='id', tokenize='unicode61'
);
CREATE TRIGGER turns_ai AFTER INSERT ON turns BEGIN
  INSERT INTO turns_fts(rowid, text) VALUES (new.id, new.text);
END;
PRAGMA user_version=1;
"#;

    struct Env {
        root: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Env {
        fn new(name: &str) -> Env {
            let guard = crate::paths::test_env_lock();
            let root =
                std::env::temp_dir().join(format!("subrosa-merge-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            std::env::set_var("SUBROSA_DIR", &root);
            Env {
                root,
                _guard: guard,
            }
        }

        fn open(&self, db: &str) -> Connection {
            std::env::set_var("SUBROSA_DB", self.root.join(db));
            db::connect().unwrap()
        }

        // The real write path: a transcript file through ingest_file.
        fn ingest(&self, db: &str, sid: &str, texts: &[&str]) -> PathBuf {
            let project = self.root.join("-tmp-demo");
            fs::create_dir_all(&project).unwrap();
            let lines: String = texts
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    format!(
                        "{{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:0{i}Z\",\"uuid\":\"{sid}{i}\",\"cwd\":\"/tmp/demo\",\"message\":{{\"role\":\"user\",\"content\":\"{t}\"}}}}\n"
                    )
                })
                .collect();
            let path = project.join(format!("{sid}.jsonl"));
            fs::write(&path, lines).unwrap();
            let conn = self.open(db);
            ingest::ingest_file(&conn, &path).unwrap();
            self.root.join(db)
        }

        fn live(&self) -> Connection {
            self.open("live.db")
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            std::env::remove_var("SUBROSA_DB");
            std::env::remove_var("SUBROSA_DIR");
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn dump(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        let n = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        while let Some(r) = rows.next().unwrap() {
            let cols: Vec<Value> = (0..n).map(|i| r.get(i).unwrap()).collect();
            out.push(format!("{cols:?}"));
        }
        out
    }

    fn hits(conn: &Connection, table: &str, word: &str) -> i64 {
        conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE {table} MATCH ?"),
            [format!("\"{word}\"")],
            |r| r.get(0),
        )
        .unwrap()
    }

    const COUNTS: &str = "SELECT (SELECT count(*) FROM sessions) * 1000000 + \
         (SELECT count(*) FROM turns) * 1000 + (SELECT count(*) FROM session_tags)";

    #[test]
    fn fresh_merge_lands_every_turn() {
        let env = Env::new("fresh");
        let snap = env.ingest(
            "snap.db",
            "F",
            &[
                "cache-prod rollout TICKET-123",
                "the wombatlodge note",
                "done",
            ],
        );
        let conn = env.live();
        assert_eq!(merge_file(&snap).unwrap(), (1, 3, 0));
        assert_eq!(count(&conn, "SELECT count(*) FROM turns"), 3);
        assert_eq!(hits(&conn, "turns_fts", "wombatlodge"), 1);
        assert_eq!(
            dump(
                &conn,
                "SELECT num_turns, last_seq, first_ts, last_ts, scan_seq, file_path, project \
                 FROM sessions WHERE session_id='F'"
            ),
            vec![
                "[Integer(3), Integer(2), Text(\"2026-01-01T00:00:00Z\"), \
                 Text(\"2026-01-01T00:00:02Z\"), Integer(0), Null, Text(\"-tmp-demo\")]"
            ]
        );
        assert!(
            count(
                &conn,
                "SELECT count(*) FROM session_tags WHERE session_id='F'"
            ) > 0
        );
    }

    #[test]
    fn mixed_state_keeps_local_rows_and_adds_only_new_turns() {
        let env = Env::new("mixed");
        // Session S arrives through an earlier merge, with 3 turns.
        let first = env.ingest("first.db", "S", &["alpha one", "zebrafinch two", "three"]);
        let conn = env.live();
        assert_eq!(merge_file(&first).unwrap(), (1, 3, 0));
        // Session L is archived here, with a queue row and a watermark.
        env.ingest("live.db", "L", &["local kestrelnest", "local two"]);
        assert_eq!(ingest::enqueue_checkpoint(&conn, "L").unwrap(), "queued");
        ingest::advance_checkpoint(&conn, "L", 0).unwrap();
        db::ensure_trigram_index(&conn).unwrap();
        db::ensure_embeddings_table(&conn).unwrap();
        conn.execute(
            "INSERT INTO turn_embeddings(turn_id, model, dim, vec) VALUES (1, 'm', 1, x'00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO facts(project, name, leaf_path) VALUES ('-tmp-demo', 'n', 'n.md')",
            [],
        )
        .unwrap();

        let snap = env.root.join("old.db");
        let old = Connection::open(&snap).unwrap();
        old.execute_batch(V1_DDL).unwrap();
        old.execute_batch(
            "INSERT INTO sessions(session_id, project, cwd) VALUES \
               ('S','-tmp-demo','/tmp/demo'), ('N','-tmp-demo','/tmp/demo'), \
               ('L','-tmp-demo','/tmp/demo'), ('E','-tmp-demo','/tmp/demo');
             INSERT INTO turns(session_id, seq, ts, role, text, project, cwd) VALUES \
               ('S',0,'2026-01-01T00:00:00Z','user','changed elsewhere','-tmp-demo','/tmp/demo'), \
               ('S',1,'2026-01-01T00:00:01Z','user','zebrafinch two','-tmp-demo','/tmp/demo'), \
               ('S',2,'2026-01-01T00:00:02Z','user','three','-tmp-demo','/tmp/demo'), \
               ('S',3,'2026-01-01T00:00:03Z','user','four','-tmp-demo','/tmp/demo'), \
               ('S',4,'2026-01-01T00:00:04Z','user','quokkaburrow five','-tmp-demo','/tmp/demo'), \
               ('N',0,'2026-01-02T00:00:00Z','user','new one','-tmp-demo','/tmp/demo'), \
               ('N',1,'2026-01-02T00:00:01Z','user','new two','-tmp-demo','/tmp/demo'), \
               ('L',0,'2026-01-03T00:00:00Z','user','remote ospreyperch','-tmp-demo','/tmp/demo'), \
               ('L',1,'2026-01-03T00:00:01Z','user','remote two','-tmp-demo','/tmp/demo'), \
               ('L',2,'2026-01-03T00:00:02Z','user','remote three','-tmp-demo','/tmp/demo');",
        )
        .unwrap();
        drop(old);

        let s_rows = "SELECT id, text FROM turns WHERE session_id='S' AND seq < 3 ORDER BY id";
        let l_session = "SELECT * FROM sessions WHERE session_id='L'";
        let l_turns = "SELECT * FROM turns WHERE session_id='L' ORDER BY id";
        let queue = "SELECT * FROM checkpoint_queue";
        let untouched = "SELECT (SELECT count(*) FROM checkpoint_queue) * 10000 + \
             (SELECT count(*) FROM facts) * 100 + (SELECT count(*) FROM turn_embeddings)";
        let before = [s_rows, l_session, l_turns, queue].map(|q| dump(&conn, q));
        assert_eq!(before[0].len(), 3);
        assert_eq!(before[2].len(), 2);
        assert_eq!(before[3].len(), 1);
        assert_eq!(count(&conn, untouched), 10101);

        assert_eq!(merge_file(&snap).unwrap(), (1, 4, 1));

        assert_eq!(
            [s_rows, l_session, l_turns, queue].map(|q| dump(&conn, q)),
            before
        );
        assert_eq!(count(&conn, untouched), 10101);
        assert_eq!(
            dump(
                &conn,
                "SELECT session_id, num_turns, last_seq, last_ts FROM sessions \
                 WHERE file_path IS NULL ORDER BY session_id"
            ),
            vec![
                "[Text(\"N\"), Integer(2), Integer(1), Text(\"2026-01-02T00:00:01Z\")]",
                "[Text(\"S\"), Integer(5), Integer(4), Text(\"2026-01-01T00:00:04Z\")]"
            ]
        );
        for table in ["turns_fts", "turns_fts_tri"] {
            assert_eq!(hits(&conn, table, "quokkaburrow"), 1, "{table} new turn");
            assert_eq!(hits(&conn, table, "zebrafinch"), 1, "{table} old turn");
            assert_eq!(
                hits(&conn, table, "ospreyperch"),
                0,
                "{table} local session"
            );
        }
        assert_eq!(
            count(&conn, "SELECT count(*) FROM sessions WHERE session_id='E'"),
            0
        );

        let counts = count(&conn, COUNTS);
        assert_eq!(merge_file(&snap).unwrap(), (0, 0, 1));
        assert_eq!(count(&conn, COUNTS), counts);
        assert_eq!(count(&conn, untouched), 10101);
    }

    #[test]
    fn old_local_session_with_zero_scan_seq_is_skipped() {
        let env = Env::new("oldlocal");
        let snap = env.ingest("snap.db", "O", &["remote cache-prod", "remote ibisroost"]);
        env.open("snap.db")
            .execute("UPDATE sessions SET checkpointed_seq=1", [])
            .unwrap();
        env.ingest("live.db", "O", &["local TICKET-123"]);
        let conn = env.live();
        // Old archives hold locally archived rows with scan_seq = 0.
        conn.execute("UPDATE sessions SET scan_seq=0, num_turns=9", [])
            .unwrap();
        let rows = [
            "SELECT * FROM sessions",
            "SELECT * FROM turns ORDER BY id",
            "SELECT * FROM session_tags ORDER BY tag",
        ];
        let before = rows.map(|q| dump(&conn, q));
        assert!(!before[2].is_empty());
        assert_eq!(merge_file(&snap).unwrap(), (0, 0, 1));
        assert_eq!(rows.map(|q| dump(&conn, q)), before);

        // A merged session keeps the default watermark, not the snapshot value.
        conn.execute_batch("DELETE FROM turns; DELETE FROM sessions;")
            .unwrap();
        assert_eq!(merge_file(&snap).unwrap(), (1, 2, 0));
        assert_eq!(count(&conn, "SELECT checkpointed_seq FROM sessions"), -1);
    }

    #[test]
    fn forgotten_session_stays_out_on_a_second_merge() {
        let env = Env::new("forgotten");
        let snap = env.ingest("snap.db", "G", &["remote otterhollow", "remote two"]);
        let conn = env.live();
        assert_eq!(merge_file(&snap).unwrap(), (1, 2, 0));
        // The same rows `forget` removes, then the id it records.
        conn.execute_batch(
            "DELETE FROM turns WHERE session_id='G';
             DELETE FROM session_tags WHERE session_id='G';
             DELETE FROM sessions WHERE session_id='G';
             INSERT INTO forgotten_sessions(session_id, forgotten_at) VALUES ('G', 'now');",
        )
        .unwrap();
        assert_eq!(merge_file(&snap).unwrap(), (0, 0, 1));
        assert_eq!(count(&conn, "SELECT count(*) FROM turns"), 0);
        assert_eq!(count(&conn, "SELECT count(*) FROM sessions"), 0);
        assert_eq!(hits(&conn, "turns_fts", "otterhollow"), 0);
    }

    #[test]
    fn newer_snapshot_is_refused_before_any_write() {
        let env = Env::new("newer");
        let snap = env.ingest("snap.db", "F", &["one"]);
        env.open("snap.db")
            .pragma_update(None, "user_version", db::SCHEMA_VERSION + 1)
            .unwrap();
        let conn = env.live();
        let counts = count(&conn, COUNTS);
        let error = merge_file(&snap).unwrap_err().to_string();
        assert!(
            error.contains(&format!("{}", db::SCHEMA_VERSION + 1))
                && error.contains(&format!("({})", db::SCHEMA_VERSION)),
            "{error}"
        );
        assert_eq!(count(&conn, COUNTS), counts);
    }

    #[test]
    fn snapshot_text_is_redacted_again() {
        let env = Env::new("redact");
        let snap = env.ingest("snap.db", "F", &["one"]);
        env.open("snap.db")
            .execute("UPDATE turns SET text='password=hunter2'", [])
            .unwrap();
        let conn = env.live();
        merge_file(&snap).unwrap();
        assert_eq!(
            dump(&conn, "SELECT text FROM turns"),
            vec!["[Text(\"password=\u{2039}redacted\u{203a}\")]"]
        );
    }

    #[test]
    fn encrypted_mirror_is_refused() {
        let env = Env::new("sealed");
        let conn = env.live();
        let counts = count(&conn, COUNTS);
        let sealed = env.root.join("subrosa-latest.db.enc");
        fs::write(&sealed, b"SUBROSA1 not a database at all").unwrap();
        let error = merge_file(&sealed).unwrap_err().to_string();
        assert!(error.contains("subrosa restore"), "{error}");
        assert_eq!(count(&conn, COUNTS), counts);
    }

    #[test]
    fn merge_leaves_no_sidecar_files_next_to_the_snapshot() {
        let env = Env::new("sidecar");
        let snap = env.ingest("snap dir#1/snap.db", "F", &["one", "two"]);
        let _conn = env.live();
        assert_eq!(merge_file(&snap).unwrap(), (1, 2, 0));
        let names: Vec<String> = fs::read_dir(snap.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["snap.db"]);
    }
}
