//! Transcript JSONL → turns rows: keeps real user prompts and assistant
//! text/thinking, compacts tool_use to name + args, keeps only a short head
//! of tool_result, drops meta wrappers. The stored-text format is pinned
//! byte-for-byte by golden tests — existing archives must re-ingest cleanly.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;

use crate::db::now_iso;
use crate::redact::redact;

// Per-record flattening caps (chars). Tool output is mostly noise; keep a searchable head.
const TOOL_USE_CAP: usize = 300;
const TOOL_RESULT_CAP: usize = 500;
const THINKING_CAP: usize = 2000;
const RECORD_CAP: usize = 8000;

// Machine-generated wrapper records arrive as `user` turns but carry no conversation.
// Real prompts never start with these literal tags.
const NOISE_PREFIXES: [&str; 7] = [
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    // Our own context injections (recall header, session-start nudge) —
    // archiving them would feed past injections back into future results.
    "[subrosa recall]",
    "[subrosa]",
];

/// JSON with `", "` / `": "` separators — the archive's canonical stored-text
/// format for tool args, pinned byte-for-byte by golden tests.
struct SpacedSeps;

impl serde_json::ser::Formatter for SpacedSeps {
    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }
    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }
    fn begin_object_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        writer.write_all(b": ")
    }
}

fn to_json_spaced(v: &Value) -> String {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, SpacedSeps);
    if serde::Serialize::serialize(v, &mut ser).is_err() {
        return v.to_string();
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// Char-based truncation so multi-byte text never splits mid-codepoint.
fn cap(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn is_command_noise(text: &str) -> bool {
    let s = text.trim_start();
    NOISE_PREFIXES.iter().any(|p| s.starts_with(p))
}

/// tool_result content is sometimes a string, sometimes a list of blocks.
fn stringify(body: Option<&Value>) -> String {
    match body {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for b in items {
                match b {
                    Value::Object(m) => {
                        let is_text = m.get("type").and_then(Value::as_str) == Some("text")
                            || m.contains_key("text");
                        if is_text {
                            out.push(
                                m.get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                            );
                        }
                    }
                    Value::String(s) => out.push(s.clone()),
                    _ => {}
                }
            }
            out.join("\n")
        }
        Some(v @ Value::Object(m)) => m
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string(v).unwrap_or_default()),
        Some(v) => v.to_string(),
    }
}

