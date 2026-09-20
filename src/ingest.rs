//! Transcript JSONL → turns rows: keeps real user prompts and assistant
//! text/thinking, compacts tool_use to name + args, keeps only a short head
//! of tool_result, drops meta wrappers. The stored-text format is pinned
//! byte-for-byte by golden tests — existing archives must re-ingest cleanly.

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

pub struct IngestReport {
    pub inserted: i64,
    pub scanned: i64,
    pub skipped: i64,
    pub partial_tail: bool,
    pub complete: bool,
}

/// Parse one transcript JSONL and upsert its turns + session row. Idempotent.
/// Returns (inserted, scanned).
pub fn ingest_file_report(
    conn: &Connection,
    path: &Path,
    require_complete: bool,
) -> Result<IngestReport, Box<dyn Error>> {
    if !path.exists() {
        return Ok(IngestReport {
            inserted: 0,
            scanned: 0,
            skipped: 0,
            partial_tail: false,
            complete: true,
        });
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
    let (resume_offset, resume_seq, stored_skipped, stored_size, stored_mtime, stored_turns): (u64, i64, i64, u64, i64, i64) = conn
        .query_row(
            "SELECT scan_offset, scan_seq, skipped_lines, partial_tail, file_size, file_mtime, num_turns FROM sessions WHERE session_id=?",
            [&sid],
            |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?, r.get(2)?, r.get::<_, i64>(4)?.max(0) as u64, r.get(5)?, r.get(6)?)),
        )
        .optional()?
        .unwrap_or((0, 0, 0, 0, 0, 0));
    let stored_high_water = resume_seq - 1;

    let mut file = fs::File::open(path)?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let file_mtime = file
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    let strict_read = require_complete;
    // All 4 checks prevent a stale or corrupt cursor from hiding new content.
    if !strict_read
        && stored_turns > 0
        && stored_size == file_len
        && stored_mtime == file_mtime
        && resume_offset == file_len
    {
        return Ok(IngestReport {
            inserted: 0,
            scanned: 0,
            skipped: stored_skipped,
            partial_tail: false,
            complete: stored_skipped == 0,
        });
    }
    let archived_texts: std::collections::HashMap<i64, String> = if strict_read {
        conn.prepare("SELECT seq, text FROM turns WHERE session_id=?")?
            .query_map([&sid], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    } else {
        std::collections::HashMap::new()
    };
    let reset_for_shorter_file = stored_size > file_len;
    let (mut offset, mut seq) = if strict_read || reset_for_shorter_file {
        (0, 0)
    } else if stored_turns > 0
        && stored_size > 0
        && stored_size < file_len
        && stored_mtime <= file_mtime
        && resume_offset < file_len
    {
        (resume_offset, resume_seq)
    } else {
        (0, 0)
    };
    if strict_read && stored_size > 0 && file_len < stored_size {
        conn.execute(
            "UPDATE sessions SET skipped_lines=MAX(skipped_lines, 1), partial_tail=0 WHERE session_id=?",
            [&sid],
        )?;
        eprintln!(
            "[subrosa] {}: content changed rather than grew; refusing to merge it",
            path.display()
        );
        return Ok(IngestReport {
            inserted: 0,
            scanned: 0,
            skipped: stored_skipped.max(1),
            partial_tail: false,
            complete: false,
        });
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut reader = BufReader::new(file);

    let mut rows: Vec<Row> = Vec::new();
    let (mut first_ts, mut last_ts, mut cwd): (Option<String>, Option<String>, Option<String>) =
        (None, None, None);
    let mut scanned: i64 = 0;
    let mut skipped: i64 = stored_skipped;
    let mut partial_tail = false;
    let mut changed = false;
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
            partial_tail = true;
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
            if strict_read && i <= stored_high_water && archived_texts.contains_key(&i) {
                changed = true;
            }
            continue;
        }
        let Ok(o) = serde_json::from_str::<Value>(line) else {
            skipped += 1;
            if strict_read && i <= stored_high_water && archived_texts.contains_key(&i) {
                changed = true;
            }
            continue;
        };
        scanned += 1;
        let Some((role, text)) = flatten_record(&o) else {
            if strict_read && i <= stored_high_water && archived_texts.contains_key(&i) {
                changed = true;
            }
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

        if strict_read {
            let current_text = &rows.last().expect("row just pushed").text;
            let changed_archived = archived_texts
                .get(&i)
                .is_some_and(|archived| archived != current_text);
            // A filtered record below the stored high-water mark is a rewrite, not an append.
            let changed_filtered = i <= stored_high_water && !archived_texts.contains_key(&i);
            if changed_archived || changed_filtered {
                changed = true;
            }
        }
    }

    if changed {
        conn.execute(
            "UPDATE sessions SET skipped_lines=MAX(skipped_lines, 1), partial_tail=0 WHERE session_id=?",
            [&sid],
        )?;
        eprintln!(
            "[subrosa] {}: content changed rather than grew; refusing to merge it",
            path.display()
        );
        return Ok(IngestReport {
            inserted: 0,
            scanned: 0,
            skipped: stored_skipped.max(1),
            partial_tail: false,
            complete: false,
        });
    }

    let mut inserted = 0;
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
                inserted += stmt.execute(params![
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
                ])? as i64;
            }
        }
        tx.commit()?;
    }

    let (num_turns, last_seq): (i64, i64) = conn.query_row(
        "SELECT count(*), COALESCE(max(seq), -1) FROM turns WHERE session_id=?",
        [&sid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let fsize = fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
    partial_tail = partial_tail || (fsize as u64) > offset;
    let complete = !partial_tail && fsize as u64 == offset;
    // first_ts/last_ts are MIN/MAX, not COALESCE-overwrite: an incremental pass only
    // sees the newly appended records, so its local min would otherwise clobber the
    // true session start. NULL-safe so a pass with no timestamps keeps the stored one.
    // Cursors only move forward so a rewrite cannot make later content look new.
    conn.execute(
        "INSERT INTO sessions \
           (session_id,file_path,project,cwd,first_ts,last_ts,num_turns,last_seq,file_size,file_mtime,scan_offset,scan_seq,skipped_lines,partial_tail,ingested_at) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) \
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
           file_mtime=excluded.file_mtime, \
           scan_offset=CASE WHEN excluded.file_size < sessions.file_size THEN excluded.scan_offset ELSE MAX(sessions.scan_offset, excluded.scan_offset) END, \
           scan_seq=CASE WHEN excluded.file_size < sessions.file_size THEN excluded.scan_seq ELSE MAX(sessions.scan_seq, excluded.scan_seq) END, \
           skipped_lines=excluded.skipped_lines, \
           partial_tail=excluded.partial_tail, \
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
            file_mtime,
            offset as i64,
            seq,
            skipped,
            partial_tail as i64,
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
    Ok(IngestReport {
        inserted,
        scanned,
        skipped,
        partial_tail,
        complete: complete && skipped == 0,
    })
}

pub fn ingest_file(conn: &Connection, path: &Path) -> Result<(i64, i64), Box<dyn Error>> {
    let report = ingest_file_report(conn, path, false)?;
    Ok((report.inserted, report.scanned))
}

/// Ingest any transcript whose size changed since last archive (catch-up for a
/// missed SessionEnd). Returns (files_seen, files_ingested, turns_inserted).
pub fn sweep(
    conn: &Connection,
    root: &Path,
    require_complete: bool,
) -> Result<(i64, i64, i64, bool), Box<dyn Error>> {
    let mut transcripts = Vec::new();
    for project_dir in fs::read_dir(root)? {
        let project_dir = project_dir?;
        let p = project_dir.path();
        if !p.is_dir() {
            continue;
        }
        let entries = fs::read_dir(&p)?;
        for f in entries {
            let f = f?;
            let fp = f.path();
            if fp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                // DirEntry metadata — no second stat per transcript later.
                transcripts.push(fp);
            }
        }
    }
    transcripts.sort();

    let (mut files, mut ingested, mut inserted_total, mut complete) = (0, 0, 0, true);
    for path in transcripts {
        files += 1;
        match ingest_file_report(conn, &path, require_complete) {
            Ok(report) => {
                let ins = report.inserted;
                ingested += 1;
                inserted_total += ins;
                complete &= report.complete;
            }
            Err(e) => return Err(format!("sweep failed for {path:?}: {e}").into()),
        }
    }
    let queued: Vec<String> = conn
        .prepare(
            "SELECT s.session_id FROM sessions s \
             LEFT JOIN checkpoint_queue q ON q.session_id=s.session_id \
             WHERE q.session_id IS NULL AND COALESCE((SELECT max(seq) FROM turns WHERE session_id=s.session_id), -1) > COALESCE(s.checkpointed_seq, -1)
               AND EXISTS (SELECT 1 FROM turns t WHERE t.session_id=s.session_id)",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for sid in queued {
        enqueue_checkpoint(conn, &sid)?;
    }
    Ok((files, ingested, inserted_total, complete))
}

pub fn distilled_boundary(conn: &Connection, sid: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE((SELECT max(seq) FROM turns WHERE session_id=?), -1)",
        [sid],
        |r| r.get(0),
    )
}

