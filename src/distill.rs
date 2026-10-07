//! Opt-in checkpoint draining. The child receives only redacted archived text.

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::{db, embed, ingest, paths};

const MAX_SESSIONS: i64 = 3;
const RETRY_SECS: i64 = 3600;
const LIVE_SECS: u64 = 600;

pub fn run(auto: bool, ended: Option<&str>) -> std::process::ExitCode {
    if !auto {
        eprintln!("[subrosa] distill requires --auto");
        return std::process::ExitCode::FAILURE;
    }
    match run_worker(ended) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            crate::hook::log(&format!("distill worker error: {e}"));
            eprintln!("[subrosa] distill failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

pub fn spawn_if_due(ended: Option<&str>) {
    let path = match paths::distill_path() {
        Ok(Some(path)) => path,
        Ok(None) => return,
        Err(e) => {
            crate::hook::log(&format!("distill config error: {e}"));
            return;
        }
    };
    if !path.is_absolute() {
        crate::hook::log("distill config path is not absolute");
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut argv = vec![exe.as_os_str(), OsStr::new("distill"), OsStr::new("--auto")];
    if let Some(sid) = ended {
        argv.extend([OsStr::new("--ended"), OsStr::new(sid)]);
    }
    if let Err(e) = embed::detach(&argv) {
        crate::hook::log(&format!("distill spawn failed: {e}"));
    }
}

fn run_worker(ended: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let Some(program) = paths::distill_path()? else {
        return Ok(());
    };
    if retry_pending() {
        return Ok(());
    }
    let failures = record_attempt();
    let Some(lock) = run_lock()? else {
        return Ok(());
    };
    let conn = db::connect_with_timeout(Duration::from_secs(10))?;
    let rows = queued_rows(&conn)?;
    let mut failed = false;
    let mut runs = 0;
    for (sid, path, incomplete) in rows {
        if runs >= MAX_SESSIONS {
            break;
        }
        if ended != Some(sid.as_str()) && is_live(path.as_deref()) {
            continue;
        }
        if incomplete != 0 {
            crate::hook::log(&format!(
                "distill {sid} deferred: session archive is incomplete"
            ));
            continue;
        }
        runs += 1;
        let result = distill_one(&program, &sid);
        match result {
            Ok(text) => {
                crate::hook::log(&format!("distill {sid} {text}"));
                write_last(&sid, &text);
            }
            Err(e) => {
                write_last(
                    &sid,
                    &format!(
                        "{}: {e}",
                        if e.downcast_ref::<Deferred>().is_some() {
                            "deferred"
                        } else {
                            "failed"
                        }
                    ),
                );
                failed |= handle_error(&sid, e.as_ref(), failures);
            }
        }
    }
    drop(lock);
    if !failed {
        clear_failures();
    }
    Ok(())
}

pub(crate) fn is_live(path: Option<&str>) -> bool {
    path.and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(LIVE_SECS))
}

fn write_last(sid: &str, result: &str) {
    let _ = paths::write_control_file(
        &paths::distill_last_path(),
        &format!(
            "at={}\nsid={sid}\nresult={}\n",
            crate::timeutil::now_unix(),
            embed::one_line(result)
        ),
    );
}

fn handle_error(sid: &str, error: &(dyn std::error::Error + 'static), failures: u32) -> bool {
    if error.downcast_ref::<Deferred>().is_some() {
        crate::hook::log(&format!("distill {sid} deferred: {error}"));
        false
    } else {
        crate::hook::log(&format!("distill {sid} failed: {error}"));
        record_failure(failures, &error.to_string());
        true
    }
}

fn retry_pending() -> bool {
    paths::read_control_file(&paths::distill_state_path(), 4096)
        .ok()
        .flatten()
        .and_then(|s| paths::kv_get(&s, "last_attempt"))
        .and_then(|s| s.parse::<i64>().ok())
        .is_some_and(|at| crate::timeutil::now_unix().saturating_sub(at) < RETRY_SECS)
}

fn record_attempt() -> u32 {
    let failures = paths::read_control_file(&paths::distill_state_path(), 4096)
        .ok()
        .flatten()
        .and_then(|s| paths::kv_get(&s, "failures"))
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    let _ = paths::write_control_file(
        &paths::distill_state_path(),
        &format!(
            "last_attempt={}\nfailures={failures}\n",
            crate::timeutil::now_unix()
        ),
    );
    failures
}

fn record_failure(failures: u32, error: &str) {
    let _ = paths::write_control_file(
        &paths::distill_state_path(),
        &format!(
            "last_attempt={}\nfailures={failures}\nerror={}\n",
            crate::timeutil::now_unix(),
            embed::one_line(error)
        ),
    );
}

fn clear_failures() {
    let _ = std::fs::remove_file(paths::distill_state_path());
}

fn run_lock() -> Result<Option<Connection>, Box<dyn std::error::Error>> {
    let db = paths::db_path();
    let real = db.canonicalize().unwrap_or_else(|_| {
        db.parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.join(db.file_name().unwrap_or_default()))
            .unwrap_or(db.clone())
    });
    let path = real.with_file_name(format!(
        "{}.distill-lock",
        real.file_name().unwrap_or_default().to_string_lossy()
    ));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(50))?;
    match conn.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => Ok(Some(conn)),
        Err(e)
            if matches!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

fn queued_rows(conn: &Connection) -> rusqlite::Result<Vec<(String, Option<String>, i64)>> {
    let mut stmt = conn.prepare("SELECT q.session_id, s.file_path, COALESCE(s.skipped_lines,0) + COALESCE(s.partial_tail,0) FROM checkpoint_queue q LEFT JOIN sessions s USING(session_id) ORDER BY q.enqueue_seq DESC")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect();
    rows
}

fn child_id() -> Result<String, Box<dyn std::error::Error>> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    ))
}

