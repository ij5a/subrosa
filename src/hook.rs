//! Claude Code hook entrypoints. Mechanical only — read the hook JSON on
//! stdin, do the work, log to the data dir, and always exit 0 so a memory
//! problem can never block a session. Stdout is reserved for intentional
//! context injection (session-start nudge, recall hits); errors go to the
//! log, never the session. Never spawns `claude` (recursion).

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use crate::{db, ingest, paths, recall, HookEvent};

/// Warn when a project's always-loaded MEMORY.md grows past this size.
const MEMORY_MD_WARN_BYTES: u64 = 23000;

pub fn run(event: HookEvent) -> ExitCode {
    let mut raw = String::new();
    // Cap the payload read — a runaway paste must not balloon the hook process.
    let _ = std::io::stdin()
        .take(8 * 1024 * 1024)
        .read_to_string(&mut raw);
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    let result = match event {
        HookEvent::SessionStart => session_start(&input),
        HookEvent::SessionEnd => session_end(&input),
        HookEvent::UserPromptSubmit => user_prompt_submit(&input),
        HookEvent::PreCompact => pre_compact(&input),
        HookEvent::Stop => stop(&input),
    };
    if let Err(e) = result {
        let name = match event {
            HookEvent::SessionStart => "session-start",
            HookEvent::SessionEnd => "session-end",
            HookEvent::UserPromptSubmit => "user-prompt-submit",
            HookEvent::PreCompact => "pre-compact",
            HookEvent::Stop => "stop",
        };
        log(&format!("{name} error: {e}"));
    }
    ExitCode::SUCCESS
}

/// Catch-up ingest of any transcript that grew since the last archive
/// (covers a missed or hard-killed SessionEnd), then print the nudge.
fn session_start(input: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let sweep_result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let conn = db::connect()?;
        let (files, ingested, inserted) = ingest::sweep(&conn, &paths::projects_dir())?;
        log(&format!(
            "session-start sweep: {files} transcripts, {ingested} changed, +{inserted} turns"
        ));
        Ok(())
    })();
    // The nudge is independent of sweep health — print whatever applies.
    let lines = nudge_lines(input);
    if !lines.is_empty() {
        println!("{}", lines.join("\n"));
        log(&format!("session-start nudge: {} line(s)", lines.len()));
    }
    sweep_result
}

/// Short, actionable nudge (stdout is injected into context). Stays silent
/// unless there's something to act on: sessions awaiting checkpoint, or a
/// project MEMORY.md approaching the always-loaded byte cap.
fn nudge_lines(input: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(paths::pending_log()) {
        // Dedupe by session id — a session can fire SessionEnd more than once.
        let seen: HashSet<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.rsplit('\t').next().unwrap_or(l))
            .collect();
        let n = seen.len();
        if n > 0 {
            out.push(format!(
                "[subrosa] {n} session(s) queued for checkpoint — run /subrosa:checkpoint-backlog \
                 to save them to memory now (in-session; handles up to 5, clears them as it finishes)."
            ));
        }
    }
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });
    if let Some(cwd) = cwd {
        let mm = paths::projects_dir()
            .join(db::encode_cwd(&cwd))
            .join("memory")
            .join("MEMORY.md");
        if let Ok(md) = std::fs::metadata(&mm) {
            if md.len() > MEMORY_MD_WARN_BYTES {
                out.push(format!(
                    "[subrosa] MEMORY.md is {:.1}KB (>{}KB) — near the always-loaded cap; \
                     trim index hooks or archive stale facts.",
                    md.len() as f64 / 1024.0,
                    MEMORY_MD_WARN_BYTES / 1000
                ));
            }
        }
    }
    out
}

/// Archive the just-ended transcript and queue its session id for checkpointing.
/// Steps are isolated: a lock failure in one must not skip the others.
fn session_end(input: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let conn = db::connect()?;
    if let Some(tp) = input.get("transcript_path").and_then(Value::as_str) {
        let p = Path::new(tp);
        if p.is_file() {
            match ingest::ingest_file(&conn, p) {
                Ok((inserted, scanned)) => log(&format!(
                    "session-end ingest {tp}: +{inserted} turns ({scanned} records scanned)"
                )),
                Err(e) => log(&format!("session-end ingest error {tp}: {e}")),
            }
        }
    }
    if let Some(sid) = input.get("session_id").and_then(Value::as_str) {
        match ingest::enqueue_checkpoint(&conn, sid, &paths::pending_log()) {
            Ok(status) => log(&format!("session-end enqueue {sid}: {status}")),
            Err(e) => log(&format!("session-end enqueue error {sid}: {e}")),
        }
    }
    // Throttled snapshot — no-op unless the newest backup is >24h old.
    match crate::backup::throttled(&conn) {
        Ok(Some(label)) => log(&format!("session-end backup: {label}")),
        Ok(None) => {}
        Err(e) => log(&format!("session-end backup error: {e}")),
    }
    Ok(())
}

/// Compaction is about to summarize the conversation away: archive the
/// transcript as it stands, and forget this session's recall dedup — the
/// injected blocks die with the old context, so re-injection is useful again.
/// Stdout stays empty (PreCompact stdout is not context), and exiting 0 is
/// load-bearing: exit 2 would BLOCK the compaction.
fn pre_compact(input: &Value) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(tp) = input.get("transcript_path").and_then(Value::as_str) {
        let p = Path::new(tp);
        if p.is_file() {
            let conn = db::connect()?;
            match ingest::ingest_file(&conn, p) {
                Ok((inserted, scanned)) => log(&format!(
                    "pre-compact ingest {tp}: +{inserted} turns ({scanned} records scanned)"
                )),
                Err(e) => log(&format!("pre-compact ingest error {tp}: {e}")),
            }
        }
    }
    if let Some(sid) = input.get("session_id").and_then(Value::as_str) {
        recall::forget_session(&paths::recall_seen_log(), sid);
        log(&format!("pre-compact recall-dedup reset {sid}"));
    }
    Ok(())
}

/// Stop fires after each assistant turn: incrementally ingest the in-progress
/// transcript so the live session is searchable before SessionEnd. Ingest only
/// — no checkpoint enqueue, backup, or recall reset. Exit 0 always; a non-zero
/// exit would block the turn from ending.
fn stop(input: &Value) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(tp) = input.get("transcript_path").and_then(Value::as_str) {
        let p = Path::new(tp);
        if p.is_file() {
            let conn = db::connect()?;
            match ingest::ingest_file(&conn, p) {
                Ok((inserted, scanned)) => log(&format!(
                    "stop ingest {tp}: +{inserted} turns ({scanned} records scanned)"
                )),
                Err(e) => log(&format!("stop ingest error {tp}: {e}")),
            }
        }
    }
    Ok(())
}

/// Inject recall hits for the prompt (stdout is added to context). Read-only,
/// quiet on any problem — a recall miss must never slow or block a prompt.
fn user_prompt_submit(input: &Value) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(text) = recall::run(input) {
        println!("{text}");
        log(&format!(
            "user-prompt-submit recall: {} hit(s)",
            text.lines().count().saturating_sub(1)
        ));
    }
    Ok(())
}

/// Append one line to the hook log in the data dir. Best-effort: never fails.
fn log(msg: &str) {
    let path = paths::hook_log();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {}", db::now_iso(), msg);
    }
}