pub fn advance_checkpoint(conn: &Connection, sid: &str, boundary: i64) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE sessions SET checkpointed_seq=MAX(COALESCE(checkpointed_seq, -1), ?) WHERE session_id=?",
        rusqlite::params![boundary, sid],
    )
}

/// Append a session to the checkpoint queue — but only when it's worth distilling
/// and isn't already queued or already checkpointed. Idempotent: the SessionEnd
/// hook fires repeatedly on resume. Returns: queued | pruned | unchanged | duplicate.
pub fn enqueue_checkpoint(conn: &Connection, sid: &str) -> Result<&'static str, Box<dyn Error>> {
    let user_turns: i64 = conn.query_row(
        "SELECT count(*) FROM turns WHERE session_id=? AND role='user' AND is_sidechain=0",
        [sid],
        |r| r.get(0),
    )?;
    if user_turns < 1 {
        return Ok("pruned");
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let row: Option<(i64, i64)> = tx
        .query_row(
            "SELECT COALESCE((SELECT max(seq) FROM turns WHERE session_id=s.session_id), -1), COALESCE(s.checkpointed_seq,-1) \
             FROM sessions s WHERE s.session_id=?",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((last_seq, checkpointed_seq)) = row else {
        tx.commit()?;
        return Ok("pruned"); // not yet ingested
    };
    let scan_reset: bool = tx.query_row(
        "SELECT scan_offset=0 AND scan_seq=0 FROM sessions WHERE session_id=?",
        [sid],
        |r| r.get(0),
    )?;
    if last_seq <= checkpointed_seq && !scan_reset {
        tx.commit()?;
        return Ok("unchanged"); // already checkpointed and hasn't grown past the mark
    }
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO checkpoint_queue(session_id, enqueued_at, enqueue_seq) VALUES (?, ?, (SELECT COALESCE(MAX(enqueue_seq), 0) + 1 FROM checkpoint_queue))",
        rusqlite::params![sid, now_iso()],
    )?;
    tx.commit()?;
    Ok(if inserted == 0 { "duplicate" } else { "queued" })
}
