//! Read or de-queue one archived session — helpers for the /checkpoint-backlog skill.

use std::path::PathBuf;
use std::process::ExitCode;

use rusqlite::OptionalExtension;

use crate::{db, ingest, paths};

/// Cap on how many candidates an ambiguous prefix lists before "+N more".
const AMBIGUOUS_LIST_CAP: usize = 10;

/// Missing fields render as the literal `None` — part of the session-dump
/// format the checkpoint skills consume (pinned by golden tests).
fn opt_str(v: Option<&str>) -> &str {
    v.unwrap_or("None")
}

/// What a session argument resolves to: a single id, nothing, or several.
enum Resolved {
    One(String),
    None,
    Ambiguous(Vec<String>),
}

/// Resolve a session argument to one archived session id. An exact id wins
/// outright (even if it also prefixes another); otherwise it's treated as a
/// prefix — the 8-char `sid8` that `search`/`related` print. Resolves against
/// the `turns` table so it only ever points at a dumpable session. `substr(…)=`
/// (not `LIKE`) so `%`/`_` in the input aren't read as wildcards.
fn resolve_session(conn: &rusqlite::Connection, arg: &str) -> Resolved {
    if arg.is_empty() {
        return Resolved::None;
    }
    let exact = conn
        .query_row(
            "SELECT 1 FROM turns WHERE session_id = ? LIMIT 1",
            [arg],
            |_| Ok(()),
        )
        .optional()
        .unwrap_or(None);
    if exact.is_some() {
        return Resolved::One(arg.to_string());
    }
    let matches: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT session_id FROM turns \
             WHERE substr(session_id, 1, ?1) = ?2 ORDER BY session_id",
        )
        .and_then(|mut s| {
            s.query_map(rusqlite::params![arg.chars().count() as i64, arg], |r| {
                r.get::<_, String>(0)
            })?
            .collect()
        })
        .unwrap_or_default();
    match matches.len() {
        0 => Resolved::None,
        1 => Resolved::One(matches.into_iter().next().unwrap()),
        _ => Resolved::Ambiguous(matches),
    }
}

