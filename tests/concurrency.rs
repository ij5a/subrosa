//! Concurrency: session-end hooks firing together (several windows closing at
//! once) must never error, lose a queue entry, or fail the first snapshot.
//! Regression tests for a "database is locked" hook-log error.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_subrosa")
}

struct TestEnv {
    data: PathBuf,
    projects: PathBuf,
}

fn setup(tag: &str) -> TestEnv {
    let root = std::env::temp_dir().join(format!("subrosa-conc-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let data = root.join("data");
    let projects = root.join("projects");
    fs::create_dir_all(projects.join("-tmp-demo")).unwrap();
    fs::create_dir_all(&data).unwrap();
    TestEnv { data, projects }
}

fn write_transcript(env: &TestEnv, sid: &str, turns: usize) -> PathBuf {
    let p = env.projects.join("-tmp-demo").join(format!("{sid}.jsonl"));
    let mut f = fs::File::create(&p).unwrap();
    for i in 0..turns {
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-06-12T01:{:02}:{:02}Z","uuid":"{sid}-u{i}","cwd":"/tmp/demo","message":{{"role":"user","content":"turn {i} of {sid} — concurrent ingest exercise"}}}}"#,
            (i / 60) % 60,
            i % 60
        )
        .unwrap();
    }
    p
}

/// A child pointed at the throwaway dirs, with EVERY inherited SUBROSA_*
/// dropped first. An exported SUBROSA_DB would aim these toy sessions at the
/// real archive, and an exported SUBROSA_MIRROR plus a passphrase would let
/// the mirror purge delete a real file.
fn base_cmd(env: &TestEnv) -> Command {
    let mut cmd = Command::new(bin());
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("SUBROSA_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
        // Never let a test start the background indexer — it downloads a model.
        .env("SUBROSA_SEMANTIC", "off");
    cmd
}

fn spawn_session_end(env: &TestEnv, sid: &str, transcript: &Path) -> Child {
    let mut child = base_cmd(env)
        .args(["hook", "session-end"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let payload = format!(
        r#"{{"session_id":"{sid}","transcript_path":"{}","reason":"other"}}"#,
        transcript.display()
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child
}

#[test]
fn concurrent_session_end_hooks_never_error() {
    let env = setup("hooks");
    let sessions: Vec<(String, PathBuf)> = (0..4)
        .map(|n| {
            let sid = format!("cccc-dddd-{n:04}");
            let p = write_transcript(&env, &sid, 400);
            (sid, p)
        })
        .collect();

    // Two hooks per session: duplicate SessionEnd firings are part of the real
    // incident shape (closed-then-resumed windows re-fire the same session).
    let mut children = Vec::new();
    for _round in 0..2 {
        for (sid, p) in &sessions {
            children.push(spawn_session_end(&env, sid, p));
        }
    }
    for mut c in children {
        assert!(c.wait().unwrap().success(), "a hook exited non-zero");
    }

    let log = fs::read_to_string(env.data.join("hook.log")).unwrap_or_default();
    assert!(
        !log.contains("error"),
        "hook.log reports errors under concurrency:\n{log}"
    );
    let pending = fs::read_to_string(env.data.join("pending-checkpoint.log")).unwrap_or_default();
    for (sid, _) in &sessions {
        assert!(
            pending.contains(sid.as_str()),
            "{sid} missing from queue:\n{pending}"
        );
    }
    // No prior snapshot existed, so the un-throttled backup ran while other
    // hooks were writing — it must have completed despite the contention.
    let snaps: Vec<PathBuf> = fs::read_dir(env.data.join("backups"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(!snaps.is_empty(), "no snapshot survived the concurrent run");
}

#[test]
fn connect_needs_no_write_lock_when_schema_current() {
    let env = setup("rdonly");
    // Warm-up creates the schema and sets user_version once.
    let mut warm = spawn_session_end(&env, "warm-0000", Path::new("/nonexistent"));
    assert!(warm.wait().unwrap().success());

    // Hold the write lock, then run a session-start (connect + sweep of an
    // empty projects dir = reads only). It must finish while the lock is held.
    let holder = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    holder
        .execute("INSERT INTO sessions(session_id) VALUES('lock-holder')", [])
        .unwrap();

    let mut child = base_cmd(&env)
        .args(["hook", "session-start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let finished = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    holder.execute_batch("COMMIT").unwrap();

    let status =
        finished.expect("connect() blocked on the write lock — no-op user_version write is back");
    assert!(status.success());
    let log = fs::read_to_string(env.data.join("hook.log")).unwrap_or_default();
    assert!(
        !log.contains("error"),
        "session-start errored while a writer held the lock:\n{log}"
    );
}