fn mute(conn: &Connection, sid: &str, child_id: &str) -> rusqlite::Result<i64> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let boundary = ingest::distilled_boundary(&tx, sid)?;
    tx.execute(
        "INSERT OR IGNORE INTO sessions(session_id, checkpointed_seq) VALUES (?, 9223372036854775807)",
        [child_id],
    )?;
    tx.commit()?;
    Ok(boundary)
}

fn distill_one(program: &std::path::Path, sid: &str) -> Result<String, Box<dyn std::error::Error>> {
    let program = program.canonicalize()?;
    let conn = db::connect_with_timeout(Duration::from_secs(10))?;
    let id = child_id()?;
    let boundary = mute(&conn, sid, &id)?;
    let incomplete: (i64, i64) = conn.query_row(
        "SELECT COALESCE(skipped_lines,0) + COALESCE(partial_tail,0), COALESCE(checkpointed_seq, -1) FROM sessions WHERE session_id=?",
        [sid],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;
    let (incomplete, checkpointed) = incomplete;
    if incomplete != 0 {
        return Err(Deferred("session archive is incomplete").into());
    }
    let start = db::now_iso();
    let memdir = conn
        .query_row(
            "SELECT project FROM sessions WHERE session_id=?",
            [sid],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|project| paths::projects_dir().join(project).join("memory"))
        .unwrap_or_else(|| paths::mem_dir().join("memory"));
    std::fs::create_dir_all(&memdir)?;
    let before = snapshot(&memdir)?;

    // NOTE: --since only when 0 <= watermark < last seq, else a full reread. A replaced longer
    // transcript can still hide changed turns below the watermark; a reset marker would close that.
    let since = if (0..boundary).contains(&checkpointed) {
        format!(" Earlier runs distilled this session through seq {checkpointed}. Read only the later turns with subrosa session {sid} --since {checkpointed} --boundary, and read earlier turns only for context.")
    } else {
        String::new()
    };
    let prompt = format!(
        "{}\n\nSession id: {sid}\nMemory directory: {}\nToday: {}\nProcess only this queued session. Use --origin-session {sid} only for fact upsert, and --memdir {} for every fact and generate command. In queued mode, skip the queue step and never run checkpoint-mark, checkpoint-drop, or checkpoint-clear. Print exactly one line: SESSION_TOTAL: saved <n>, updated <n>. saved counts new leaf files you wrote in the memory directory. updated counts every other change, such as an edited leaf, a changed hook, or an archived fact. Print SESSION_TOTAL: saved 0, updated 0 when you changed nothing.{}\n",
        include_str!("../skills/checkpoint/SKILL.md"),
        memdir.display(),
        db::now_iso(),
        memdir.display(), since
    );

    // `current_dir` does not update PWD, so a wrapper saw the symlink target. Set it explicitly.
    let output = Command::new(program)
        .args([
            "-p",
            "--bare",
            "--session-id",
            &id,
            "--model",
            "sonnet",
            "--max-turns",
            "40",
            "--max-budget-usd",
            "1",
            "--allowedTools",
            &format!(
                "Read(//{}/**),Edit(//{}/**),Bash(subrosa session *),Bash(subrosa search *),Bash(subrosa fact *),Bash(subrosa generate *)",
                memdir.display().to_string().trim_start_matches('/'),
                memdir.display().to_string().trim_start_matches('/')
            ),
            "--",
            &prompt,
        ])
        .current_dir(&memdir)
        .env("PWD", &memdir)
        .env("SUBROSA_ORIGIN_SESSION", sid)
        .env("PATH", format!("{}:{}", paths::mem_dir().join("bin").display(), std::env::var("PATH").unwrap_or_default()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    let status = output.status;
    if !status.success() {
        crate::hook::log(&format!("distill {sid} child exited with {status}"));
        return Err(format!("child exited with {status}").into());
    }
    let end = db::now_iso();
    let conn = db::connect_with_timeout(Duration::from_secs(10))?;
    let project: String = conn.query_row(
        "SELECT project FROM sessions WHERE session_id=?",
        [sid],
        |r| r.get(0),
    )?;
    let proof = prove(
        &conn,
        &project,
        sid,
        &memdir,
        &before,
        &start,
        &end,
        &String::from_utf8_lossy(&output.stdout),
    )?;
    let result = match proof {
        Proof::Saved | Proof::NoOp => finish(&conn, sid, boundary)?,
        Proof::Rejected(reason) => return Err(reason.into()),
    };
    match result {
        "dropped" => Ok(format!("done through {boundary}")),
        "kept" => Ok(format!("kept through {boundary}, session grew")),
        "deferred" => Err(Deferred("session archive is incomplete").into()),
        _ => Err(Deferred("unknown finish result").into()),
    }
}

#[derive(Debug)]
struct Deferred(&'static str);

impl std::fmt::Display for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Deferred {}

#[derive(Debug, PartialEq)]
enum Proof {
    Saved,
    NoOp,
    Rejected(String),
}

fn snapshot(
    memdir: &std::path::Path,
) -> Result<BTreeMap<String, [u8; 32]>, Box<dyn std::error::Error>> {
    let mut out = BTreeMap::new();
    if !memdir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(memdir)? {
        let path = entry?.path();
        if path.file_name().and_then(|n| n.to_str()) == Some("MEMORY.md") || !path.is_file() {
            continue;
        }
        let mut hash = Sha256::new();
        hash.update(std::fs::read(&path)?);
        out.insert(
            path.file_name().unwrap().to_string_lossy().into_owned(),
            hash.finalize().into(),
        );
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn prove(
    conn: &Connection,
    project: &str,
    sid: &str,
    memdir: &std::path::Path,
    before: &BTreeMap<String, [u8; 32]>,
    start: &str,
    end: &str,
    stdout: &str,
) -> Result<Proof, Box<dyn std::error::Error>> {
    let after = snapshot(memdir)?;
    let changed: Vec<&str> = after
        .iter()
        .filter_map(|(name, hash)| (before.get(name) != Some(hash)).then_some(name.as_str()))
        .collect();
    if changed.is_empty() {
        if before.keys().any(|name| !after.contains_key(name)) {
            return Ok(Proof::Rejected("leaf deleted during distill".into()));
        }
        return Ok(
            if stdout.lines().any(|line| {
                line.strip_prefix("SESSION_TOTAL: saved 0, updated ")
                    .is_some_and(|count| {
                        !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit())
                    })
            }) {
                Proof::NoOp
            } else {
                Proof::Rejected("missing exact no-op report".into())
            },
        );
    }
    for leaf in changed {
        let found: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM facts WHERE project=? AND leaf_path=? AND (origin_session=? OR (updated_at>=? AND updated_at<=?)))", params![project, leaf, sid, start, end], |r| r.get(0))?;
        if !found {
            return Ok(Proof::Rejected(format!(
                "changed leaf {leaf} has no fact row"
            )));
        }
    }
    Ok(Proof::Saved)
}

fn finish(conn: &Connection, sid: &str, boundary: i64) -> rusqlite::Result<&'static str> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let latest: i64 = tx.query_row(
        "SELECT COALESCE(max(seq), -1) FROM turns WHERE session_id=?",
        [sid],
        |r| r.get(0),
    )?;
    let incomplete: i64 = tx.query_row("SELECT COALESCE(skipped_lines,0) + COALESCE(partial_tail,0) FROM sessions WHERE session_id=?", [sid], |r| r.get(0))?;
    if latest < boundary || incomplete != 0 {
        tx.rollback()?;
        return Ok("deferred");
    }
    ingest::advance_checkpoint(&tx, sid, boundary)?;
    let result = if latest == boundary {
        tx.execute("DELETE FROM checkpoint_queue WHERE session_id=?", [sid])?;
        "dropped"
    } else {
        "kept"
    };
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    fn proof_db() -> (Connection, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("subrosa-proof-{}", child_id().unwrap()));
        fs::create_dir_all(&root).unwrap();
        let conn = Connection::open(root.join("proof.db")).unwrap();
        conn.execute_batch("CREATE TABLE sessions(session_id TEXT PRIMARY KEY, project TEXT, last_seq INTEGER, checkpointed_seq INTEGER DEFAULT -1, skipped_lines INTEGER DEFAULT 0, partial_tail INTEGER DEFAULT 0); CREATE TABLE turns(session_id TEXT, seq INTEGER, role TEXT, text TEXT); CREATE TABLE checkpoint_queue(session_id TEXT PRIMARY KEY, enqueued_at TEXT, enqueue_seq INTEGER); CREATE TABLE facts(project TEXT, leaf_path TEXT, origin_session TEXT, updated_at TEXT);").unwrap();
        conn.execute("INSERT INTO sessions(session_id,project,last_seq,checkpointed_seq) VALUES ('sid','project',1,0)", []).unwrap();
        conn.execute("INSERT INTO checkpoint_queue(session_id,enqueued_at,enqueue_seq) VALUES ('sid','now',1)", []).unwrap();
        conn.execute(
            "INSERT INTO turns(session_id,seq,role,text) VALUES ('sid',1,'user','turn')",
            [],
        )
        .unwrap();
        let memdir = root.join("memory");
        fs::create_dir_all(&memdir).unwrap();
        (conn, memdir)
    }

    #[test]
    fn mute_waits_for_writer_lock() {
        let (conn, _) = proof_db();
        conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        let path = conn
            .query_row("PRAGMA database_list", [], |row| row.get::<_, String>(2))
            .unwrap();
        let writer = Connection::open(path).unwrap();
        writer.busy_timeout(Duration::from_secs(2)).unwrap();
        conn.busy_timeout(Duration::from_secs(2)).unwrap();
        writer
            .execute_batch(
                "BEGIN IMMEDIATE; UPDATE sessions SET last_seq=last_seq WHERE session_id='sid'",
            )
            .unwrap();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            writer.execute_batch("COMMIT").unwrap();
        });

        assert!(mute(&conn, "sid", "child").is_ok());
        handle.join().unwrap();
    }

    #[test]
    fn live_transcript_is_only_live_when_recent() {
        let path = std::env::temp_dir().join(format!("subrosa-live-{}", child_id().unwrap()));
        let file = fs::File::create(&path).unwrap();
        assert!(is_live(path.to_str()));
        file.set_modified(SystemTime::now() - Duration::from_secs(11 * 60))
            .unwrap();
        assert!(!is_live(path.to_str()));
        assert!(!is_live(None));
        assert!(!is_live(Some("/no/such/transcript")));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn allowed_tools_scope_file_access_to_memdir() {
        let memdir = std::path::Path::new("/Users/test/memory");
        let allowed = format!(
            "Read(//{}/**),Edit(//{}/**),Bash(subrosa session *),Bash(subrosa search *),Bash(subrosa fact *),Bash(subrosa generate *)",
            memdir.display().to_string().trim_start_matches('/'),
            memdir.display().to_string().trim_start_matches('/')
        );
        assert!(allowed.starts_with("Read(//Users/test/memory/**),Edit(//Users/test/memory/**),"));
        assert!(!allowed
            .split(',')
            .any(|rule| matches!(rule, "Read" | "Write" | "Edit")));
    }

    #[test]
    fn saved_leaf_with_row_proves_and_drops() {
        let (conn, memdir) = proof_db();
        fs::write(memdir.join("note.md"), "new").unwrap();
        let before = BTreeMap::new();
        conn.execute("INSERT INTO facts(project,leaf_path,origin_session,updated_at) VALUES ('project','note.md','sid','2026-01-01T00:00:01+00:00')", []).unwrap();
        assert_eq!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &before,
                "2025-01-01",
                "9999-01-01",
                ""
            )
            .unwrap(),
            Proof::Saved
        );
        assert_eq!(finish(&conn, "sid", 1).unwrap(), "dropped");
    }

    #[test]
    fn changed_leaf_without_row_is_rejected() {
        let (conn, memdir) = proof_db();
        let before = BTreeMap::new();
        fs::write(memdir.join("note.md"), "new").unwrap();
        assert!(matches!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &before,
                "2025-01-01",
                "9999-01-01",
                ""
            )
            .unwrap(),
            Proof::Rejected(_)
        ));
    }

    #[test]
    fn row_without_leaf_change_is_rejected() {
        let (conn, memdir) = proof_db();
        conn.execute("INSERT INTO facts(project,leaf_path,origin_session,updated_at) VALUES ('project','missing.md','sid','2026-01-01T00:00:01+00:00'), ('other','other.md','sid','2026-01-01T00:00:01+00:00')", []).unwrap();
        assert!(matches!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &BTreeMap::new(),
                "2025-01-01",
                "9999-01-01",
                ""
            )
            .unwrap(),
            Proof::Rejected(_)
        ));
    }

    #[test]
    fn noop_needs_exact_report_line() {
        let (conn, memdir) = proof_db();
        assert_eq!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &BTreeMap::new(),
                "2025-01-01",
                "9999-01-01",
                "SESSION_TOTAL: saved 0, updated 0"
            )
            .unwrap(),
            Proof::NoOp
        );
        assert_eq!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &BTreeMap::new(),
                "2025-01-01",
                "9999-01-01",
                "SESSION_TOTAL: saved 0, updated 1"
            )
            .unwrap(),
            Proof::NoOp
        );
        for report in [
            "SESSION_TOTAL: saved 1, updated 0",
            "SESSION_TOTAL: saved 0, updated ",
            "SESSION_TOTAL: saved 0, updated 1x",
        ] {
            assert!(matches!(
                prove(
                    &conn,
                    "project",
                    "sid",
                    &memdir,
                    &BTreeMap::new(),
                    "2025-01-01",
                    "9999-01-01",
                    report
                )
                .unwrap(),
                Proof::Rejected(_)
            ));
        }
        assert!(matches!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &BTreeMap::new(),
                "2025-01-01",
                "9999-01-01",
                " SESSION_TOTAL: saved 0, updated 0",
            )
            .unwrap(),
            Proof::Rejected(_)
        ));
        assert!(matches!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &BTreeMap::new(),
                "2025-01-01",
                "9999-01-01",
                "{\"text\":\"SESSION_TOTAL: saved 0, updated 0\"}",
            )
            .unwrap(),
            Proof::Rejected(_)
        ));
        conn.execute(
            "INSERT INTO turns(session_id,seq,role,text) VALUES ('sid',2,'user','grown')",
            [],
        )
        .unwrap();
        let proof = prove(
            &conn,
            "project",
            "sid",
            &memdir,
            &BTreeMap::new(),
            "2025-01-01",
            "9999-01-01",
            "SESSION_TOTAL: saved 0, updated 0",
        )
        .unwrap();
        assert_eq!(proof, Proof::NoOp);
    }

    #[test]
    fn deleted_leaf_is_rejected_as_noop() {
        let (conn, memdir) = proof_db();
        let mut before = BTreeMap::new();
        before.insert("deleted.md".into(), [0; 32]);
        assert!(matches!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &before,
                "2025-01-01",
                "9999-01-01",
                "SESSION_TOTAL: saved 0, updated 1"
            )
            .unwrap(),
            Proof::Rejected(message) if message == "leaf deleted during distill"
        ));
    }

    #[test]
    fn finish_keeps_growth_and_defers_partial_tail() {
        let (conn, _) = proof_db();
        conn.execute(
            "INSERT INTO turns(session_id,seq,role,text) VALUES ('sid',2,'user','grown')",
            [],
        )
        .unwrap();
        assert_eq!(finish(&conn, "sid", 1).unwrap(), "kept");
        assert_eq!(
            conn.query_row(
                "SELECT checkpointed_seq FROM sessions WHERE session_id='sid'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        conn.execute("DELETE FROM turns WHERE seq=2", []).unwrap();
        conn.execute(
            "UPDATE sessions SET partial_tail=1 WHERE session_id='sid'",
            [],
        )
        .unwrap();
        assert_eq!(finish(&conn, "sid", 1).unwrap(), "deferred");
    }

    #[test]
    fn incomplete_archive_stays_at_old_watermark() {
        let (conn, _) = proof_db();
        conn.execute(
            "INSERT INTO turns(session_id,seq,role,text) VALUES ('sid',2,'user','grown')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions SET partial_tail=1 WHERE session_id='sid'",
            [],
        )
        .unwrap();
        assert_eq!(finish(&conn, "sid", 2).unwrap(), "deferred");
        assert_eq!(
            conn.query_row(
                "SELECT checkpointed_seq FROM sessions WHERE session_id='sid'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM checkpoint_queue WHERE session_id='sid'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn grown_session_keeps_progress_without_retry_state() {
        let _env = crate::paths::test_env_lock();
        let root =
            std::env::temp_dir().join(format!("subrosa-distill-test-{}", child_id().unwrap()));
        fs::create_dir_all(&root).unwrap();
        std::env::set_var("SUBROSA_DIR", &root);
        let (conn, memdir) = proof_db();
        conn.execute(
            "INSERT INTO turns(session_id,seq,role,text) VALUES ('sid',2,'user','grown')",
            [],
        )
        .unwrap();
        assert_eq!(finish(&conn, "sid", 1).unwrap(), "kept");
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM checkpoint_queue WHERE session_id='sid'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let proof = prove(
            &conn,
            "project",
            "sid",
            &memdir,
            &BTreeMap::new(),
            "2025-01-01",
            "9999-01-01",
            "SESSION_TOTAL: saved 0, updated 0",
        )
        .unwrap();
        assert_eq!(proof, Proof::NoOp);
        assert!(!paths::distill_state_path().exists());
        std::env::remove_var("SUBROSA_DIR");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_child_with_fact_row_keeps_queue() {
        let (conn, memdir) = proof_db();
        fs::write(memdir.join("note.md"), "new").unwrap();
        conn.execute("INSERT INTO facts(project,leaf_path,origin_session,updated_at) VALUES ('project','note.md','sid','2026-01-01T00:00:01+00:00')", []).unwrap();
        let proof = prove(
            &conn,
            "project",
            "sid",
            &memdir,
            &BTreeMap::new(),
            "2025-01-01",
            "9999-01-01",
            "",
        )
        .unwrap();
        assert_eq!(proof, Proof::Saved);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM checkpoint_queue WHERE session_id='sid'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn deleted_leaf_with_row_is_rejected() {
        let (conn, memdir) = proof_db();
        let path = memdir.join("note.md");
        fs::write(&path, "old").unwrap();
        let before = snapshot(&memdir).unwrap();
        fs::remove_file(path).unwrap();
        conn.execute("INSERT INTO facts(project,leaf_path,origin_session,updated_at) VALUES ('project','note.md','sid','2026-01-01T00:00:01+00:00')", []).unwrap();
        assert!(matches!(
            prove(
                &conn,
                "project",
                "sid",
                &memdir,
                &before,
                "2025-01-01",
                "9999-01-01",
                ""
            )
            .unwrap(),
            Proof::Rejected(_)
        ));
    }
}