/// Print a session's flattened turns (for the in-session model to read). Accepts
/// the full session id or any unique prefix (e.g. the 8-char id `search` and
/// `related` print).
pub fn dump(arg: &str) -> ExitCode {
    let conn = match db::connect_readonly() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    let sid = match resolve_session(&conn, arg) {
        Resolved::One(id) => id,
        Resolved::None => {
            eprintln!("[subrosa] no archived turns for session {arg} (not ingested?)");
            return ExitCode::FAILURE;
        }
        Resolved::Ambiguous(ids) => {
            eprintln!(
                "[subrosa] \"{arg}\" matches {} sessions — use a longer prefix or the full id:",
                ids.len()
            );
            for id in ids.iter().take(AMBIGUOUS_LIST_CAP) {
                eprintln!("  {id}");
            }
            if ids.len() > AMBIGUOUS_LIST_CAP {
                eprintln!("  … and {} more", ids.len() - AMBIGUOUS_LIST_CAP);
            }
            return ExitCode::from(2);
        }
    };
    let rows: Result<Vec<(String, Option<String>)>, _> = conn
        .prepare("SELECT role, text FROM turns WHERE session_id=? ORDER BY seq")
        .and_then(|mut s| {
            s.query_map([sid.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        });
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[subrosa] query error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if rows.is_empty() {
        eprintln!("[subrosa] no archived turns for session {sid} (not ingested?)");
        return ExitCode::FAILURE;
    }
    type Meta = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let meta: Option<Meta> = conn
        .query_row(
            "SELECT project, cwd, first_ts, last_ts FROM sessions WHERE session_id=?",
            [sid.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .unwrap_or(None);
    if let Some((project, cwd, first, last)) = meta {
        let project = project.unwrap_or_default();
        let memdir = paths::projects_dir().join(&project).join("memory");
        println!(
            "# session {sid}  project={project}  cwd={}  {}..{}",
            opt_str(cwd.as_deref()),
            opt_str(first.as_deref()),
            opt_str(last.as_deref())
        );
        println!("# memdir: {}\n", memdir.display());
    }
    for (role, text) in rows {
        println!("## {role}\n{}\n", text.unwrap_or_default());
    }
    ExitCode::SUCCESS
}

/// Remove one session from the queue and record the checkpoint high-water mark
/// so a re-fired SessionEnd won't re-queue it unless the transcript grows.
pub fn drop_sid(sid: &str) -> ExitCode {
    // last_seq is read fresh at drop time — it may have grown since queuing.
    if let Ok(conn) = db::connect() {
        let _ = conn.execute(
            "UPDATE sessions SET checkpointed_seq=last_seq WHERE session_id=?",
            [sid],
        );
    }
    let pending = paths::pending_log();
    let Ok(text) = std::fs::read_to_string(&pending) else {
        println!("[subrosa] queue empty");
        return ExitCode::SUCCESS;
    };
    let lines: Vec<&str> = text.lines().collect();
    let keep: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|ln| ln.trim().rsplit('\t').next().unwrap_or(ln.trim()) != sid)
        .collect();
    if keep.len() != lines.len() {
        let body = if keep.is_empty() {
            String::new()
        } else {
            keep.join("\n") + "\n"
        };
        if let Err(e) = std::fs::write(&pending, body) {
            eprintln!("[subrosa] cannot write queue: {e}");
            return ExitCode::FAILURE;
        }
        println!("[subrosa] dropped {sid} from queue");
    } else {
        println!("[subrosa] {sid} not in queue");
    }
    ExitCode::SUCCESS
}

/// Conditionally queue a session (same gate as the SessionEnd hook).
pub fn enqueue(sid: &str) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    match ingest::enqueue_checkpoint(&conn, sid, &paths::pending_log()) {
        Ok(status) => {
            println!("[subrosa] enqueue {sid}: {status}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[subrosa] enqueue failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Mark the currently-running session as checkpointed: ingest its latest turns,
/// set the high-water mark, and drop it from the queue. Run as the last step of
/// /checkpoint so the session doesn't re-queue on the next SessionEnd.
pub fn mark_current() -> ExitCode {
    let Some(f) = live_session_file() else {
        println!("[subrosa] mark-current: no transcript found");
        return ExitCode::SUCCESS;
    };
    let sid = f
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Bring last_seq up to the checkpoint moment before recording the mark.
    let _ = ingest::ingest_file(&conn, &f);
    let _ = conn.execute(
        "UPDATE sessions SET checkpointed_seq=last_seq WHERE session_id=?",
        [sid.as_str()],
    );
    let last: Option<i64> = conn
        .query_row(
            "SELECT last_seq FROM sessions WHERE session_id=?",
            [sid.as_str()],
            |r| r.get(0),
        )
        .optional()
        .unwrap_or(None);
    let pending = paths::pending_log();
    if let Ok(text) = std::fs::read_to_string(&pending) {
        let lines: Vec<&str> = text.lines().collect();
        let keep: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|ln| ln.trim().rsplit('\t').next().unwrap_or(ln.trim()) != sid)
            .collect();
        if keep.len() != lines.len() {
            let body = if keep.is_empty() {
                String::new()
            } else {
                keep.join("\n") + "\n"
            };
            let _ = std::fs::write(&pending, body);
        }
    }
    println!(
        "[subrosa] marked current session {sid} checkpointed (last_seq={})",
        last.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
    );
    ExitCode::SUCCESS
}

/// Best guess at the session running right now: the most recently modified
/// transcript under the projects dir (/checkpoint keeps appending to it).
fn live_session_file() -> Option<PathBuf> {
    let root = paths::projects_dir();
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for project in std::fs::read_dir(root).ok()?.flatten() {
        let p = project.path();
        if !p.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for f in entries.flatten() {
            let fp = f.path();
            if fp.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(mtime) = f.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                newest = Some((mtime, fp));
            }
        }
    }
    newest.map(|(_, p)| p)
}