/// Map one raw transcript record to (role, text), or None to skip.
pub fn flatten_record(o: &Value) -> Option<(String, String)> {
    let t = o.get("type").and_then(Value::as_str)?;
    if t != "user" && t != "assistant" {
        return None;
    }
    if o.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
        return None; // local-command caveats / injected wrappers, not real conversation
    }
    let empty = Value::Object(serde_json::Map::new());
    let msg = match o.get("message") {
        Some(m) if m.is_object() => m,
        _ => &empty,
    };
    let role = msg.get("role").and_then(Value::as_str).unwrap_or(t);
    let mut parts: Vec<String> = Vec::new();
    match msg.get("content") {
        Some(Value::String(s)) => {
            let s = s.trim();
            if !s.is_empty() {
                parts.push(s.to_string());
            }
        }
        Some(Value::Array(blocks)) => {
            for b in blocks {
                let Value::Object(m) = b else { continue };
                match m.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let s = m.get("text").and_then(Value::as_str).unwrap_or("").trim();
                        if !s.is_empty() {
                            parts.push(s.to_string());
                        }
                    }
                    Some("thinking") => {
                        let s = m
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim();
                        if !s.is_empty() {
                            parts.push(cap(s, THINKING_CAP));
                        }
                    }
                    Some("tool_use") => {
                        let name = m.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let input = m.get("input").cloned().unwrap_or(Value::Null);
                        let input = if input.is_null() {
                            Value::Object(serde_json::Map::new())
                        } else {
                            input
                        };
                        let arg = to_json_spaced(&input);
                        parts.push(format!("⚙ {} {}", name, cap(&arg, TOOL_USE_CAP)));
                    }
                    Some("tool_result") => {
                        let body = stringify(m.get("content"));
                        let body = body.trim();
                        if !body.is_empty() {
                            parts.push(format!("↪ {}", cap(body, TOOL_RESULT_CAP)));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let text = parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() || is_command_noise(text) {
        return None;
    }
    Some((role.to_string(), cap(&redact(text), RECORD_CAP)))
}

fn session_count(conn: &Connection, sid: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT count(*) FROM turns WHERE session_id=?",
        [sid],
        |r| r.get(0),
    )
}

struct Row {
    seq: i64,
    uuid: Option<String>,
    ts: Option<String>,
    role: String,
    text: String,
    is_meta: i64,
    is_sidechain: i64,
    cwd: Option<String>,
}

/// Parse one transcript JSONL and upsert its turns + session row. Idempotent.
/// Returns (inserted, scanned).
pub fn ingest_file(conn: &Connection, path: &Path) -> Result<(i64, i64), Box<dyn Error>> {
    if !path.exists() {
        return Ok((0, 0));
    }
    // Filename stem == sessionId; stable key for re-ingest + file tracking.
    let sid = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let project = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Transcripts are append-only, so resume from where the last ingest stopped:
    // seek to scan_offset and number lines from scan_seq, reading only the new bytes.
    // If the file is now shorter than that offset it was truncated or replaced (not an
    // append), so reset to (0, 0) and re-read from the top.
    let (resume_offset, resume_seq): (u64, i64) = conn
        .query_row(
            "SELECT scan_offset, scan_seq FROM sessions WHERE session_id=?",
            [&sid],
            |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0));

    let mut file = fs::File::open(path)?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let (mut offset, mut seq) = if resume_offset <= file_len {
        (resume_offset, resume_seq)
    } else {
        (0, 0)
    };
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut reader = BufReader::new(file);

    let mut rows: Vec<Row> = Vec::new();
    let (mut first_ts, mut last_ts, mut cwd): (Option<String>, Option<String>, Option<String>) =
        (None, None, None);
    let mut scanned: i64 = 0;

    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break; // EOF
        }
        // No trailing newline means the final line is still being written: leave it
        // (and the cursor) for the next pass, when it's complete. This is the old
        // "half-written last line, picked up next pass" behavior without re-reading.
        if buf.last() != Some(&b'\n') {
            break;
        }
        offset += n as u64;
        // seq is the absolute line index (blank/unparseable lines consume an index
        // too), so a record always maps to the same seq across passes — the basis
        // for INSERT OR IGNORE dedup.
        let i = seq;
        seq += 1;
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(o) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        scanned += 1;
        let Some((role, text)) = flatten_record(&o) else {
            continue;
        };
        let ts = o
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(ref t) = ts {
            if first_ts.as_deref().map(|f| t.as_str() < f).unwrap_or(true) {
                first_ts = Some(t.clone());
            }
            if last_ts.as_deref().map(|l| t.as_str() > l).unwrap_or(true) {
                last_ts = Some(t.clone());
            }
        }
        let row_cwd = o.get("cwd").and_then(Value::as_str).map(str::to_string);
        if cwd.is_none() {
            cwd = row_cwd.clone();
        }
        rows.push(Row {
            seq: i,
            uuid: o.get("uuid").and_then(Value::as_str).map(str::to_string),
            ts,
            role,
            text,
            is_meta: o.get("isMeta").and_then(Value::as_bool).unwrap_or(false) as i64,
            is_sidechain: o
                .get("isSidechain")
                .and_then(Value::as_bool)
                .unwrap_or(false) as i64,
            cwd: row_cwd,
        });
    }

    let before = session_count(conn, &sid)?;
    if !rows.is_empty() {
        // Immediate: take the write lock at BEGIN (where busy_timeout applies)
        // instead of risking a mid-transaction SQLITE_BUSY that bypasses it.
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO turns\
                 (session_id,seq,uuid,ts,role,text,is_meta,is_sidechain,project,cwd) \
                 VALUES (?,?,?,?,?,?,?,?,?,?)",
            )?;
            for r in &rows {
                stmt.execute(params![
                    sid,
                    r.seq,
                    r.uuid,
                    r.ts,
                    r.role,
                    r.text,
                    r.is_meta,
                    r.is_sidechain,
                    project,
                    r.cwd
                ])?;
            }
        }
        tx.commit()?;
    }
    let inserted = session_count(conn, &sid)? - before;

    let (num_turns, last_seq): (i64, i64) = conn.query_row(
        "SELECT count(*), COALESCE(max(seq), -1) FROM turns WHERE session_id=?",
        [&sid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let fsize = fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
    // first_ts/last_ts are MIN/MAX, not COALESCE-overwrite: an incremental pass only
    // sees the newly appended records, so its local min would otherwise clobber the
    // true session start. NULL-safe so a pass with no timestamps keeps the stored one.
    conn.execute(
        "INSERT INTO sessions \
           (session_id,file_path,project,cwd,first_ts,last_ts,num_turns,last_seq,file_size,scan_offset,scan_seq,ingested_at) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?) \
         ON CONFLICT(session_id) DO UPDATE SET \
           file_path=excluded.file_path, \
           project=excluded.project, \
           cwd=COALESCE(excluded.cwd, sessions.cwd), \
           first_ts=CASE \
             WHEN sessions.first_ts IS NULL THEN excluded.first_ts \
             WHEN excluded.first_ts IS NULL THEN sessions.first_ts \
             WHEN excluded.first_ts < sessions.first_ts THEN excluded.first_ts \
             ELSE sessions.first_ts END, \
           last_ts=CASE \
             WHEN sessions.last_ts IS NULL THEN excluded.last_ts \
             WHEN excluded.last_ts IS NULL THEN sessions.last_ts \
             WHEN excluded.last_ts > sessions.last_ts THEN excluded.last_ts \
             ELSE sessions.last_ts END, \
           num_turns=excluded.num_turns, \
           last_seq=excluded.last_seq, \
           file_size=excluded.file_size, \
           scan_offset=excluded.scan_offset, \
           scan_seq=excluded.scan_seq, \
           ingested_at=excluded.ingested_at",
        params![
            sid,
            path.to_string_lossy(),
            project,
            cwd,
            first_ts,
            last_ts,
            num_turns,
            last_seq,
            fsize,
            offset as i64,
            seq,
            now_iso()
        ],
    )?;
    // Auto-derive read-only tags from the stored (already-redacted) turns — but only
    // when this pass added turns. Tags are a pure function of the stored set, so an
    // incremental no-op pass can skip the full re-derive. Swallow-and-log: a tagging
    // failure must never fail an ingest that already stored its turns. ingest_file is
    // the single funnel for every write path (sweep / SessionEnd / PreCompact / Stop / CLI).
    if inserted > 0 {
        if let Err(e) = crate::tags::derive_tags(conn, &sid) {
            eprintln!("[subrosa] tag derivation {sid}: {e}");
        }
    }
    Ok((inserted, scanned))
}

/// Ingest any transcript whose size changed since last archive (catch-up for a
/// missed SessionEnd). Returns (files_seen, files_ingested, turns_inserted).
pub fn sweep(conn: &Connection, root: &Path) -> Result<(i64, i64, i64), Box<dyn Error>> {
    if !root.exists() {
        return Ok((0, 0, 0));
    }
    let mut seen: HashMap<String, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT session_id, file_size FROM sessions")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            seen.insert(r.get(0)?, r.get(1)?);
        }
    }
    let mut transcripts = Vec::new();
    for project_dir in fs::read_dir(root)? {
        let Ok(project_dir) = project_dir else {
            continue;
        };
        let p = project_dir.path();
        if !p.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&p) else {
            continue;
        };
        for f in entries.flatten() {
            let fp = f.path();
            if fp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                // DirEntry metadata — no second stat per transcript later.
                let Ok(md) = f.metadata() else { continue };
                transcripts.push((fp, md.len() as i64));
            }
        }
    }
    transcripts.sort();

    let (mut files, mut ingested, mut inserted_total) = (0, 0, 0);
    for (path, size) in transcripts {
        files += 1;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if seen.get(&stem) == Some(&size) {
            continue; // unchanged since last ingest
        }
        let (ins, _) = ingest_file(conn, &path)?;
        ingested += 1;
        inserted_total += ins;
    }
    Ok((files, ingested, inserted_total))
}

