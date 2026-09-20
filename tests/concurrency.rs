//! Concurrency: session-end hooks firing together (several windows closing at
//! once) must never error, lose a queue entry, or fail the first snapshot.
//! Regression tests for a "database is locked" hook-log error.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

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

fn user_record(sid: &str) -> String {
    format!(
        r#"{{"type":"user","timestamp":"2026-06-12T01:00:00Z","uuid":"{sid}-u1","cwd":"/tmp/demo","message":{{"role":"user","content":"survive"}}}}"#
    )
}

// This nudges overlap but does not force it, so the test can pass without the race.
#[test]
fn concurrent_schema_upgrades_both_succeed() {
    let env = setup("schema-upgrades");
    let first = base_cmd(&env)
        .args(["init"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(25));
    let second = base_cmd(&env)
        .args(["init"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(first.wait_with_output().unwrap().status.success());
    assert!(second.wait_with_output().unwrap().status.success());
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
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pending =
            String::from_utf8(base_cmd(&env).args(["pending"]).output().unwrap().stdout).unwrap();
        if sessions
            .iter()
            .all(|(sid, _)| pending.contains(sid.as_str()))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sessions missing from queue:\n{pending}"
        );
        std::thread::sleep(Duration::from_millis(50));
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while !env.data.join("memory.db").is_file() {
        assert!(
            Instant::now() < deadline,
            "SessionEnd worker did not create the database"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

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

#[test]
fn session_end_returns_fast_and_worker_queues_after_contention() {
    let env = setup("session-end-worker");
    let sid = "worker-0001";
    let transcript = write_transcript(&env, sid, 2);
    let warm = base_cmd(&env).args(["init"]).output().unwrap();
    assert!(warm.status.success());
    let holder = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let blocker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        holder.execute_batch("COMMIT").unwrap();
    });
    let started = Instant::now();
    let mut hook = spawn_session_end(&env, sid, &transcript);
    assert!(hook.wait().unwrap().success());
    assert!(started.elapsed() < Duration::from_secs(2));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pending = base_cmd(&env).args(["pending"]).output().unwrap();
        let text = String::from_utf8_lossy(&pending.stdout);
        if text.contains(sid) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not queue {sid}: {text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    blocker.join().unwrap();
}

#[cfg(unix)]
#[test]
fn session_end_worker_survives_parent_process_group_exit() {
    let env = setup("session-end-process-group");
    let sid = "group-worker-0001";
    let transcript = env.projects.join(format!("-tmp-demo/{sid}.jsonl"));
    fs::write(&transcript, user_record(sid) + "\n").unwrap();

    let payload = format!(
        r#"{{"session_id":"{sid}","transcript_path":"{}","reason":"other"}}"#,
        transcript.display()
    );
    let mut parent = Command::new("sh")
        .args([
            "-c",
            "printf '%s' \"$SUBROSA_TEST_PAYLOAD\" | \"$SUBROSA_BIN\" hook session-end; sleep 5",
        ])
        .env("SUBROSA_BIN", bin())
        .env("SUBROSA_TEST_PAYLOAD", payload)
        .env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
        .env("SUBROSA_SEMANTIC", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let group = format!("-{}", parent.id());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !env.data.join("memory.db").is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = Command::new("kill").args(["-TERM", &group]).status();
    // The hook may exit normally before the signal arrives, and either outcome is valid.
    let _ = parent.wait();
    for _ in 0..100 {
        let output = base_cmd(&env).args(["pending"]).output().unwrap();
        let pending = String::from_utf8_lossy(&output.stdout);
        if pending.contains(sid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("session-end worker did not survive its parent process group");
}

#[test]
fn sweep_recovers_when_session_end_worker_never_runs() {
    let env = setup("session-end-sweep");
    let sid = "sweep-0001";
    let transcript = write_transcript(&env, sid, 2);
    let init = base_cmd(&env).args(["init"]).output().unwrap();
    assert!(init.status.success());
    let ingest = base_cmd(&env)
        .args(["ingest", transcript.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(ingest.status.success());
    let drop = base_cmd(&env)
        .args(["checkpoint-drop", sid])
        .output()
        .unwrap();
    assert!(drop.status.success());
    let sweep = base_cmd(&env).args(["sweep", "--quiet"]).output().unwrap();
    assert!(sweep.status.success());
    let pending = base_cmd(&env).args(["pending"]).output().unwrap();
    assert!(String::from_utf8_lossy(&pending.stdout).contains(sid));
}
