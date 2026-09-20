//! Read or de-queue one archived session — helpers for the /checkpoint-backlog skill.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};

use crate::{db, ingest, paths, tags};

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

/// The shared "ambiguous prefix" listing (`dump` and `mark_current` print it alike).
fn print_ambiguous(arg: &str, ids: &[String]) {
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
/// `related` print). With `show_tags`, adds one `# tags:` line to the header;
/// the default output stays byte-identical (pinned by session_dump.golden).
pub fn dump(arg: &str, show_tags: bool, show_boundary: bool) -> ExitCode {
    let conn = match db::connect_queue_readonly() {
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
            print_ambiguous(arg, &ids);
            return ExitCode::from(2);
        }
    };
    let tx = match conn.unchecked_transaction() {
        Ok(tx) => tx,
        Err(e) => {
            eprintln!("[subrosa] cannot start session read: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rows: Result<Vec<(String, Option<String>)>, _> = tx
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
    let meta: Option<Meta> = tx
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
        // Default output ends here (a memdir line + blank line). --tags slots one
        // extra line in before the blank, so session_dump.golden stays untouched.
        println!("# memdir: {}", memdir.display());
        if show_boundary {
            let boundary: i64 = tx
                .query_row(
                    "SELECT COALESCE(max(seq), -1) FROM turns WHERE session_id=?",
                    [sid.as_str()],
                    |r| r.get(0),
                )
                .unwrap_or(-1);
            println!("# last_seq: {boundary}");
        }
        if show_tags {
            let line = match tags::tags_for_session(&conn, &sid) {
                Ok(t) if !t.is_empty() => t.join(", "),
                _ => "—".to_string(),
            };
            println!("# tags: {line}");
        }
        println!();
    }
    if let Err(e) = tx.commit() {
        eprintln!("[subrosa] cannot finish session read: {e}");
        return ExitCode::FAILURE;
    }
    for (role, text) in rows {
        println!("## {role}\n{}\n", text.unwrap_or_default());
    }
    ExitCode::SUCCESS
}

/// Remove one session from the queue and record the checkpoint high-water mark
/// so a re-fired SessionEnd won't re-queue it unless the transcript grows.
pub fn drop_sid(sid: &str, max_seq: Option<i64>) -> ExitCode {
    let conn = match db::connect_with_timeout(std::time::Duration::from_secs(10)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    let tx = match Transaction::new_unchecked(&conn, TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(e) => {
            eprintln!("[subrosa] cannot start checkpoint transaction: {e}");
            return ExitCode::FAILURE;
        }
    };
    let last_seq = match tx
        .query_row(
            "SELECT COALESCE((SELECT max(seq) FROM turns WHERE session_id=?), -1)",
            [sid],
            |r| r.get::<_, i64>(0),
        )
        .optional()
    {
        Ok(Some(seq)) => seq,
        Ok(None) => -1,
        Err(e) => {
            eprintln!("[subrosa] cannot read session: {e}");
            return ExitCode::FAILURE;
        }
    };
    let boundary = max_seq.unwrap_or_else(|| {
        tx.query_row(
            "SELECT COALESCE(checkpointed_seq, -1) FROM sessions WHERE session_id=?",
            [sid],
            |r| r.get(0),
        )
        .unwrap_or(-1)
    });
    if boundary > last_seq {
        eprintln!("[subrosa] max sequence {boundary} is above archived last sequence {last_seq}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = ingest::advance_checkpoint(&tx, sid, boundary) {
        eprintln!("[subrosa] cannot mark session {sid} checkpointed: {e}");
        return ExitCode::FAILURE;
    }
    if boundary < last_seq {
        if let Err(e) = tx.commit() {
            eprintln!("[subrosa] cannot save checkpoint: {e}");
            return ExitCode::FAILURE;
        }
        println!("[subrosa] kept {sid} queued; checkpointed through {boundary} of {last_seq}");
    } else {
        match tx.execute("DELETE FROM checkpoint_queue WHERE session_id=?", [sid]) {
            Ok(0) => println!("[subrosa] {sid} not in queue"),
            Ok(_) => println!("[subrosa] dropped {sid} from queue"),
            Err(e) => {
                eprintln!("[subrosa] cannot drop {sid} from queue: {e}");
                return ExitCode::FAILURE;
            }
        }
        if let Err(e) = tx.commit() {
            eprintln!("[subrosa] cannot save checkpoint: {e}");
            return ExitCode::FAILURE;
        }
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
    match ingest::enqueue_checkpoint(&conn, sid) {
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

/// Mark a session as checkpointed: ingest its latest turns, set the high-water
/// mark, and drop it from the queue. Run as the last step of /checkpoint so the
/// session doesn't re-queue on the next SessionEnd. No argument targets the cwd
/// project's live session; an explicit id/prefix pins it exactly. A busier
/// transcript elsewhere (another project, a spawned agent) can't steal the mark.
pub fn mark_current(arg: Option<&str>, max_seq: Option<i64>) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (sid, file) = match arg {
        Some(a) => match resolve_session(&conn, a) {
            Resolved::One(id) => {
                // Rebuild the transcript path from the stored project encoding;
                // an already-rotated file is fine — the mark still lands.
                let file = conn
                    .query_row(
                        "SELECT project FROM sessions WHERE session_id=?",
                        [id.as_str()],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .unwrap_or(None)
                    .map(|proj| paths::projects_dir().join(proj).join(format!("{id}.jsonl")))
                    .filter(|f| f.exists());
                (id, file)
            }
            Resolved::None => {
                eprintln!("[subrosa] no archived turns for session {a} (not ingested?)");
                return ExitCode::FAILURE;
            }
            Resolved::Ambiguous(ids) => {
                print_ambiguous(a, &ids);
                return ExitCode::from(2);
            }
        },
        None => {
            let Some(f) = live_session_file() else {
                println!("[subrosa] mark-current: no transcript found");
                return ExitCode::SUCCESS;
            };
            let sid = f
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            (sid, Some(f))
        }
    };
    // Bring last_seq up to the checkpoint moment unless the caller supplied a boundary.
    if let Some(f) = &file {
        if max_seq.is_none() {
            if let Err(e) = ingest::ingest_file(&conn, f) {
                eprintln!("[subrosa] cannot ingest session before marking: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let tx = match Transaction::new_unchecked(&conn, TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(e) => {
            eprintln!("[subrosa] cannot start checkpoint transaction: {e}");
            return ExitCode::FAILURE;
        }
    };
    let last = ingest::distilled_boundary(&tx, &sid).unwrap_or(-1);
    let full_mark = max_seq.map(|seq| seq == last).unwrap_or(true);
    if let Some(max_seq) = max_seq {
        if max_seq > last {
            eprintln!("[subrosa] max sequence {max_seq} is above archived last sequence {last}");
            return ExitCode::FAILURE;
        }
    }
    let mark_result = ingest::advance_checkpoint(&tx, &sid, max_seq.unwrap_or(last));
    if let Err(e) = mark_result {
        eprintln!("[subrosa] cannot mark session {sid} checkpointed: {e}");
        return ExitCode::FAILURE;
    }
    if full_mark {
        if let Err(e) = tx.execute(
            "DELETE FROM checkpoint_queue WHERE session_id=?",
            [sid.as_str()],
        ) {
            eprintln!("[subrosa] cannot drop {sid} from queue: {e}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = tx.commit() {
        eprintln!("[subrosa] cannot save checkpoint: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "[subrosa] marked current session {sid} checkpointed (last_seq={})",
        last
    );
    ExitCode::SUCCESS
}

/// Best guess at the session running right now: the most recently modified
/// transcript in the cwd's OWN project dir (the checkpoint flow keeps appending to it),
/// falling back to the newest across all projects. The cwd scope keeps a busier
/// concurrent session in another project from stealing the mark; raw + resolved
/// cwd are both tried because Claude Code encodes the symlink-resolved path.
fn live_session_file() -> Option<PathBuf> {
    let root = paths::projects_dir();
    if let Ok(cwd) = std::env::current_dir() {
        let mut cands: Vec<String> = vec![db::encode_cwd(&cwd.to_string_lossy())];
        if let Ok(real) = cwd.canonicalize() {
            let e = db::encode_cwd(&real.to_string_lossy());
            if !cands.contains(&e) {
                cands.push(e);
            }
        }
        let best = cands
            .iter()
            .filter_map(|p| newest_jsonl_in(&root.join(p)))
            .max_by_key(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok());
        if best.is_some() {
            return best;
        }
    }
    // Fallback: newest transcript anywhere (the pre-cwd-scope behavior).
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for project in std::fs::read_dir(root).ok()?.flatten() {
        let p = project.path();
        if !p.is_dir() {
            continue;
        }
        let Some(f) = newest_jsonl_in(&p) else {
            continue;
        };
        let Ok(mtime) = std::fs::metadata(&f).and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            newest = Some((mtime, f));
        }
    }
    newest.map(|(_, p)| p)
}

/// Newest `.jsonl` in one directory (None when the dir is missing or empty).
fn newest_jsonl_in(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for f in std::fs::read_dir(dir).ok()?.flatten() {
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
    newest.map(|(_, p)| p)
}