/// Append a session to the checkpoint queue — but only when it's worth distilling
/// and isn't already queued or already checkpointed. Idempotent: the SessionEnd
/// hook fires repeatedly on resume. Returns: queued | pruned | unchanged | duplicate.
pub fn enqueue_checkpoint(
    conn: &Connection,
    sid: &str,
    log_path: &Path,
) -> Result<&'static str, Box<dyn Error>> {
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT COALESCE(last_seq,-1), COALESCE(checkpointed_seq,-1) \
             FROM sessions WHERE session_id=?",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let user_turns: i64 = conn.query_row(
        "SELECT count(*) FROM turns WHERE session_id=? AND role='user' AND is_sidechain=0",
        [sid],
        |r| r.get(0),
    )?;
    let Some((last_seq, checkpointed_seq)) = row else {
        return Ok("pruned"); // not yet ingested
    };
    if user_turns < 1 {
        return Ok("pruned"); // empty / sub-agent-only / bare slash-command
    }
    if last_seq <= checkpointed_seq {
        return Ok("unchanged"); // already checkpointed and hasn't grown past the mark
    }
    let pending: HashSet<String> = fs::read_to_string(log_path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.rsplit('\t').next().unwrap_or(l).trim().to_string())
        .collect();
    if pending.contains(sid) {
        return Ok("duplicate");
    }
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_path)?;
    writeln!(f, "{}\t{}", now_iso(), sid)?;
    Ok("queued")
}
