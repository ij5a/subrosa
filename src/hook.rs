//! Claude Code hook entrypoints. Mechanical only — read the hook JSON on stdin,
//! do the archive work, log to the data dir, and always exit 0 so a memory
//! problem can never block a session. Never spawns `claude` (recursion).

use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use crate::{db, ingest, paths, HookEvent};

pub fn run(event: HookEvent) -> ExitCode {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    let result = match event {
        HookEvent::SessionStart => session_start(),
        HookEvent::SessionEnd => session_end(&input),
    };
    if let Err(e) = result {
        let name = match event {
            HookEvent::SessionStart => "session-start",
            HookEvent::SessionEnd => "session-end",
        };
        log(&format!("{name} error: {e}"));
    }
    ExitCode::SUCCESS
}

/// Catch-up ingest of any transcript that grew since the last archive
/// (covers a missed or hard-killed SessionEnd).
fn session_start() -> Result<(), Box<dyn std::error::Error>> {
    let conn = db::connect()?;
    let (files, ingested, inserted) = ingest::sweep(&conn, &paths::projects_dir())?;
    log(&format!(
        "session-start sweep: {files} transcripts, {ingested} changed, +{inserted} turns"
    ));
    Ok(())
}

/// Archive the just-ended transcript and queue its session id for checkpointing.
fn session_end(input: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let conn = db::connect()?;
    if let Some(tp) = input.get("transcript_path").and_then(Value::as_str) {
        let p = Path::new(tp);
        if p.is_file() {
            let (inserted, scanned) = ingest::ingest_file(&conn, p)?;
            log(&format!(
                "session-end ingest {tp}: +{inserted} turns ({scanned} records scanned)"
            ));
        }
    }
    if let Some(sid) = input.get("session_id").and_then(Value::as_str) {
        let status = ingest::enqueue_checkpoint(&conn, sid, &paths::pending_log())?;
        log(&format!("session-end enqueue {sid}: {status}"));
    }
    // Throttled snapshot — no-op unless the newest backup is >24h old.
    match crate::backup::throttled(&conn) {
        Ok(Some(label)) => log(&format!("session-end backup: {label}")),
        Ok(None) => {}
        Err(e) => log(&format!("session-end backup error: {e}")),
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
