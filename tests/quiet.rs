//! Token-economy guards: recall needs an anchor-grade term, never re-injects
//! a source session into the same live session, nudge text never re-enters
//! the archive, and index hook lines stay capped.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_subrosa")
}

struct TestEnv {
    data: PathBuf,
    projects: PathBuf,
    /// Working directory for the child. Override it to test paths that are
    /// resolved relative to where the command was run.
    cwd: PathBuf,
}

fn setup(tag: &str) -> TestEnv {
    let root = std::env::temp_dir().join(format!("subrosa-quiet-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let data = root.join("data");
    let projects = root.join("projects");
    fs::create_dir_all(projects.join("-tmp-demo")).unwrap();
    fs::create_dir_all(&data).unwrap();
    TestEnv {
        cwd: data.clone(),
        data,
        projects,
    }
}

fn run(env: &TestEnv, args: &[&str], stdin: Option<&str>) -> (String, String) {
    let (out, err, _) = run_env::<&str>(env, args, stdin, &[]);
    (out, err)
}

/// Same, plus extra env pairs and the exit status. EVERY inherited `SUBROSA_*`
/// variable is dropped first — `SUBROSA_DB` in a developer's shell outranks
/// `SUBROSA_DIR` and would point the suite at a real database. The cwd is the
/// throwaway dir too, so a stray output file can't land in the repo.
fn run_env<V: AsRef<std::ffi::OsStr>>(
    env: &TestEnv,
    args: &[&str],
    stdin: Option<&str>,
    extra: &[(&str, V)],
) -> (String, String, bool) {
    let mut cmd = Command::new(bin());
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("SUBROSA_") {
            cmd.env_remove(&k);
        }
    }
    cmd.args(args)
        .env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
        // No test may start the background indexer: it would download the
        // model and index the throwaway archive behind the suite's back.
        .env("SUBROSA_SEMANTIC", "off")
        .current_dir(&env.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.unwrap_or("").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn user_rec(ts: &str, uuid: &str, text: &str) -> String {
    format!(
        r#"{{"type":"user","timestamp":"{ts}","uuid":"{uuid}","cwd":"/tmp/demo","message":{{"role":"user","content":"{text}"}}}}"#
    )
}

fn drop_turns_table(env: &TestEnv) {
    run(env, &["init"], None);
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    db.execute("DROP TABLE turns", []).unwrap();
}

fn assert_post_open_write_fails(env: &TestEnv, stem: &str) {
    let transcript = env.projects.join(format!("-tmp-demo/{stem}.jsonl"));
    fs::write(
        &transcript,
        user_rec("2026-06-12T01:00:00Z", "write-1", "write failure"),
    )
    .unwrap();
    drop_turns_table(env);
    let (_, err, ok) = run_env::<&str>(env, &["ingest", transcript.to_str().unwrap()], None, &[]);
    assert!(!ok, "post-open write failure succeeded: {err}");
}

fn ingest(env: &TestEnv, stem: &str, records: &[String]) {
    let t = env.projects.join(format!("-tmp-demo/{stem}.jsonl"));
    fs::write(&t, records.join("\n") + "\n").unwrap();
    run(env, &["ingest", t.to_str().unwrap()], None);
}

#[test]
fn ingest_missing_path_fails() {
    let env = setup("ingest-missing");
    let missing = env.projects.join("-tmp-demo/missing.jsonl");
    let (out, err, ok) = run_env::<&str>(&env, &["ingest", missing.to_str().unwrap()], None, &[]);
    assert!(!ok, "missing ingest succeeded: {out}{err}");
    assert!(err.contains("does not exist"), "unexpected error: {err}");

    assert_post_open_write_fails(&env, "write-missing");
}

#[test]
fn ingest_existing_empty_transcript_succeeds() {
    let env = setup("ingest-empty");
    let transcript = env.projects.join("-tmp-demo/empty.jsonl");
    fs::write(&transcript, "").unwrap();
    let (out, err, ok) =
        run_env::<&str>(&env, &["ingest", transcript.to_str().unwrap()], None, &[]);
    assert!(ok, "empty ingest failed: {out}{err}");
    assert!(out.contains("+0 turns"), "unexpected output: {out}");

    assert_post_open_write_fails(&env, "write-empty");
}

#[test]
fn ingest_good_transcript_succeeds_and_archives_turns() {
    let env = setup("ingest-good");
    let transcript = env.projects.join("-tmp-demo/good.jsonl");
    fs::write(
        &transcript,
        user_rec("2026-06-12T01:00:00Z", "u1", "archive this") + "\n",
    )
    .unwrap();
    let (out, err, ok) =
        run_env::<&str>(&env, &["ingest", transcript.to_str().unwrap()], None, &[]);
    assert!(ok, "good ingest failed: {out}{err}");
    assert!(out.contains("+1 turns"), "unexpected output: {out}");
    let dump = run(&env, &["session", "good"], None).0;
    assert!(
        dump.contains("archive this"),
        "turn was not archived: {dump}"
    );

    assert_post_open_write_fails(&env, "write-good");
}

#[test]
fn ingest_database_error_fails() {
    let env = setup("ingest-db-error");
    let transcript = env.projects.join("-tmp-demo/db-error.jsonl");
    fs::write(&transcript, "").unwrap();
    let broken = env.data.join("not-a-db");
    fs::create_dir_all(&broken).unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["ingest", transcript.to_str().unwrap()],
        None,
        &[("SUBROSA_DB", broken.to_str().unwrap())],
    );
    assert!(!ok, "database error succeeded: {out}{err}");
    assert!(err.contains("cannot open DB"), "unexpected error: {err}");

    assert_post_open_write_fails(&env, "write-db-error");
}

#[test]
fn sweep_reports_unreadable_transcript() {
    let env = setup("sweep-unreadable");
    run(&env, &["init"], None);
    let bad = env.projects.join("-tmp-demo/bad.jsonl");
    fs::create_dir(&bad).unwrap();
    let (out, err, ok) = run_env::<&str>(&env, &["sweep"], None, &[]);
    assert!(!ok, "sweep succeeded: {out}{err}");
    assert!(
        err.contains("sweep failed"),
        "unexpected sweep error: {err}"
    );
}

#[test]
fn require_complete_stays_failed_after_malformed_retry() {
    let env = setup("require-retry");
    let transcript = env.projects.join("-tmp-demo/retry.jsonl");
    fs::write(&transcript, "not json\n").unwrap();
    for _ in 0..2 {
        let (_, err, ok) = run_env::<&str>(
            &env,
            &["ingest", "--require-complete", transcript.to_str().unwrap()],
            None,
            &[],
        );
        assert!(!ok, "malformed input succeeded: {err}");
    }
}

#[test]
fn require_complete_sweep_rejects_partial_tail() {
    let env = setup("require-sweep");
    let transcript = env.projects.join("-tmp-demo/partial.jsonl");
    fs::write(
        &transcript,
        user_rec("2026-06-12T01:00:00Z", "p1", "complete") + "\npartial",
    )
    .unwrap();
    let (_, err, ok) = run_env::<&std::ffi::OsStr>(
        &env,
        &["ingest", "--sweep", "--require-complete"],
        None,
        &[],
    );
    assert!(!ok, "partial sweep succeeded: {err}");
}

#[test]
fn normal_append_then_strict_check_passes() {
    let env = setup("strict-append");
    let tp = env.projects.join("-tmp-demo/strict-append.jsonl");
    fs::write(&tp, user_rec("2026-06-12T01:00:00Z", "a1", "first") + "\n").unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    fs::OpenOptions::new()
        .append(true)
        .open(&tp)
        .unwrap()
        .write_all(user_rec("2026-06-12T01:01:00Z", "a2", "second").as_bytes())
        .unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&tp)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    assert!(
        run_env::<&str>(
            &env,
            &["ingest", "--require-complete", tp.to_str().unwrap()],
            None,
            &[]
        )
        .2
    );
}

#[test]
fn strict_sweep_rejects_rewritten_transcript() {
    let env = setup("strict-sweep-rewrite");
    let tp = env.projects.join("-tmp-demo/strict-sweep-rewrite.jsonl");
    fs::write(
        &tp,
        user_rec("2026-06-12T01:00:00Z", "s1", "original")
            + "\n"
            + &user_rec("2026-06-12T01:01:00Z", "s2", "second")
            + "\n",
    )
    .unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    fs::write(
        &tp,
        user_rec("2026-06-12T01:00:00Z", "s1", "original")
            + "\n"
            + &user_rec("2026-06-12T01:01:00Z", "s2", "rewritten")
            + "\n",
    )
    .unwrap();
    let (_, _, ok) = run_env::<&str>(
        &env,
        &["ingest", "--sweep", "--require-complete"],
        None,
        &[],
    );
    assert!(!ok);
}

fn filtered_record() -> String {
    user_rec(
        "2026-06-12T01:01:00Z",
        "filtered",
        "<local-command-stdout> noise",
    )
}

#[test]
fn strict_rejects_rewritten_filtered_record_and_keeps_queue() {
    for sweep in [false, true] {
        let tag = if sweep {
            "strict-filtered-rewrite-sweep"
        } else {
            "strict-filtered-rewrite-direct"
        };
        let env = setup(tag);
        let tp = env.projects.join(format!("-tmp-demo/{tag}.jsonl"));
        fs::write(
            &tp,
            user_rec("2026-06-12T01:00:00Z", "first", "first")
                + "\n"
                + &filtered_record()
                + "\n"
                + &user_rec("2026-06-12T01:02:00Z", "third", "third")
                + "\n",
        )
        .unwrap();
        assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
        run(&env, &["checkpoint-enqueue", tag], None);

        fs::write(
            &tp,
            user_rec("2026-06-12T01:00:00Z", "first", "first")
                + "\n"
                + &user_rec("2026-06-12T01:01:00Z", "rewritten", "rewritten")
                + "\n"
                + &user_rec("2026-06-12T01:02:00Z", "third", "third")
                + "\n",
        )
        .unwrap();
        let args = if sweep {
            vec!["ingest", "--sweep", "--require-complete"]
        } else {
            vec!["ingest", "--require-complete", tp.to_str().unwrap()]
        };
        let (_, _, ok) = run_env::<&str>(&env, &args, None, &[]);
        assert!(!ok, "filtered rewrite passed for {tag}");
        let dump = run(&env, &["session", tag], None).0;
        assert!(dump.contains("first") && dump.contains("third"));
        assert!(!dump.contains("rewritten"));
        assert!(run(&env, &["pending"], None).0.contains(tag));
    }
}

#[test]
fn strict_append_after_filtered_record_passes() {
    let env = setup("strict-filtered-append");
    let tp = env.projects.join("-tmp-demo/strict-filtered-append.jsonl");
    fs::write(
        &tp,
        user_rec("2026-06-12T01:00:00Z", "first", "first") + "\n" + &filtered_record() + "\n",
    )
    .unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    fs::OpenOptions::new()
        .append(true)
        .open(&tp)
        .unwrap()
        .write_all((user_rec("2026-06-12T01:02:00Z", "third", "third") + "\n").as_bytes())
        .unwrap();
    assert!(
        run_env::<&str>(
            &env,
            &["ingest", "--require-complete", tp.to_str().unwrap()],
            None,
            &[]
        )
        .2
    );
}

#[test]
fn strict_filtered_record_still_passes_when_unchanged() {
    let env = setup("strict-filtered-stays");
    let tp = env.projects.join("-tmp-demo/strict-filtered-stays.jsonl");
    fs::write(
        &tp,
        user_rec("2026-06-12T01:00:00Z", "first", "first") + "\n" + &filtered_record() + "\n",
    )
    .unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    assert!(
        run_env::<&str>(
            &env,
            &["ingest", "--require-complete", tp.to_str().unwrap()],
            None,
            &[]
        )
        .2
    );
}

#[test]
fn strict_shorter_rewrite_keeps_rows_and_queue() {
    let env = setup("shorter-repair");
    let tp = env.projects.join("-tmp-demo/shorter-repair.jsonl");
    fs::write(
        &tp,
        user_rec("2026-06-12T01:00:00Z", "l1", "old one")
            + "\n"
            + &user_rec("2026-06-12T01:01:00Z", "l2", "old two")
            + "\n",
    )
    .unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    fs::write(
        &tp,
        user_rec("2026-06-12T02:00:00Z", "new", "short repaired") + "\n",
    )
    .unwrap();
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    db.execute("INSERT INTO checkpoint_queue(session_id, enqueued_at, enqueue_seq) VALUES ('shorter-repair', '2026-06-12T00:00:00Z', 1)", []).unwrap();
    assert!(
        !run_env::<&str>(
            &env,
            &["ingest", "--require-complete", tp.to_str().unwrap()],
            None,
            &[],
        )
        .2
    );
    let rows: Vec<String> = db
        .prepare("SELECT text FROM turns WHERE session_id='shorter-repair' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, vec!["old one", "old two"]);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM checkpoint_queue WHERE session_id='shorter-repair'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn normal_ingest_recovers_from_overlong_cursor() {
    let env = setup("overlong-cursor");
    let tp = env.projects.join("-tmp-demo/overlong-cursor.jsonl");
    fs::write(&tp, user_rec("2026-06-12T01:00:00Z", "one", "one") + "\n").unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    fs::write(
        &tp,
        user_rec("2026-06-12T01:00:00Z", "one", "one")
            + "\n"
            + &user_rec("2026-06-12T01:01:00Z", "two", "two")
            + "\n",
    )
    .unwrap();
    db.execute(
        "UPDATE sessions SET scan_offset=999999 WHERE session_id='overlong-cursor'",
        [],
    )
    .unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM turns WHERE session_id='overlong-cursor'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}

#[test]
fn unchanged_ingest_skips_but_same_size_change_is_read() {
    let env = setup("unchanged-skip");
    let tp = env.projects.join("-tmp-demo/unchanged-skip.jsonl");
    let first = user_rec("2026-06-12T01:00:00Z", "one", "one") + "\n";
    fs::write(&tp, &first).unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);

    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    db.execute(
        "UPDATE sessions SET scan_offset=0 WHERE session_id='unchanged-skip'",
        [],
    )
    .unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM turns WHERE session_id='unchanged-skip'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );

    let changed = user_rec("2026-06-12T01:00:00Z", "one", "two") + "\n";
    assert_eq!(changed.len(), first.len());
    fs::write(&tp, changed).unwrap();
    let (_, _, ok) = run_env::<&str>(
        &env,
        &["ingest", "--require-complete", tp.to_str().unwrap()],
        None,
        &[],
    );
    assert!(!ok, "strict mode accepted same-size changed content");
}

#[test]
fn strict_rejects_filtered_record_rewritten_as_real() {
    let env = setup("filtered-to-real");
    let tp = env.projects.join("-tmp-demo/filtered-to-real.jsonl");
    fs::write(
        &tp,
        user_rec("2026-06-12T01:00:00Z", "first", "first") + "\n" + &filtered_record() + "\n",
    )
    .unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    fs::write(
        &tp,
        user_rec("2026-06-12T01:00:00Z", "first", "first")
            + "\n"
            + &user_rec("2026-06-12T01:01:00Z", "second", "new real")
            + "\n",
    )
    .unwrap();
    let (_, _, ok) = run_env::<&str>(
        &env,
        &["ingest", "--require-complete", tp.to_str().unwrap()],
        None,
        &[],
    );
    assert!(!ok);
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM turns WHERE session_id='filtered-to-real'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn repair_resets_cursor_and_archives_fixed_line() {
    let env = setup("repair-cursor");
    let tp = env.projects.join("-tmp-demo/repair-cursor.jsonl");
    fs::write(&tp, "not json\n").unwrap();
    let (_, _, ok) = run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]);
    assert!(ok);
    fs::write(
        &tp,
        user_rec("2026-06-17T08:00:00Z", "repair-1", "repaired-line") + "\n",
    )
    .unwrap();
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    db.execute("UPDATE sessions SET skipped_lines=0, partial_tail=0, scan_offset=0, scan_seq=0 WHERE session_id='repair-cursor'", []).unwrap();
    let (_, _, ok) = run_env::<&str>(
        &env,
        &["ingest", "--require-complete", tp.to_str().unwrap()],
        None,
        &[],
    );
    assert!(ok);
    assert!(run(&env, &["session", "repair-cursor"], None)
        .0
        .contains("repaired-line"));
}

#[test]
fn legacy_upgrade_does_not_certify_unread_tail() {
    let env = setup("legacy-tail");
    let db = env.data.join("memory.db");
    let tp = env.projects.join("-tmp-demo/legacy-tail.jsonl");
    fs::write(
        &tp,
        format!(
            "{}\nnot json\n",
            user_rec("2026-06-17T08:00:00Z", "legacy-1", "legacy-line")
        ),
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE sessions (session_id TEXT PRIMARY KEY, file_path TEXT, project TEXT, cwd TEXT, first_ts TEXT, last_ts TEXT, num_turns INTEGER DEFAULT 0, last_seq INTEGER DEFAULT -1, file_size INTEGER DEFAULT 0, checkpointed_seq INTEGER DEFAULT -1, scan_offset INTEGER NOT NULL DEFAULT 0, scan_seq INTEGER NOT NULL DEFAULT 0, ingested_at TEXT); CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, seq INTEGER NOT NULL, uuid TEXT, ts TEXT, role TEXT, text TEXT, is_meta INTEGER DEFAULT 0, is_sidechain INTEGER DEFAULT 0, project TEXT, cwd TEXT, UNIQUE(session_id, seq)); CREATE VIRTUAL TABLE turns_fts USING fts5(text, content='turns', content_rowid='id'); CREATE TABLE facts (id INTEGER PRIMARY KEY AUTOINCREMENT, project TEXT NOT NULL, name TEXT, type TEXT, title TEXT, hook TEXT, leaf_path TEXT, description TEXT, tags TEXT, index_seq INTEGER, pinned INTEGER DEFAULT 0, status TEXT DEFAULT 'active', hits INTEGER DEFAULT 0, created_at TEXT, updated_at TEXT, last_used_at TEXT, superseded_at TEXT, superseded_by INTEGER, origin_session TEXT); CREATE VIRTUAL TABLE facts_fts USING fts5(title, hook, description, content='facts', content_rowid='id'); CREATE TABLE session_tags (session_id TEXT NOT NULL, ns TEXT NOT NULL, tag TEXT NOT NULL, rank INTEGER NOT NULL, PRIMARY KEY(session_id, tag)); PRAGMA user_version=4;").unwrap();
    drop(conn);
    let (_, _, ok) = run_env::<&str>(
        &env,
        &["ingest", "--require-complete", tp.to_str().unwrap()],
        None,
        &[],
    );
    assert!(!ok);
}

#[test]
fn upgraded_database_allows_two_normal_appends() {
    let env = setup("null-head-append");
    let tp = env.projects.join("-tmp-demo/null-head-append.jsonl");
    fs::write(&tp, user_rec("2026-06-17T08:00:00Z", "n1", "first") + "\n").unwrap();
    run(&env, &["ingest", tp.to_str().unwrap()], None);
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    fs::write(
        &tp,
        format!(
            "{}\n{}\n",
            user_rec("2026-06-17T08:00:00Z", "n1", "first"),
            user_rec("2026-06-17T08:01:00Z", "n2", "second")
        ),
    )
    .unwrap();
    let (_, err, ok) = run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]);
    assert!(ok, "first append failed: {err}");
    fs::write(
        &tp,
        format!(
            "{}\n{}\n{}\n",
            user_rec("2026-06-17T08:00:00Z", "n1", "first"),
            user_rec("2026-06-17T08:01:00Z", "n2", "second"),
            user_rec("2026-06-17T08:02:00Z", "n3", "third")
        ),
    )
    .unwrap();
    let (_, err, ok) = run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]);
    assert!(ok, "second append failed: {err}");
    let rows: i64 = db
        .query_row(
            "SELECT count(*) FROM turns WHERE session_id='null-head-append'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 3);
}

#[test]
fn upgraded_archive_keeps_existing_rows_on_normal_ingest() {
    let env = setup("upgrade-keeps-rows");
    let db = env.data.join("memory.db");
    let tp = env.projects.join("-tmp-demo/upgrade-keeps-rows.jsonl");
    let first = user_rec("2026-06-17T08:00:00Z", "u1", "old first");
    let second = user_rec("2026-06-17T08:01:00Z", "u2", "old second");
    let third = user_rec("2026-06-17T08:02:00Z", "u3", "new third");
    let original = format!("{first}\n{second}\n");
    fs::write(&tp, &original).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE sessions (session_id TEXT PRIMARY KEY, file_path TEXT, project TEXT, cwd TEXT, first_ts TEXT, last_ts TEXT, num_turns INTEGER DEFAULT 0, last_seq INTEGER DEFAULT -1, file_size INTEGER DEFAULT 0, checkpointed_seq INTEGER DEFAULT -1, scan_offset INTEGER NOT NULL DEFAULT 0, scan_seq INTEGER NOT NULL DEFAULT 0, ingested_at TEXT); CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, seq INTEGER NOT NULL, uuid TEXT, ts TEXT, role TEXT, text TEXT, is_meta INTEGER DEFAULT 0, is_sidechain INTEGER DEFAULT 0, project TEXT, cwd TEXT, UNIQUE(session_id, seq)); CREATE VIRTUAL TABLE turns_fts USING fts5(text, content='turns', content_rowid='id'); CREATE TABLE facts (id INTEGER PRIMARY KEY AUTOINCREMENT, project TEXT NOT NULL, name TEXT, type TEXT, title TEXT, hook TEXT, leaf_path TEXT, description TEXT, tags TEXT, index_seq INTEGER, pinned INTEGER DEFAULT 0, status TEXT DEFAULT 'active', hits INTEGER DEFAULT 0, created_at TEXT, updated_at TEXT, last_used_at TEXT, superseded_at TEXT, superseded_by INTEGER, origin_session TEXT); CREATE VIRTUAL TABLE facts_fts USING fts5(title, hook, description, content='facts', content_rowid='id'); CREATE TABLE session_tags (session_id TEXT NOT NULL, ns TEXT NOT NULL, tag TEXT NOT NULL, rank INTEGER NOT NULL, PRIMARY KEY(session_id, tag)); PRAGMA user_version=4;").unwrap();
    conn.execute("INSERT INTO sessions(session_id,file_path,num_turns,last_seq,file_size,scan_offset,scan_seq) VALUES ('upgrade-keeps-rows', ?, 2, 1, ?, ?, 2)", rusqlite::params![tp.to_string_lossy(), original.len() as i64, original.len() as i64]).unwrap();
    conn.execute("INSERT INTO turns(session_id,seq,uuid,role,text) VALUES ('upgrade-keeps-rows',0,'u1','user','original archived first'),('upgrade-keeps-rows',1,'u2','user','original archived second')", []).unwrap();
    drop(conn);
    fs::write(&tp, format!("{original}{third}\n")).unwrap();
    let (_, err, ok) = run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]);
    assert!(ok, "upgrade ingest failed: {err}");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let rows: Vec<String> = conn
        .prepare("SELECT text FROM turns WHERE session_id='upgrade-keeps-rows' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            "original archived first",
            "original archived second",
            "new third"
        ]
    );
}

#[test]
fn strict_repair_clears_state_and_re_reads_content() {
    let env = setup("strict-repair");
    let tp = env.projects.join("-tmp-demo/strict-repair.jsonl");
    fs::write(
        &tp,
        user_rec("2026-06-17T08:00:00Z", "r1", "original") + "\n",
    )
    .unwrap();
    run(&env, &["ingest", tp.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "strict-repair"], None);
    fs::write(
        &tp,
        user_rec("2026-06-17T08:00:00Z", "r1", "repaired") + "\n",
    )
    .unwrap();
    let (_, _, ok) = run_env::<&str>(
        &env,
        &["ingest", "--require-complete", tp.to_str().unwrap()],
        None,
        &[],
    );
    assert!(!ok);
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    db.execute("DELETE FROM turns WHERE session_id='strict-repair'", [])
        .unwrap();
    db.execute("UPDATE sessions SET num_turns=0, last_seq=-1, file_size=0, file_mtime=0, skipped_lines=0, partial_tail=0, scan_offset=0, scan_seq=0 WHERE session_id='strict-repair'", []).unwrap();
    let (_, err, ok) = run_env::<&str>(
        &env,
        &["ingest", "--require-complete", tp.to_str().unwrap()],
        None,
        &[],
    );
    assert!(ok, "repair ingest failed: {err}");
    let text: String = db
        .query_row(
            "SELECT text FROM turns WHERE session_id='strict-repair' AND seq=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(text.contains("repaired"));
    let (_, output, ok) = run_env::<&str>(&env, &["search", "repaired"], None, &[]);
    assert!(ok, "repaired text is not searchable: {output}");
}

#[test]
fn strict_rejects_archived_record_replaced_by_whitespace() {
    let env = setup("whitespace-rewrite");
    let tp = env.projects.join("-tmp-demo/whitespace-rewrite.jsonl");
    fs::write(&tp, user_rec("2026-06-17T08:00:00Z", "w1", "held") + "\n").unwrap();
    assert!(run_env::<&str>(&env, &["ingest", tp.to_str().unwrap()], None, &[]).2);
    let size = fs::metadata(&tp).unwrap().len();
    fs::write(&tp, " ".repeat(size as usize - 1) + "\n").unwrap();
    let (_, _, ok) = run_env::<&str>(
        &env,
        &["ingest", "--require-complete", tp.to_str().unwrap()],
        None,
        &[],
    );
    assert!(!ok);
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT text FROM turns WHERE session_id='whitespace-rewrite'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "held"
    );
}

#[test]
fn sweep_fails_when_projects_root_is_not_a_directory() {
    let env = setup("bad-project-root");
    let root_file = env.data.join("projects-file");
    fs::write(&root_file, "x").unwrap();
    let (_, err, ok) = run_env(
        &env,
        &["ingest", "--sweep"],
        None,
        &[("SUBROSA_PROJECTS_DIR", root_file.as_os_str())],
    );
    assert!(!ok, "bad projects root succeeded: {err}");
}

#[test]
fn checkpoint_clear_requires_confirmation_and_preserves_queue() {
    let env = setup("checkpoint-clear");
    let transcript = env.projects.join("-tmp-demo/aaaa-1111.jsonl");
    fs::write(
        &transcript,
        [
            user_rec("2026-06-12T01:00:00Z", "u1", "queue me"),
            user_rec("2026-06-12T01:01:00Z", "u2", "queue me too"),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let (ingest_out, ingest_err, ingest_ok) =
        run_env::<&str>(&env, &["ingest", transcript.to_str().unwrap()], None, &[]);
    assert!(ingest_ok, "ingest failed: {ingest_out} {ingest_err}");
    let (_, ingest_err, ingest_ok) =
        run_env::<&str>(&env, &["ingest", transcript.to_str().unwrap()], None, &[]);
    assert!(ingest_ok, "ingest failed: {ingest_err}");
    run(&env, &["checkpoint-enqueue", "aaaa-1111"], None);
    let (_, err, ok) = run_env::<&str>(&env, &["checkpoint-clear"], None, &[]);
    assert!(!ok);
    assert!(err.contains("1 queued session(s)"));
    assert!(err.contains("checkpoint-drop <id>"));
    assert!(run(&env, &["pending"], None).0.contains("aaaa-1111"));
}

#[test]
fn checkpoint_clear_confirm_refuses_incomplete_queue() {
    let env = setup("checkpoint-clear-confirm");
    let transcript = env.projects.join("-tmp-demo/clear-confirm.jsonl");
    fs::write(
        &transcript,
        [
            user_rec("2026-06-12T01:00:00Z", "u1", "queue me"),
            user_rec("2026-06-12T01:01:00Z", "u2", "queue me too"),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let (ingest_out, ingest_err, ingest_ok) =
        run_env::<&str>(&env, &["ingest", transcript.to_str().unwrap()], None, &[]);
    assert!(ingest_ok, "ingest failed: {ingest_out} {ingest_err}");
    let (enqueue_out, enqueue_err, enqueue_ok) =
        run_env::<&str>(&env, &["checkpoint-enqueue", "clear-confirm"], None, &[]);
    assert!(enqueue_ok, "enqueue failed: {enqueue_out} {enqueue_err}");
    let (_, err, ok) = run_env::<&str>(&env, &["checkpoint-clear", "--confirm"], None, &[]);
    assert!(!ok);
    assert!(err.contains("still need per-session verification"));
    assert!(run(&env, &["pending"], None).0.contains("clear-confirm"));
}

#[test]
fn queue_read_paths_use_the_database_queue() {
    let env = setup("queue-upgrade");
    let db = env.data.join("memory.db");
    let transcript = env.projects.join("-tmp-demo/old-session.jsonl");
    fs::write(
        &transcript,
        [
            user_rec("2026-06-12T01:00:00Z", "u1", "one"),
            user_rec("2026-06-12T01:01:00Z", "u2", "two"),
        ]
        .join("\n"),
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "old-session"], None);
    let (prompt, err, ok) = run_env::<&str>(
        &env,
        &["hook", "user-prompt-submit"],
        Some(r#"{"prompt":"hello","session_id":"live"}"#),
        &[],
    );
    assert!(ok, "prompt hook failed: {err}");
    assert!(prompt.contains("1 session(s) queued"), "got: {prompt}");
    assert_eq!(
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row("SELECT count(*) FROM checkpoint_queue", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn distill_session_start_is_quiet() {
    let env = setup("distill-session-start");
    fs::write(env.data.join("config"), "distill=/absolute/claude\n").unwrap();
    let transcript = env.projects.join("-tmp-demo/queued.jsonl");
    fs::write(
        &transcript,
        user_rec("2026-06-12T01:00:00Z", "u1", "queued") + "\n",
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "queued"], None);
    let (out, err, ok) = run_env(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live"}"#),
        &[("SUBROSA_DISTILL", "/absolute/claude")],
    );
    assert!(ok, "session-start failed: {err}");
    assert_eq!(out, "[subrosa] Archive is updated.\n");
}

#[test]
fn distill_session_start_suppresses_other_notices() {
    let env = setup("distill-session-start-notices");
    fs::write(env.data.join("config"), "distill=/absolute/claude\n").unwrap();
    let memdir = env.projects.join("-tmp-demo").join("memory");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(memdir.join("MEMORY.md"), "x".repeat(24_001)).unwrap();
    let transcript = env.projects.join("-tmp-demo/queued.jsonl");
    fs::write(
        &transcript,
        user_rec("2026-06-12T01:00:00Z", "u1", "queued") + "\n",
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "queued"], None);
    let (disabled, err, ok) = run_env(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live-disabled"}"#),
        &[("SUBROSA_DISTILL", "off"), ("SUBROSA_SEMANTIC", "off")],
    );
    assert!(ok, "disabled session-start failed: {err}");
    assert!(
        disabled.contains("MEMORY.md is"),
        "notice missing: {disabled}"
    );
    let (out, err, ok) = run_env(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live"}"#),
        &[("SUBROSA_DISTILL", "/absolute/claude")],
    );
    assert!(ok, "session-start failed: {err}");
    assert_eq!(out, "[subrosa] Archive is updated.\n");
}

#[test]
fn distill_user_prompt_has_no_backlog_line() {
    let env = setup("distill-user-prompt");
    fs::write(env.data.join("config"), "distill=/absolute/claude\n").unwrap();
    let transcript = env.projects.join("-tmp-demo/queued.jsonl");
    fs::write(
        &transcript,
        user_rec("2026-06-12T01:00:00Z", "u1", "queued") + "\n",
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "queued"], None);
    let (out, err, ok) = run_env(
        &env,
        &["hook", "user-prompt-submit"],
        Some(r#"{"prompt":"no recall","cwd":"/tmp/demo","session_id":"live"}"#),
        &[("SUBROSA_DISTILL", "/absolute/claude")],
    );
    assert!(ok, "user-prompt-submit failed: {err}");
    assert_eq!(out, "");
}

#[test]
fn legacy_queue_import_runs_on_real_v4_upgrade_once() {
    let env = setup("legacy-upgrade");
    let db = env.data.join("memory.db");
    let legacy = env.data.join("pending-checkpoint.log");
    fs::write(
        &legacy,
        "2026-06-12T02:00:00+00:00\tlater-session\n2026-06-12T01:00:00+00:00\tlegacy-session\n",
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (session_id TEXT PRIMARY KEY, file_path TEXT, project TEXT, cwd TEXT, first_ts TEXT, last_ts TEXT, num_turns INTEGER DEFAULT 0, last_seq INTEGER DEFAULT -1, file_size INTEGER DEFAULT 0, checkpointed_seq INTEGER DEFAULT -1, scan_offset INTEGER NOT NULL DEFAULT 0, scan_seq INTEGER NOT NULL DEFAULT 0, ingested_at TEXT); CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, seq INTEGER NOT NULL, uuid TEXT, ts TEXT, role TEXT, text TEXT, is_meta INTEGER DEFAULT 0, is_sidechain INTEGER DEFAULT 0, project TEXT, cwd TEXT, UNIQUE(session_id, seq)); CREATE VIRTUAL TABLE turns_fts USING fts5(text, content='turns', content_rowid='id', tokenize='porter unicode61'); CREATE TABLE facts (id INTEGER PRIMARY KEY AUTOINCREMENT, project TEXT NOT NULL, name TEXT, type TEXT, title TEXT, hook TEXT, leaf_path TEXT, description TEXT, tags TEXT, index_seq INTEGER, pinned INTEGER DEFAULT 0, status TEXT DEFAULT 'active', hits INTEGER DEFAULT 0, created_at TEXT, updated_at TEXT, last_used_at TEXT, superseded_at TEXT, superseded_by INTEGER, origin_session TEXT); CREATE VIRTUAL TABLE facts_fts USING fts5(title, hook, description, content='facts', content_rowid='id', tokenize='porter unicode61'); CREATE TABLE session_tags (session_id TEXT NOT NULL, ns TEXT NOT NULL, tag TEXT NOT NULL, rank INTEGER NOT NULL, PRIMARY KEY(session_id, tag)); PRAGMA user_version=4;",
    ).unwrap();
    drop(conn);
    run(&env, &["init"], None);
    let pending = run(&env, &["pending"], None).0;
    assert!(pending.find("later-session").unwrap() < pending.find("legacy-session").unwrap());
    fs::write(&legacy, "2026-06-12T03:00:00+00:00\tnew-session\n").unwrap();
    run(&env, &["init"], None);
    let pending = run(&env, &["pending"], None).0;
    assert!(pending.contains("legacy-session"));
    assert!(!pending.contains("new-session"));
    assert!(legacy.exists());
}

#[test]
fn legacy_queue_import_skips_completed_and_avoids_sequence_collision() {
    let env = setup("legacy-completed");
    let db = env.data.join("memory.db");
    let legacy = env.data.join("pending-checkpoint.log");
    fs::write(
        &legacy,
        "2026-06-12T01:00:00+00:00\tcompleted\n2026-06-12T02:00:00+00:00\tlive\n",
    )
    .unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (session_id TEXT PRIMARY KEY, file_path TEXT, project TEXT, cwd TEXT, first_ts TEXT, last_ts TEXT, num_turns INTEGER DEFAULT 0, last_seq INTEGER DEFAULT -1, file_size INTEGER DEFAULT 0, checkpointed_seq INTEGER DEFAULT -1, scan_offset INTEGER NOT NULL DEFAULT 0, scan_seq INTEGER NOT NULL DEFAULT 0, ingested_at TEXT); CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, seq INTEGER NOT NULL, uuid TEXT, ts TEXT, role TEXT, text TEXT, is_meta INTEGER DEFAULT 0, is_sidechain INTEGER DEFAULT 0, project TEXT, cwd TEXT, UNIQUE(session_id, seq)); CREATE VIRTUAL TABLE turns_fts USING fts5(text, content='turns', content_rowid='id', tokenize='porter unicode61'); CREATE VIRTUAL TABLE facts_fts USING fts5(title, hook, description, content='facts', content_rowid='id', tokenize='porter unicode61'); CREATE TABLE facts (id INTEGER PRIMARY KEY AUTOINCREMENT, project TEXT NOT NULL, name TEXT, type TEXT, title TEXT, hook TEXT, leaf_path TEXT, description TEXT, tags TEXT, index_seq INTEGER, pinned INTEGER DEFAULT 0, status TEXT DEFAULT 'active', hits INTEGER DEFAULT 0, created_at TEXT, updated_at TEXT, last_used_at TEXT, superseded_at TEXT, superseded_by INTEGER, origin_session TEXT); CREATE TABLE session_tags (session_id TEXT NOT NULL, ns TEXT NOT NULL, tag TEXT NOT NULL, rank INTEGER NOT NULL, PRIMARY KEY(session_id, tag)); CREATE TABLE checkpoint_queue (session_id TEXT PRIMARY KEY, enqueued_at TEXT NOT NULL, enqueue_seq INTEGER NOT NULL DEFAULT 0); INSERT INTO sessions(session_id, checkpointed_seq, last_seq) VALUES ('completed', 0, 0), ('live', -1, 0); INSERT INTO turns(session_id, seq) VALUES ('completed', 0), ('live', 0); INSERT INTO checkpoint_queue(session_id, enqueued_at, enqueue_seq) VALUES ('existing', '2026-06-12T00:00:00Z', 7); PRAGMA user_version=4;",
    ).unwrap();
    drop(conn);
    run(&env, &["init"], None);
    let conn = rusqlite::Connection::open(db).unwrap();
    let rows: Vec<(String, i64)> = conn
        .prepare("SELECT session_id, enqueue_seq FROM checkpoint_queue ORDER BY session_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, vec![("existing".into(), 7), ("live".into(), 8)]);
}

#[test]
fn checkpoint_mark_empty_queue_is_safe() {
    let env = setup("checkpoint-empty");
    let (_, err, ok) = run_env::<&str>(&env, &["checkpoint-mark", "missing-session"], None, &[]);
    assert!(!ok);
    assert!(err.contains("no archived turns"));
    assert!(!run(&env, &["pending"], None).0.contains("missing-session"));
}

#[test]
fn checkpoint_drop_keeps_uncheckpointed_turns_queued() {
    let env = setup("checkpoint-boundary");
    let transcript = env.projects.join("-tmp-demo/boundary.jsonl");
    let records = [
        user_rec("2026-06-12T01:00:00Z", "u1", "first"),
        user_rec("2026-06-12T01:01:00Z", "u2", "second"),
        user_rec("2026-06-12T01:02:00Z", "u3", "third"),
    ];
    fs::write(&transcript, records.join("\n") + "\n").unwrap();
    let (_, err, ok) = run_env::<&str>(&env, &["ingest", transcript.to_str().unwrap()], None, &[]);
    assert!(ok, "ingest failed: {err}");
    run(&env, &["checkpoint-enqueue", "boundary"], None);
    let (_, err, ok) = run_env::<&str>(
        &env,
        &["checkpoint-drop", "boundary", "--max-seq", "1"],
        None,
        &[],
    );
    assert!(ok, "bounded drop failed: {err}");
    assert!(run(&env, &["pending"], None).0.contains("boundary"));
    let (_, err, ok) = run_env::<&str>(
        &env,
        &["checkpoint-drop", "boundary", "--max-seq", "2"],
        None,
        &[],
    );
    assert!(ok, "final drop failed: {err}");
    assert!(!run(&env, &["pending"], None).0.contains("boundary"));
}

#[test]
fn checkpoint_drop_abandon_removes_queue_without_advancing_watermark() {
    let env = setup("checkpoint-abandon");
    let transcript = env.projects.join("-tmp-demo/abandon.jsonl");
    fs::write(
        &transcript,
        user_rec("2026-06-12T01:00:00Z", "u1", "abandon me"),
    )
    .unwrap();
    let (_, err, ok) = run_env::<&str>(&env, &["ingest", transcript.to_str().unwrap()], None, &[]);
    assert!(ok, "ingest failed: {err}");
    run(&env, &["checkpoint-enqueue", "abandon"], None);
    let db = rusqlite::Connection::open(env.data.join("memory.db")).unwrap();
    db.execute(
        "UPDATE sessions SET checkpointed_seq=0 WHERE session_id='abandon'",
        [],
    )
    .unwrap();
    let (_, err, ok) = run_env::<&str>(
        &env,
        &["checkpoint-drop", "abandon", "--abandon"],
        None,
        &[],
    );
    assert!(ok, "abandon failed: {err}");
    assert!(!run(&env, &["pending"], None).0.contains("abandon"));
    assert_eq!(
        db.query_row(
            "SELECT checkpointed_seq FROM sessions WHERE session_id='abandon'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
}

#[test]
fn checkpoint_drop_abandon_rejects_max_seq() {
    let env = setup("checkpoint-abandon-max");
    let transcript = env.projects.join("-tmp-demo/abandon-max.jsonl");
    fs::write(
        &transcript,
        [
            user_rec("2026-06-12T01:00:00Z", "u1", "keep me"),
            user_rec("2026-06-12T01:01:00Z", "u2", "keep me too"),
        ]
        .join("\n"),
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "abandon-max"], None);
    let (_, err, ok) = run_env::<&str>(
        &env,
        &["checkpoint-drop", "abandon-max", "--abandon", "--max-seq=5"],
        None,
        &[],
    );
    assert!(!ok);
    assert!(err.contains("cannot be used with --max-seq"));
    assert!(run(&env, &["pending"], None).0.contains("abandon-max"));
}

#[test]
fn recall_needs_an_anchor_term() {
    let env = setup("anchor");
    ingest(
        &env,
        "aaaa-1111",
        &[user_rec(
            "2026-06-12T01:00:00Z",
            "u1",
            "we always build and test the demo service",
        )],
    );
    // Two short generic words match the stored turn but carry no anchor.
    let payload = r#"{"prompt":"can we build and test","cwd":"/tmp/demo","session_id":"live-1"}"#;
    let (out, _) = run(&env, &["hook", "user-prompt-submit"], Some(payload));
    assert_eq!(out, "", "generic terms must stay silent, got:\n{out}");
    // The same archive fires once an anchor-grade term is in the match.
    let payload = r#"{"prompt":"how do we build and test the demo service","cwd":"/tmp/demo","session_id":"live-1"}"#;
    let (out, _) = run(&env, &["hook", "user-prompt-submit"], Some(payload));
    assert!(
        out.starts_with("[subrosa recall]"),
        "anchored prompt should inject, got:\n{out}"
    );
}

#[test]
fn recall_injects_each_source_session_once() {
    let env = setup("dedup");
    ingest(
        &env,
        "cccc-2222",
        &[user_rec(
            "2026-06-12T01:00:00Z",
            "u1",
            "TICKET-456 cache-prod rollout finished cleanly",
        )],
    );
    let payload = |sid: &str| {
        format!(
            r#"{{"prompt":"status of the cache-prod TICKET-456 rollout","cwd":"/tmp/demo","session_id":"{sid}"}}"#
        )
    };
    let (first, _) = run(
        &env,
        &["hook", "user-prompt-submit"],
        Some(&payload("live-1")),
    );
    assert!(
        first.starts_with("[subrosa recall]"),
        "first prompt should inject, got:\n{first}"
    );
    let (second, _) = run(
        &env,
        &["hook", "user-prompt-submit"],
        Some(&payload("live-1")),
    );
    assert_eq!(
        second, "",
        "same live session must not re-inject the same source"
    );
    let (other, _) = run(
        &env,
        &["hook", "user-prompt-submit"],
        Some(&payload("live-2")),
    );
    assert!(
        other.starts_with("[subrosa recall]"),
        "a new live session starts fresh, got:\n{other}"
    );
}

#[test]
fn nudge_text_never_archived() {
    let env = setup("nudge");
    ingest(
        &env,
        "eeee-3333",
        &[
            user_rec(
                "2026-06-12T01:00:00Z",
                "u1",
                "[subrosa] 2 session(s) queued for checkpoint — run /subrosa:checkpoint-backlog to save them to memory now.",
            ),
            user_rec(
                "2026-06-12T01:01:00Z",
                "u2",
                "TICKET-789 demo turn for the archive",
            ),
        ],
    );
    let (dump, _) = run(&env, &["session", "eeee-3333"], None);
    assert!(
        !dump.contains("queued for checkpoint"),
        "nudge re-entered the archive:\n{dump}"
    );
    assert!(dump.contains("TICKET-789"), "real turn missing:\n{dump}");
}

#[test]
fn loud_nudge_is_imperative_and_fully_prefixed() {
    let env = setup("loudnudge");
    for (sid, text) in [("zzzzzzzz-1111", "one"), ("aaaaaaaa-2222", "two")] {
        let transcript = env.projects.join(format!("-tmp-demo/{sid}.jsonl"));
        fs::write(
            &transcript,
            [
                user_rec("2026-06-12T01:00:00Z", "u1", text),
                user_rec("2026-06-12T01:01:00Z", "u2", "more"),
            ]
            .join("\n"),
        )
        .unwrap();
        run(&env, &["ingest", transcript.to_str().unwrap()], None);
        run(&env, &["checkpoint-enqueue", sid], None);
    }
    let (out, _) = run(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live-loud"}"#),
    );
    assert!(
        out.contains("ACTION REQUIRED"),
        "loud nudge missing:\n{out}"
    );
    assert!(
        out.contains("/subrosa:checkpoint-backlog"),
        "skill pointer missing:\n{out}"
    );
    // The up-to-5 ids are listed newest (later-enqueued) first.
    let ids_line = out
        .lines()
        .find(|l| l.contains("Queued, newest first"))
        .expect("ids line missing");
    assert!(
        ids_line.find("aaaaaaaa-2222").unwrap() < ids_line.find("zzzzzzzz-1111").unwrap(),
        "ids not newest-first: {ids_line}"
    );
    // Invariant: every emitted line starts with [subrosa], so the whole block is
    // filtered from the archive (NOISE_PREFIXES) however the injected context is
    // later chunked.
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.starts_with("[subrosa]"),
            "unprefixed nudge line would re-enter the archive: {line}"
        );
    }
    // Prove it: archive every nudge line plus a real turn — only the real one survives.
    let mut recs: Vec<String> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| user_rec("2026-06-12T03:00:00Z", &format!("n{i}"), l))
        .collect();
    recs.push(user_rec(
        "2026-06-12T03:01:00Z",
        "real",
        "TICKET-321 real archived turn",
    ));
    ingest(&env, "gggg-7777", &recs);
    let (dump, _) = run(&env, &["session", "gggg-7777"], None);
    assert!(
        !dump.contains("ACTION REQUIRED") && !dump.contains("queued for checkpoint"),
        "loud nudge re-entered the archive:\n{dump}"
    );
    assert!(dump.contains("TICKET-321"), "real turn missing:\n{dump}");
}

#[test]
fn quiet_nudge_mode_is_the_calm_one_liner() {
    let env = setup("quietnudge");
    fs::write(env.data.join("config"), "checkpoint_nudge=quiet\n").unwrap();
    let transcript = env.projects.join("-tmp-demo/aaaaaaa-1111.jsonl");
    fs::write(
        &transcript,
        [
            user_rec("2026-06-12T01:00:00Z", "u1", "one"),
            user_rec("2026-06-12T01:01:00Z", "u2", "two"),
        ]
        .join("\n"),
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "aaaaaaa-1111"], None);
    let (out, _) = run(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live-q"}"#),
    );
    assert!(
        out.contains("1 session(s) queued for checkpoint"),
        "quiet one-liner missing:\n{out}"
    );
    assert!(
        !out.contains("ACTION REQUIRED"),
        "quiet mode must not emit the loud block:\n{out}"
    );
}

#[test]
fn off_nudge_mode_is_silent() {
    let env = setup("offnudge");
    fs::write(env.data.join("config"), "checkpoint_nudge=off\n").unwrap();
    let transcript = env.projects.join("-tmp-demo/aaaaaaa-1111.jsonl");
    fs::write(
        &transcript,
        [
            user_rec("2026-06-12T01:00:00Z", "u1", "one"),
            user_rec("2026-06-12T01:01:00Z", "u2", "two"),
        ]
        .join("\n"),
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "aaaaaaa-1111"], None);
    let (out, _) = run(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live-off"}"#),
    );
    assert!(
        !out.contains("queued for checkpoint") && !out.contains("ACTION REQUIRED"),
        "off mode must emit no checkpoint nudge:\n{out}"
    );
}

#[test]
fn empty_env_nudge_mode_falls_back_to_config() {
    // An exported-but-empty SUBROSA_CHECKPOINT_NUDGE must not shadow the config
    // value (env::var returns Ok("") for an empty var) — config "quiet" wins.
    let env = setup("envnudge");
    fs::write(env.data.join("config"), "checkpoint_nudge=quiet\n").unwrap();
    let transcript = env.projects.join("-tmp-demo/aaaaaaa-1111.jsonl");
    fs::write(
        &transcript,
        [
            user_rec("2026-06-12T01:00:00Z", "u1", "one"),
            user_rec("2026-06-12T01:01:00Z", "u2", "two"),
        ]
        .join("\n"),
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "aaaaaaa-1111"], None);
    let (stdout, _, _) = run_env(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live-env"}"#),
        &[("SUBROSA_CHECKPOINT_NUDGE", "")],
    );
    assert!(
        stdout.contains("1 session(s) queued for checkpoint")
            && !stdout.contains("ACTION REQUIRED"),
        "empty env must fall back to config (quiet), got:\n{stdout}"
    );
}

#[test]
fn backlog_directive_rides_each_user_prompt() {
    // The one-shot SessionStart nudge gets scrolled past once the first prompt
    // lands, so the directive must also ride UserPromptSubmit while sessions are
    // queued. Empty archive here, so stdout is purely the directive (no recall).
    let env = setup("backlogride");
    for (sid, text) in [("aaaaaaaa-1111", "one"), ("bbbbbbbb-2222", "two")] {
        let transcript = env.projects.join(format!("-tmp-demo/{sid}.jsonl"));
        fs::write(
            &transcript,
            [
                user_rec("2026-06-12T01:00:00Z", "u1", text),
                user_rec("2026-06-12T01:01:00Z", "u2", "more"),
            ]
            .join("\n"),
        )
        .unwrap();
        run(&env, &["ingest", transcript.to_str().unwrap()], None);
        run(&env, &["checkpoint-enqueue", sid], None);
    }
    let payload = r#"{"prompt":"a normal question with no archive match","cwd":"/tmp/demo","session_id":"live-bd"}"#;
    let (out, _) = run(&env, &["hook", "user-prompt-submit"], Some(payload));
    assert!(
        out.contains("ACTION REQUIRED")
            && out.contains("/subrosa:checkpoint-backlog")
            && out.contains("2 session(s)")
            && out.contains("background"),
        "directive should ride the prompt and point at a background run, got:\n{out}"
    );
    // Stays [subrosa]-prefixed so NOISE_PREFIXES keeps it out of the archive.
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.starts_with("[subrosa]"),
            "unprefixed directive line would re-enter the archive: {line}"
        );
    }
}

#[test]
fn backlog_directive_silent_when_queue_empty() {
    let env = setup("backlogempty");
    let payload = r#"{"prompt":"anything at all here","cwd":"/tmp/demo","session_id":"live-be"}"#;
    let (out, _) = run(&env, &["hook", "user-prompt-submit"], Some(payload));
    assert_eq!(
        out, "",
        "empty queue and no recall must stay silent, got:\n{out}"
    );
}

#[test]
fn backlog_directive_respects_off_mode() {
    let env = setup("backlogoff");
    fs::write(env.data.join("config"), "checkpoint_nudge=off\n").unwrap();
    let transcript = env.projects.join("-tmp-demo/aaaaaaa-1111.jsonl");
    fs::write(
        &transcript,
        [
            user_rec("2026-06-12T01:00:00Z", "u1", "one"),
            user_rec("2026-06-12T01:01:00Z", "u2", "two"),
        ]
        .join("\n"),
    )
    .unwrap();
    run(&env, &["ingest", transcript.to_str().unwrap()], None);
    run(&env, &["checkpoint-enqueue", "aaaaaaa-1111"], None);
    let payload = r#"{"prompt":"anything","cwd":"/tmp/demo","session_id":"live-bo"}"#;
    let (out, _) = run(&env, &["hook", "user-prompt-submit"], Some(payload));
    assert!(
        !out.contains("ACTION REQUIRED") && !out.contains("queued for checkpoint"),
        "off mode must silence the per-prompt directive, got:\n{out}"
    );
}

#[test]
fn long_hooks_capped_in_index() {
    let env = setup("hookcap");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(
        memdir.join("reference_long.md"),
        "---\nname: long-fact\ndescription: short\n---\nbody\n",
    )
    .unwrap();
    let md = memdir.to_str().unwrap();
    let long_hook = "x".repeat(400);
    let (_, err) = run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "reference_long.md",
            "--memdir",
            md,
            "--hook",
            &long_hook,
        ],
        None,
    );
    assert!(
        err.contains("hook truncated to 240 chars"),
        "missing truncation notice, stderr:\n{err}"
    );
    let (out, _) = run(&env, &["generate", "--memdir", md, "--dry-run"], None);
    let line = out
        .lines()
        .find(|l| l.contains("reference_long.md"))
        .expect("index line for the fact");
    assert!(line.ends_with('…'), "capped hook should end with …: {line}");
    assert!(
        line.chars().count() < 300,
        "index line not capped: {} chars",
        line.chars().count()
    );
}

#[test]
fn per_project_budget_file() {
    let env = setup("budgetfile");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(
        memdir.join("reference_budget.md"),
        "---\nname: budget-fact\ndescription: a fact that has to fit\n---\nbody\n",
    )
    .unwrap();
    let md = memdir.to_str().unwrap();
    run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "reference_budget.md",
            "--memdir",
            md,
        ],
        None,
    );

    // Too small for even one line: the fact drops and stderr says how to raise it.
    fs::write(memdir.join(".budget"), "60\n").unwrap();
    let (out, err) = run(&env, &["generate", "--memdir", md, "--dry-run"], None);
    assert!(err.contains("budget=60"), ".budget not picked up:\n{err}");
    assert!(
        err.contains("echo <n> >") && err.contains(".budget"),
        "raise-the-budget hint missing:\n{err}"
    );
    assert!(
        !out.contains("reference_budget.md"),
        "fact should have been dropped:\n{out}"
    );

    // An explicit --budget beats the file.
    let (out, err) = run(
        &env,
        &["generate", "--memdir", md, "--dry-run", "--budget", "23000"],
        None,
    );
    assert!(err.contains("budget=23000"), "--budget ignored:\n{err}");
    assert!(out.contains("reference_budget.md"), "fact missing:\n{out}");

    // An unparsable file warns once and falls back to the default.
    fs::write(memdir.join(".budget"), "junk\n").unwrap();
    let (_, err) = run(&env, &["generate", "--memdir", md, "--dry-run"], None);
    assert!(
        err.contains("ignoring") && err.contains(".budget") && err.contains("budget=23000"),
        "bad .budget should warn and fall back:\n{err}"
    );
}

/// A .budget above the load cap must not silence the size nudge — that would
/// go quiet exactly when the index stops fitting in context.
#[test]
fn over_cap_budget_still_nudges() {
    let env = setup("budgetovercap");
    let memdir = env.projects.join("-tmp-demo/memory");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(memdir.join("MEMORY.md"), "x".repeat(26_000)).unwrap();
    fs::write(memdir.join(".budget"), "40000\n").unwrap();

    let (out, _) = run(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live-cap"}"#),
    );
    assert!(
        out.contains("near the always-loaded cap"),
        "a 26KB index past the 25KB load cap should still nudge, got:\n{out}"
    );
}

/// A config write that fails must not print the line that says it worked.
/// (The interactive passphrase branch needs a terminal, so `--no-mirror` is
/// the reachable half of the same decision.)
#[test]
fn setup_does_not_claim_success_when_the_config_write_fails() {
    let env = setup("setupnowrite");
    run(&env, &["init"], None);
    // A directory where the config file goes: every read and write of it fails.
    fs::create_dir_all(env.data.join("config")).unwrap();
    let (out, err, ok) = run_env::<&str>(&env, &["setup", "--no-mirror"], None, &[]);
    assert!(
        !out.contains("no mirror — snapshots stay in") && !out.contains("setup done"),
        "claimed success after a failed config write:\n{out}"
    );
    assert!(
        err.contains("could not save config"),
        "no error for the failed config write:\n{err}"
    );
    assert!(!ok, "a config write that failed must not exit 0");
}

/// The passphrase question is gated on a terminal, so a scripted `setup
/// --mirror` sets up a plaintext mirror without asking. That gate is what
/// keeps the interactive fail-closed path off this suite (it needs a pty), so
/// pin it: if setup ever starts prompting on a pipe, this changes.
#[test]
fn setup_with_a_pipe_does_not_ask_about_a_passphrase() {
    let env = setup("setuppiped");
    let mirror = env.data.join("m");
    run(&env, &["init"], None);
    let (out, err, ok) = run_env::<&str>(
        &env,
        &["setup", "--mirror", mirror.to_str().unwrap()],
        None,
        &[],
    );
    assert!(ok && out.contains("setup done"), "setup failed: {out}{err}");
    assert!(
        !out.contains("[y/N]"),
        "asked a question with no terminal to answer it:\n{out}"
    );
    assert!(
        mirror.join("subrosa/subrosa-latest.db").is_file(),
        "expected a plaintext mirror, got: {out}"
    );
}

/// Claude Code stops reading MEMORY.md at line 200, so selection has to stop
/// there too — bytes alone would happily emit hundreds of short lines.
#[test]
fn line_cap_drops_facts_the_byte_budget_would_have_kept() {
    let env = setup("linecap");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    // 250 tiny facts: well under 23000 bytes, well over 200 lines.
    let mut index = String::from("# Memory Index\n\n");
    for i in 0..250 {
        let leaf = format!("reference_l{i}.md");
        fs::write(
            memdir.join(&leaf),
            format!("---\nname: l{i}\ndescription: short {i}\n---\nbody\n"),
        )
        .unwrap();
        index.push_str(&format!("- [l{i}]({leaf}) — short {i}\n"));
    }
    fs::write(memdir.join("MEMORY.md"), &index).unwrap();
    assert!(index.len() < 23000, "fixture must fit the byte budget");
    let md = memdir.to_str().unwrap();
    run(&env, &["import", md, "--no-backup"], None);

    let (out, err) = run(&env, &["generate", "--memdir", md, "--dry-run"], None);
    assert!(
        out.lines().count() <= 200,
        "index ran past the 200-line cap: {} lines",
        out.lines().count()
    );
    assert!(
        out.len() < 23000,
        "the byte budget alone would not have dropped anything"
    );
    assert!(
        err.contains("dropped below budget"),
        "line-capped facts should report through the dropped list:\n{err}"
    );
}

/// One fact renders as one line whatever is stored in it — a newline in a
/// title would otherwise split the entry and slip past the line cap.
#[test]
fn a_multiline_title_still_renders_as_one_line() {
    let env = setup("multiline");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(
        memdir.join("reference_ml.md"),
        "---\nname: ml\ndescription: short\n---\nbody\n",
    )
    .unwrap();
    let md = memdir.to_str().unwrap();
    run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "reference_ml.md",
            "--memdir",
            md,
            "--title",
            "first\nsecond\rthird",
        ],
        None,
    );

    let (out, _) = run(&env, &["generate", "--memdir", md, "--dry-run"], None);
    // Header (2 lines) plus exactly one fact line.
    assert_eq!(
        out.lines().count(),
        3,
        "a multiline title split the entry:\n{out}"
    );
    let line = out
        .lines()
        .find(|l| l.contains("reference_ml.md"))
        .expect("index line for the fact");
    assert!(
        line.starts_with("- [first second third](") && line.ends_with("— short"),
        "line format broke: {line}"
    );
}

/// The hook reads .budget too, and hooks own neither stream: stdout is the
/// injected context and stderr is thrown away by run.sh. A bad file goes to
/// the log instead.
#[test]
fn bad_budget_stays_out_of_the_hook_streams() {
    let env = setup("budgethook");
    let memdir = env.projects.join("-tmp-demo/memory");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(memdir.join("MEMORY.md"), "x".repeat(30_000)).unwrap();
    fs::write(memdir.join(".budget"), "junk\n").unwrap();

    let (out, err, ok) = run_env::<&str>(
        &env,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/tmp/demo","session_id":"live-budget"}"#),
        &[],
    );
    assert!(ok, "hooks always exit 0");
    assert_eq!(err, "", "hook stderr must stay clean, got:\n{err}");
    // It still falls back to the default budget, so the size nudge fires.
    assert!(
        out.contains("near the always-loaded cap"),
        "expected the size nudge, got:\n{out}"
    );
    let log = fs::read_to_string(env.data.join("hook.log")).unwrap_or_default();
    assert!(
        log.contains(".budget"),
        "complaint never reached the log:\n{log}"
    );
}

/// Backdate a file's mtime so the staleness gates treat it as abandoned.
fn backdate(p: &PathBuf, secs: u64) {
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_times(fs::FileTimes::new().set_modified(when))
        .unwrap();
}

/// Newest local snapshot. Names are timestamped, so the highest path sorts last.
fn newest_snapshot(env: &TestEnv) -> PathBuf {
    fs::read_dir(env.data.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("snapshot-"))
                .unwrap_or(false)
        })
        .max()
        .expect("no local snapshot")
}

#[test]
fn mirror_stays_plaintext_without_a_passphrase() {
    let env = setup("mirrorplain");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("SUBROSA_MIRROR", mirror.to_str().unwrap())],
    );
    assert!(ok, "backup failed: {err}");
    assert!(
        out.contains("+ mirror") && !out.contains("encrypted"),
        "unexpected label: {out}"
    );
    assert!(mirror.join("subrosa-latest.db").is_file(), "no mirror copy");
    assert!(
        !mirror.join("subrosa-latest.db.enc").exists(),
        "encrypted a mirror with no passphrase set"
    );
}

/// A sealed mirror plus a passphrase that went missing is a configuration
/// problem, not permission to republish the archive in the clear.
#[test]
fn losing_the_passphrase_never_downgrades_to_plaintext() {
    let env = setup("mirrorinverse");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    fs::write(mirror.join("subrosa-latest.db.enc"), b"SUBROSA1 sealed").unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("SUBROSA_MIRROR", mirror.to_str().unwrap())],
    );
    assert!(ok, "the local snapshot should still succeed: {err}");
    assert!(
        err.contains("not downgrading") && !out.contains("+ mirror"),
        "expected a refusal, got out={out} err={err}"
    );
    assert!(
        !mirror.join("subrosa-latest.db").exists(),
        "wrote plaintext next to a sealed mirror"
    );
    assert!(
        mirror.join("subrosa-latest.db.enc").exists(),
        "deleted the sealed mirror instead of refusing"
    );
}

/// `mirror=none` is only written by a deliberate opt-out, so a variable
/// exported in a shell profile must not quietly switch mirroring back on.
#[test]
fn config_none_outranks_a_mirror_env_var() {
    let env = setup("mirrornonewins");
    let mirror = env.data.join("from-the-env");
    run(&env, &["init"], None);
    fs::write(env.data.join("config"), "mirror=none\n").unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("SUBROSA_MIRROR", mirror.to_str().unwrap())],
    );
    assert!(ok, "backup failed: {err}");
    assert!(!out.contains("+ mirror"), "claimed a mirror: {out}");
    assert!(
        !mirror.exists(),
        "the env var overrode a deliberate opt-out"
    );
}

/// The other half of the same rule: with a real path in the config, the env
/// var still wins. Only the `none` sentinel is special.
#[test]
fn the_env_var_still_beats_a_configured_path() {
    let env = setup("mirrorenvwins");
    let from_config = env.data.join("from-the-config");
    let from_env = env.data.join("from-the-env");
    run(&env, &["init"], None);
    fs::write(
        env.data.join("config"),
        format!("mirror={}\n", from_config.display()),
    )
    .unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("SUBROSA_MIRROR", from_env.to_str().unwrap())],
    );
    assert!(ok && out.contains("+ mirror"), "backup failed: {out}{err}");
    assert!(
        from_env.join("subrosa-latest.db").is_file(),
        "the env var should have won"
    );
    assert!(
        !from_config.exists(),
        "wrote to the configured path instead"
    );
}

/// The opt-out has to hold against the same ambient env var.
#[test]
fn setup_no_mirror_holds_against_a_mirror_env_var() {
    let env = setup("setupoptout");
    let mirror = env.data.join("from-the-env");
    run(&env, &["init"], None);
    let (out, err, ok) = run_env::<&str>(&env, &["setup", "--no-mirror"], None, &[]);
    assert!(
        ok && out.contains("no mirror"),
        "opt-out failed: {out}{err}"
    );

    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("SUBROSA_MIRROR", mirror.to_str().unwrap())],
    );
    assert!(ok, "backup failed: {err}");
    assert!(
        !out.contains("+ mirror") && !mirror.exists(),
        "the env var undid `setup --no-mirror`: {out}"
    );
}

/// A plaintext twin that reappears — cloud version history, an old binary —
/// has to go on every path where encryption was asked for, including the ones
/// that then bail out. Covers forced and throttled runs under both a broken
/// passphrase and a missing one with a sealed mirror already present.
#[cfg(unix)]
#[test]
fn plaintext_twin_is_purged_even_when_the_mirror_then_fails() {
    use std::os::unix::ffi::OsStringExt;

    let env = setup("mirrorbailout");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    fs::write(mirror.join("subrosa-latest.db.enc"), b"SUBROSA1 sealed").unwrap();
    let twin = mirror.join("subrosa-latest.db");

    // (i) passphrase set but unreadable, (ii) no passphrase but a .enc present.
    let broken = std::ffi::OsString::from_vec(vec![0x66, 0xff, 0x6f]);
    let cases: [(&str, Vec<(&str, std::ffi::OsString)>); 2] = [
        (
            "unreadable passphrase",
            vec![
                ("SUBROSA_MIRROR", mirror.as_os_str().to_owned()),
                ("SUBROSA_MIRROR_PASSPHRASE", broken),
            ],
        ),
        (
            "missing passphrase, sealed mirror present",
            vec![("SUBROSA_MIRROR", mirror.as_os_str().to_owned())],
        ),
    ];
    for (what, envs) in cases {
        for args in [&["backup", "--force"][..], &["backup"][..]] {
            fs::write(&twin, b"reappeared").unwrap();
            let (out, err, ok) = run_env(&env, args, None, &envs);
            assert!(ok, "{what} / {args:?}: backup should still exit 0: {err}");
            assert!(
                !out.contains("+ mirror"),
                "{what} / {args:?}: must not claim a mirror: {out}"
            );
            assert!(
                !twin.exists(),
                "{what} / {args:?}: plaintext twin survived the bailout"
            );
        }
    }
}

/// The whole point of the mirror passphrase is undone if a session that typed
/// it gets archived in the clear — and then sealed into the mirror with itself.
#[test]
fn the_mirror_passphrase_never_reaches_the_archive() {
    let env = setup("passphraseredact");
    // A real passphrase has spaces in it, so a value pattern that stops at the
    // first space archives most of the secret.
    ingest(
        &env,
        "pppp-1111",
        &[
            user_rec(
                "2026-06-12T01:00:00Z",
                "u1",
                "SUBROSA_MIRROR_PASSPHRASE=\\\"correct horse battery staple\\\" subrosa backup",
            ),
            user_rec(
                "2026-06-12T01:01:00Z",
                "u2",
                "mirror_passphrase=correct horse battery staple",
            ),
            user_rec(
                "2026-06-12T01:02:00Z",
                "u3",
                "export SUBROSA_MIRROR_PASSPHRASE='correct horse battery staple'",
            ),
            // An escaped quote inside the value must not end the mask early.
            // Reads as: SUBROSA_MIRROR_PASSPHRASE="correct \"horse ...\"" backup
            user_rec(
                "2026-06-12T01:03:00Z",
                "u4",
                r#"SUBROSA_MIRROR_PASSPHRASE=\"correct \\\"horse battery staple\\\"\" backup"#,
            ),
        ],
    );
    let (dump, _) = run(&env, &["session", "pppp-1111"], None);
    for word in ["correct", "horse", "battery", "staple"] {
        assert!(
            !dump.contains(word),
            "{word:?} from the passphrase reached the archive:\n{dump}"
        );
    }
    assert_eq!(
        dump.matches("‹redacted›").count(),
        4,
        "every value should be masked:\n{dump}"
    );
    // The key names stay readable, so the turn is still worth searching.
    assert!(
        dump.contains("SUBROSA_MIRROR_PASSPHRASE") && dump.contains("mirror_passphrase"),
        "masking ate the key names:\n{dump}"
    );
}

/// The passphrase rule takes the rest of the line, so its separator has to
/// stop at the newline — otherwise a sentence ending in "the passphrase:"
/// wipes whatever block follows it.
#[test]
fn a_trailing_passphrase_word_does_not_wipe_the_next_block() {
    let env = setup("passphraseeol");
    let rec = r#"{"type":"user","timestamp":"2026-06-12T03:00:00Z","uuid":"e1","cwd":"/tmp/demo","message":{"role":"user","content":[{"type":"text","text":"remind me to set the mirror passphrase:"},{"type":"text","text":"subrosa backup --force"}]}}"#;
    ingest(&env, "eeee-2222", &[rec.to_string()]);

    let (dump, _) = run(&env, &["session", "eeee-2222"], None);
    assert!(
        dump.contains("subrosa backup --force"),
        "the next block was wiped:\n{dump}"
    );
    assert!(
        !dump.contains("‹redacted›"),
        "nothing to mask here:\n{dump}"
    );
}

/// A turn is every block joined with "\n", and ingest caps tool blocks before
/// redaction — so subrosa itself manufactures unterminated quotes mid-value.
/// A pattern that hunted for a closing quote would run on to the next block's
/// opening quote and archive the secret sitting between them.
#[test]
fn a_capped_block_cannot_expose_the_next_blocks_secret() {
    let env = setup("cappedblock");
    // Long enough that the 500-char tool_result cap cuts inside the value,
    // leaving the opening quote with no partner.
    let filler = "a".repeat(600);
    let rec = format!(
        r#"{{"type":"user","timestamp":"2026-06-12T02:00:00Z","uuid":"m1","cwd":"/tmp/demo","message":{{"role":"user","content":[{{"type":"tool_result","content":"API_KEY=\"{filler}"}},{{"type":"text","text":"PASSWORD=\"s3cr3t-live-value\" done"}}]}}}}"#
    );
    ingest(&env, "mmmm-1111", &[rec]);

    let (dump, _) = run(&env, &["session", "mmmm-1111"], None);
    assert!(
        !dump.contains("s3cr3t-live-value"),
        "the next block's secret reached the archive:\n{dump}"
    );
    assert!(
        dump.contains("PASSWORD") && dump.contains("API_KEY"),
        "a key name was swallowed:\n{dump}"
    );
    assert!(
        dump.contains("done"),
        "text after the second secret was swallowed:\n{dump}"
    );
}

/// An evicted PLAINTEXT mirror is only a placeholder here, but the readable
/// object is still in the cloud — deleting the placeholder is what removes it.
/// The sealed placeholder next to it must survive untouched.
#[test]
fn purge_removes_an_evicted_plaintext_placeholder() {
    let env = setup("mirrorevicted");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    let plain_ghost = mirror.join(".subrosa-latest.db.icloud");
    let sealed_ghost = mirror.join(".subrosa-latest.db.enc.icloud");
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    for args in [&["backup", "--force"][..], &["backup"][..]] {
        fs::write(&plain_ghost, b"evicted plaintext").unwrap();
        fs::write(&sealed_ghost, b"evicted ciphertext").unwrap();
        let (_, err, ok) = run_env(&env, args, None, &creds);
        assert!(ok, "{args:?}: backup failed: {err}");
        assert!(
            !plain_ghost.exists(),
            "{args:?}: evicted plaintext placeholder survived"
        );
        assert!(
            sealed_ghost.exists(),
            "{args:?}: purge ate the sealed placeholder"
        );
    }
}

/// iCloud swaps an evicted file for a dot-prefixed placeholder. That still
/// means a sealed mirror is there, so a lost passphrase must not downgrade.
#[test]
fn an_evicted_icloud_placeholder_still_blocks_a_downgrade() {
    let env = setup("mirroricloud");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    fs::write(mirror.join(".subrosa-latest.db.enc.icloud"), b"placeholder").unwrap();
    let twin = mirror.join("subrosa-latest.db");
    fs::write(&twin, b"reappeared").unwrap();

    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("SUBROSA_MIRROR", mirror.to_str().unwrap())],
    );
    assert!(ok, "the local snapshot should still succeed: {err}");
    assert!(
        err.contains("not downgrading") && !out.contains("+ mirror"),
        "an evicted .enc should still refuse: out={out} err={err}"
    );
    assert!(!twin.exists(), "plaintext twin survived");
}

/// A tilde in a hand-edited config would otherwise create a folder literally
/// named "~" and leave the real one empty.
#[test]
fn mirror_path_expands_a_leading_tilde() {
    let env = setup("mirrortilde");
    run(&env, &["init"], None);
    fs::write(env.data.join("config"), "mirror=~/mirror\n").unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("HOME", env.data.to_str().unwrap())],
    );
    assert!(ok && out.contains("+ mirror"), "backup failed: {out}{err}");
    assert!(
        env.data.join("mirror/subrosa-latest.db").is_file(),
        "tilde was not expanded"
    );
    assert!(!env.data.join("~").exists(), "created a literal ~ folder");
}

/// Both mirror branches write the same tmp name, so the purge reads what's
/// inside: a young tmp holding a readable database goes now, a young one
/// holding ciphertext is somebody's live write and stays.
#[test]
fn purge_reads_tmp_contents_not_just_the_name() {
    let env = setup("mirrorlivetmp");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    let readable = mirror.join(".subrosa-latest-424242.tmp");
    let sealed = mirror.join(".subrosa-latest-535353.tmp");
    fs::write(&readable, b"SQLite format 3\0half a plaintext copy").unwrap();
    fs::write(&sealed, b"SUBROSA1 a peer is mid-write").unwrap();

    let (_, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
        ],
    );
    assert!(ok, "backup failed: {err}");
    assert!(
        !readable.exists(),
        "a readable tmp survived because it was young"
    );
    assert!(
        sealed.is_file(),
        "purge deleted a peer's live encrypted tmp"
    );
}

/// A purge that can't finish is a mirror failure, not a success with a quiet
/// shrug — and it happens before sealing, so nothing gets written.
#[test]
fn unpurgeable_plaintext_fails_the_mirror() {
    let env = setup("mirrorstuck");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    // A directory can't be removed with remove_file, so the purge has to fail.
    fs::create_dir_all(mirror.join("subrosa-latest.db")).unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
        ],
    );
    assert!(ok, "the local snapshot should still succeed: {err}");
    assert!(
        err.contains("mirror skipped") && !out.contains("+ mirror"),
        "a failed purge must fail the mirror, got out={out} err={err}"
    );
    assert!(
        !mirror.join("subrosa-latest.db.enc").exists(),
        "sealed anyway with plaintext still sitting there"
    );
}

#[test]
fn mirror_is_encrypted_with_a_passphrase() {
    let env = setup("mirrorenc");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    // Everything plaintext from before encryption was turned on has to go: the
    // twin, and an abandoned .tmp from a plaintext copy that died mid-write.
    fs::create_dir_all(&mirror).unwrap();
    fs::write(mirror.join("subrosa-latest.db"), b"stale plaintext").unwrap();
    let abandoned = mirror.join(".subrosa-latest-999999.tmp");
    fs::write(&abandoned, b"half a copy").unwrap();
    backdate(&abandoned, 7200);

    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
        ],
    );
    assert!(ok, "backup failed: {err}");
    assert!(out.contains("+ mirror (encrypted)"), "unlabelled: {out}");
    let sealed = fs::read(mirror.join("subrosa-latest.db.enc")).unwrap();
    assert!(
        sealed.starts_with(b"SUBROSA1"),
        "no magic on the mirror file"
    );
    assert!(
        !mirror.join("subrosa-latest.db").exists(),
        "stale plaintext twin left in the synced folder"
    );
    assert!(
        !mirror.join(".subrosa-latest-999999.tmp").exists(),
        "stale plaintext .tmp left in the synced folder"
    );
}

/// The throttled path is what actually runs on most session ends: it has no
/// snapshot to mirror, but a plaintext copy appearing in the synced folder
/// still has to be cleared.
#[test]
fn throttled_backup_still_clears_plaintext() {
    let env = setup("mirrorthrottle");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");

    fs::write(mirror.join("subrosa-latest.db"), b"appeared later").unwrap();
    let (out, err, ok) = run_env(&env, &["backup"], None, &creds);
    assert!(
        ok && out.contains("throttled"),
        "expected a throttled run: {out}{err}"
    );
    assert!(
        !mirror.join("subrosa-latest.db").exists(),
        "throttled run left plaintext in the synced folder"
    );
}

/// A passphrase that's set but unreadable must skip the mirror, not quietly
/// downgrade to writing the archive out in the clear.
#[cfg(unix)]
#[test]
fn unreadable_env_passphrase_skips_the_mirror() {
    use std::os::unix::ffi::OsStringExt;

    let env = setup("mirrorbadenv");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            (
                "SUBROSA_MIRROR",
                std::ffi::OsString::from(mirror.to_str().unwrap()),
            ),
            (
                "SUBROSA_MIRROR_PASSPHRASE",
                std::ffi::OsString::from_vec(vec![0x66, 0xff, 0x6f]),
            ),
        ],
    );
    assert!(ok, "backup itself should still succeed: {err}");
    assert!(
        err.contains("mirror skipped") && !out.contains("+ mirror"),
        "should have skipped the mirror, got out={out} err={err}"
    );
    assert!(
        !mirror.join("subrosa-latest.db").exists(),
        "fell back to plaintext with an unusable passphrase"
    );
}

/// One trimming rule everywhere: a padded env value and the trimmed value in
/// the config file are the same passphrase.
#[test]
fn passphrase_is_trimmed_across_env_and_config() {
    let env = setup("mirrortrim");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    let (_, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "  padded pass  "),
        ],
    );
    assert!(ok, "backup failed: {err}");

    // Same value, stored trimmed in the config, opens what the env sealed.
    fs::write(env.data.join("config"), "mirror_passphrase=padded pass\n").unwrap();
    let out = env.data.join("restored.db");
    let (_, err, ok) = run_env::<&str>(
        &env,
        &[
            "restore",
            mirror.join("subrosa-latest.db.enc").to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
        None,
        &[],
    );
    assert!(
        ok && out.is_file(),
        "config passphrase didn't open it: {err}"
    );
}

#[test]
fn config_passphrase_survives_an_empty_env_var() {
    let env = setup("mirrorcfg");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    fs::write(
        env.data.join("config"),
        "mirror_passphrase=from the config\n",
    )
    .unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", ""),
        ],
    );
    assert!(ok, "backup failed: {err}");
    assert!(
        out.contains("+ mirror (encrypted)") && mirror.join("subrosa-latest.db.enc").is_file(),
        "empty env var shadowed the config passphrase: {out}"
    );
}

#[test]
fn restore_round_trips_the_snapshot() {
    let env = setup("restore");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");

    let enc = mirror.join("subrosa-latest.db.enc");
    let out = env.data.join("restored.db");
    let args = [
        "restore",
        enc.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ];
    let (stdout, err, ok) = run_env(&env, &args, None, &creds);
    assert!(ok, "restore failed: {err}");
    assert!(stdout.contains("immutable=1"), "no next steps:\n{stdout}");
    assert_eq!(
        fs::read(&out).unwrap(),
        fs::read(newest_snapshot(&env)).unwrap(),
        "restored bytes differ from the snapshot"
    );

    // No --force flag yet: an existing target is never overwritten.
    let (_, err, ok) = run_env(&env, &args, None, &creds);
    assert!(
        !ok && err.contains("already exists"),
        "should refuse an existing target: {err}"
    );

    // Without --out it lands in the cwd. The happy path needs a cwd unrelated
    // to the input's folder — a parent of it is refused, and rightly so.
    let mut env = env;
    let elsewhere = env.data.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    env.cwd = elsewhere.clone();
    let (_, err, ok) = run_env(&env, &args[..2], None, &creds);
    assert!(ok, "default-path restore failed: {err}");
    assert!(
        elsewhere.join("subrosa-latest.db").is_file() && !mirror.join("subrosa-latest.db").exists(),
        "default output went to the wrong folder"
    );
}

/// The dangerous cwd: run it from inside the mirror folder and the default
/// target would be the decrypted archive sitting next to the encrypted one.
#[test]
fn restore_refuses_to_default_into_the_synced_folder() {
    let mut env = setup("restoresynced");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");

    env.cwd = mirror.clone();
    let enc = mirror.join("subrosa-latest.db.enc");
    let (_, err, ok) = run_env(&env, &["restore", enc.to_str().unwrap()], None, &creds);
    assert!(
        !ok && err.contains("--out"),
        "should refuse to write next to the input: {err}"
    );
    assert!(
        !mirror.join("subrosa-latest.db").exists(),
        "wrote plaintext into the synced folder"
    );

    // A bare relative name has an empty parent — the shape that made an
    // earlier version of this guard pass everything through.
    let (_, err, ok) = run_env(&env, &["restore", "subrosa-latest.db.enc"], None, &creds);
    assert!(
        !ok && err.contains("--out"),
        "relative input should refuse too: {err}"
    );
    assert!(
        !mirror.join("subrosa-latest.db").exists(),
        "relative input wrote plaintext into the synced folder"
    );

    // A subfolder of the mirror is still inside the synced tree.
    let sub = mirror.join("sub");
    fs::create_dir_all(&sub).unwrap();
    env.cwd = sub.clone();
    let (_, err, ok) = run_env(&env, &["restore", enc.to_str().unwrap()], None, &creds);
    assert!(
        !ok && err.contains("--out"),
        "a mirror subfolder should refuse too: {err}"
    );
    assert!(
        !sub.join("subrosa-latest.db").exists(),
        "wrote plaintext into a mirror subfolder"
    );
    env.cwd = mirror.clone();

    // An explicit --out into the mirror dir is the user's call: warn, allow.
    let forced = mirror.join("on-purpose.db");
    let (_, err, ok) = run_env(
        &env,
        &[
            "restore",
            enc.to_str().unwrap(),
            "--out",
            forced.to_str().unwrap(),
        ],
        None,
        &creds,
    );
    assert!(ok && forced.is_file(), "explicit --out should work: {err}");
    assert!(
        err.contains("warning"),
        "no warning for the mirror dir: {err}"
    );
}

/// On a recovery machine there's no mirror configured, so the folders around
/// the encrypted file are the only signal left: inside it, or exactly one
/// level up (setup mirrors into `<cloud root>/subrosa`, so the parent is the
/// cloud root). No further — the grandparent is normally just $HOME.
#[test]
fn restore_guards_the_folder_tree_without_a_mirror_config() {
    let mut env = setup("restoretree");
    let synced = env.data.join("cloud");
    let inner = synced.join("subrosa");
    run(&env, &["init"], None);
    let creds = [
        ("SUBROSA_MIRROR", inner.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");
    let enc = inner.join("subrosa-latest.db.enc");

    // From here on, nothing points at a mirror — only the tree shape does.
    let recovery = [("SUBROSA_MIRROR_PASSPHRASE", "correct horse")];
    let deeper = inner.join("deeper");
    fs::create_dir_all(&deeper).unwrap();
    env.cwd = deeper.clone();
    let (_, err, ok) = run_env(&env, &["restore", enc.to_str().unwrap()], None, &recovery);
    assert!(
        !ok && err.contains("--out"),
        "a folder below the encrypted file should refuse: {err}"
    );
    assert!(
        !deeper.join("subrosa-latest.db").exists(),
        "wrote plaintext"
    );

    // The cloud-root shape: cd <cloud root> && subrosa restore subrosa/....
    env.cwd = synced.clone();
    let (_, err, ok) = run_env(
        &env,
        &["restore", "subrosa/subrosa-latest.db.enc"],
        None,
        &recovery,
    );
    assert!(
        !ok && err.contains("--out"),
        "the cloud root should refuse: {err}"
    );
    assert!(
        !synced.join("subrosa-latest.db").exists(),
        "wrote plaintext"
    );

    // Two levels up is the README's own example — $HOME with the archive down
    // in ~/Library/... — and it has to keep working. Sweeping every ancestor
    // would refuse this and say $HOME is cloud-synced, which it isn't.
    let home = env.data.clone();
    env.cwd = home.clone();
    let (_, err, ok) = run_env(&env, &["restore", enc.to_str().unwrap()], None, &recovery);
    assert!(ok, "two levels up must still work: {err}");
    assert!(
        home.join("subrosa-latest.db").is_file(),
        "nothing was written"
    );
}

/// The mirror lives at `<cloud root>/subrosa`, so its siblings are synced too.
/// Writing a readable archive into one of them is the same leak by another
/// door — with or without a mirror configured.
#[test]
fn restore_refuses_a_sibling_of_the_mirror() {
    let mut env = setup("restoresibling");
    let cloud = env.data.join("cloud");
    let mirror = cloud.join("subrosa");
    let sibling = cloud.join("other");
    fs::create_dir_all(&sibling).unwrap();
    run(&env, &["init"], None);
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");
    let enc = mirror.join("subrosa-latest.db.enc");

    env.cwd = sibling.clone();
    let (_, err, ok) = run_env(&env, &["restore", enc.to_str().unwrap()], None, &creds);
    assert!(
        !ok && err.contains("--out"),
        "a sibling of the mirror should refuse: {err}"
    );

    // Same shape on a recovery machine, where only the input's own path says
    // where the synced folder is.
    let recovery = [("SUBROSA_MIRROR_PASSPHRASE", "correct horse")];
    let (_, err, ok) = run_env(&env, &["restore", enc.to_str().unwrap()], None, &recovery);
    assert!(
        !ok && err.contains("--out"),
        "a sibling should refuse with no mirror configured either: {err}"
    );
    assert!(
        !sibling.join("subrosa-latest.db").exists(),
        "wrote plaintext into a synced sibling"
    );
}

/// A mirror straight under $HOME would make the whole home directory the
/// "synced root", and then the refusal's own advice — pass a path outside it —
/// would be impossible to follow. That case drops back to the narrow rule.
#[test]
fn a_mirror_under_home_does_not_refuse_all_of_home() {
    let mut env = setup("restorehomemirror");
    let home = env.data.join("home");
    let mirror = home.join("subrosa");
    let work = home.join("work");
    fs::create_dir_all(&work).unwrap();
    run(&env, &["init"], None);
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
        ("HOME", home.to_str().unwrap()),
    ];
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");
    let enc = mirror.join("subrosa-latest.db.enc");

    // Somewhere else under $HOME is fine — otherwise there'd be nowhere to go.
    env.cwd = work.clone();
    let (_, err, ok) = run_env(&env, &["restore", enc.to_str().unwrap()], None, &creds);
    assert!(ok, "a folder under $HOME should still work: {err}");
    assert!(
        work.join("subrosa-latest.db").is_file(),
        "nothing was written"
    );

    // The mirror folder itself is still off limits.
    let deeper = mirror.join("deeper");
    fs::create_dir_all(&deeper).unwrap();
    env.cwd = deeper.clone();
    let (_, err, ok) = run_env(&env, &["restore", enc.to_str().unwrap()], None, &creds);
    assert!(
        !ok && err.contains("--out"),
        "inside the mirror should still refuse: {err}"
    );
    assert!(
        !deeper.join("subrosa-latest.db").exists(),
        "wrote plaintext inside the mirror"
    );
}

/// The sibling rule keys off the `subrosa` leaf that setup always creates.
/// A hand-copied file somewhere ordinary keeps the narrow rule, or every
/// sibling of ~/Downloads would be refused too.
#[test]
fn restore_keeps_the_narrow_rule_for_an_ordinary_folder() {
    let mut env = setup("restorenarrow");
    let downloads = env.data.join("downloads");
    let elsewhere = env.data.join("elsewhere");
    fs::create_dir_all(&downloads).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    run(&env, &["init"], None);
    let plain_mirror = env.data.join("m");
    let creds = [
        ("SUBROSA_MIRROR", plain_mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");
    fs::copy(
        plain_mirror.join("subrosa-latest.db.enc"),
        downloads.join("subrosa-latest.db.enc"),
    )
    .unwrap();

    // A sibling of downloads/ is nothing to do with a synced folder.
    env.cwd = elsewhere.clone();
    let (_, err, ok) = run_env(
        &env,
        &[
            "restore",
            downloads.join("subrosa-latest.db.enc").to_str().unwrap(),
        ],
        None,
        &[("SUBROSA_MIRROR_PASSPHRASE", "correct horse")],
    );
    assert!(ok, "an ordinary sibling should still work: {err}");
    assert!(
        elsewhere.join("subrosa-latest.db").is_file(),
        "nothing was written"
    );
}

/// A mirror folder that won't resolve — deleted leaf, unmounted volume — must
/// not quietly switch the check off; it falls back to the configured path.
#[test]
fn restore_still_guards_an_unresolvable_mirror() {
    let mut env = setup("restoreghostmirror");
    let cloud = env.data.join("cloud");
    let ghost = cloud.join("never-created");
    let downloads = env.data.join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    fs::create_dir_all(&cloud).unwrap();
    run(&env, &["init"], None);

    // Seal one in a folder that does exist, then copy it out of the way.
    let real = env.data.join("real");
    let (_, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", real.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
        ],
    );
    assert!(ok, "backup failed: {err}");
    let copied = downloads.join("subrosa-latest.db.enc");
    fs::copy(real.join("subrosa-latest.db.enc"), &copied).unwrap();

    // Now point the config at a folder that was never created.
    env.cwd = cloud.clone();
    let (_, err, ok) = run_env(
        &env,
        &["restore", copied.to_str().unwrap()],
        None,
        &[
            ("SUBROSA_MIRROR", ghost.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
        ],
    );
    assert!(
        !ok && err.contains("--out"),
        "an unresolvable mirror should still guard its parent: {err}"
    );
    assert!(
        err.contains("cannot resolve the mirror folder"),
        "should say the check ran on the configured path: {err}"
    );
    assert!(
        !cloud.join("subrosa-latest.db").exists(),
        "wrote plaintext into the cloud root"
    );
}

/// With a mirror configured, standing in its parent is the cloud root even
/// when the file you're restoring came from somewhere else entirely.
#[test]
fn restore_guards_the_mirror_parent_for_an_input_from_elsewhere() {
    let mut env = setup("restoremirrorparent");
    let cloud = env.data.join("cloud");
    let mirror = cloud.join("subrosa");
    let downloads = env.data.join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    run(&env, &["init"], None);
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");

    // Copied out of the synced folder, so the input's own folder is harmless.
    let copied = downloads.join("subrosa-latest.db.enc");
    fs::copy(mirror.join("subrosa-latest.db.enc"), &copied).unwrap();
    env.cwd = cloud.clone();
    let (_, err, ok) = run_env(&env, &["restore", copied.to_str().unwrap()], None, &creds);
    assert!(
        !ok && err.contains("--out"),
        "the mirror's parent should refuse: {err}"
    );
    assert!(
        !cloud.join("subrosa-latest.db").exists(),
        "wrote plaintext into the cloud root"
    );

    // mirror=none stops subrosa writing there. It does not make the folder
    // any less cloud-synced, so this guard still has to fire.
    fs::write(env.data.join("config"), "mirror=none\n").unwrap();
    let (_, err, ok) = run_env(&env, &["restore", copied.to_str().unwrap()], None, &creds);
    assert!(
        !ok && err.contains("--out"),
        "mirror=none blinded the restore guard: {err}"
    );
    assert!(
        !cloud.join("subrosa-latest.db").exists(),
        "wrote plaintext into the cloud root under mirror=none"
    );

    // An explicit --out stays the user's call: warn, then proceed.
    let forced = cloud.join("on-purpose.db");
    let (_, err, ok) = run_env(
        &env,
        &[
            "restore",
            copied.to_str().unwrap(),
            "--out",
            forced.to_str().unwrap(),
        ],
        None,
        &creds,
    );
    assert!(ok && forced.is_file(), "explicit --out should work: {err}");
    assert!(
        err.contains("warning"),
        "no warning under mirror=none: {err}"
    );
}

/// The write path honours mirror=none, but a readable copy already sitting
/// beside a sealed one still has to go — removing it takes data out of the
/// cloud. With no sealed file there, the opt-out means hands off.
#[test]
fn sentinel_still_clears_a_twin_next_to_a_sealed_mirror() {
    let env = setup("sentineltwin");
    let mirror = env.data.join("from-the-env");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    fs::write(env.data.join("config"), "mirror=none\n").unwrap();
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let twin = mirror.join("subrosa-latest.db");

    // Sealed file present: the twin goes, even on the throttled path.
    fs::write(mirror.join("subrosa-latest.db.enc"), b"SUBROSA1 sealed").unwrap();
    fs::write(&twin, b"readable copy").unwrap();
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");
    assert!(!twin.exists(), "twin survived next to a sealed mirror");
    let (out, err, ok) = run_env(&env, &["backup"], None, &creds);
    assert!(
        ok && out.contains("throttled"),
        "expected a throttle: {out}{err}"
    );
    fs::write(&twin, b"came back").unwrap();
    run_env(&env, &["backup"], None, &creds);
    assert!(!twin.exists(), "throttled run left the twin behind");

    // No sealed file: the opt-out wins and we don't touch the folder.
    fs::remove_file(mirror.join("subrosa-latest.db.enc")).unwrap();
    fs::write(&twin, b"not ours to delete").unwrap();
    run_env(&env, &["backup"], None, &creds);
    assert!(
        twin.exists(),
        "deleted a file in a folder we were told to stay out of"
    );
}

/// The mirror image of the sentinel rule: `SUBROSA_MIRROR=none` turns writing
/// off, but a real folder named in the config is still a synced folder, so the
/// protective paths have to keep seeing it.
#[test]
fn env_none_does_not_blind_the_guards_for_a_configured_mirror() {
    let mut env = setup("envnoneguards");
    let cloud = env.data.join("cloud");
    let mirror = cloud.join("subrosa");
    let downloads = env.data.join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    run(&env, &["init"], None);
    fs::write(
        env.data.join("config"),
        format!(
            "mirror={}\nmirror_passphrase=correct horse\n",
            mirror.display()
        ),
    )
    .unwrap();
    let (_, err, ok) = run_env::<&str>(&env, &["backup", "--force"], None, &[]);
    assert!(ok, "backup failed: {err}");

    let copied = downloads.join("subrosa-latest.db.enc");
    fs::copy(mirror.join("subrosa-latest.db.enc"), &copied).unwrap();
    let twin = mirror.join("subrosa-latest.db");
    fs::write(&twin, b"readable copy").unwrap();

    // From here the env var opts out of writing — and nothing more.
    let off = [("SUBROSA_MIRROR", "none")];
    env.cwd = cloud.clone();
    let (_, err, ok) = run_env(&env, &["restore", copied.to_str().unwrap()], None, &off);
    assert!(
        !ok && err.contains("--out"),
        "SUBROSA_MIRROR=none blinded the restore guard: {err}"
    );
    assert!(
        !cloud.join("subrosa-latest.db").exists(),
        "wrote plaintext into the cloud root"
    );

    let (out, err, ok) = run_env(&env, &["backup", "--force"], None, &off);
    assert!(ok, "backup failed: {err}");
    // Both halves of the promise: writing is off, and the twin still goes.
    assert!(
        !out.contains("+ mirror"),
        "SUBROSA_MIRROR=none still wrote a mirror: {out}"
    );
    assert!(!twin.exists(), "twin survived under SUBROSA_MIRROR=none");

    // A trailing space must not defeat the sentinel — that would create a
    // folder literally called "none " and fill it with a readable copy.
    fs::write(&twin, b"came back").unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("SUBROSA_MIRROR", "none ")],
    );
    assert!(ok, "backup failed: {err}");
    assert!(
        !out.contains("+ mirror"),
        "a padded none still wrote a mirror: {out}"
    );
    assert!(!twin.exists(), "a padded none blinded the purge");
}

/// Only one folder wins the write, but when env and config name different
/// real ones the loser is just as cloud-synced. Both have to be guarded, and
/// both have to be cleaned.
#[test]
fn both_named_mirror_folders_are_guarded_and_cleaned() {
    let mut env = setup("twomirrors");
    let cloud_a = env.data.join("cloud-a");
    let a = cloud_a.join("subrosa");
    let b = env.data.join("cloud-b").join("subrosa");
    run(&env, &["init"], None);

    // Seal one into each, so both hold a .enc.
    for dir in [&a, &b] {
        let (_, err, ok) = run_env(
            &env,
            &["backup", "--force"],
            None,
            &[
                ("SUBROSA_MIRROR", dir.to_str().unwrap()),
                ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
            ],
        );
        assert!(ok, "seeding {} failed: {err}", dir.display());
    }
    // From here the config names A and the env names B: B wins the write.
    fs::write(
        env.data.join("config"),
        format!("mirror={}\nmirror_passphrase=correct horse\n", a.display()),
    )
    .unwrap();
    let creds = [("SUBROSA_MIRROR", b.to_str().unwrap())];

    // Restoring B's file while standing in A's cloud root must still refuse.
    env.cwd = cloud_a.clone();
    let (_, err, ok) = run_env(
        &env,
        &["restore", b.join("subrosa-latest.db.enc").to_str().unwrap()],
        None,
        &creds,
    );
    assert!(
        !ok && err.contains("--out"),
        "the folder that lost the write went unguarded: {err}"
    );

    // And a twin in A gets cleared even though B is where we write.
    let twin_a = a.join("subrosa-latest.db");
    fs::write(&twin_a, b"readable copy").unwrap();
    let (out, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(ok, "backup failed: {err}");
    assert!(
        b.join("subrosa-latest.db.enc").is_file() && out.contains("+ mirror (encrypted)"),
        "the env folder should still win the write: {out}"
    );
    assert!(!twin_a.exists(), "the losing folder was never cleaned");
}

/// iCloud's placeholder for an evicted `.tmp` is the double-dot form, which
/// no prefix match ever saw — an evicted partial copy would sit there forever.
#[test]
fn evicted_tmp_placeholders_are_swept() {
    let env = setup("evictedtmp");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    let ghost = mirror.join("..subrosa-latest-424242.tmp.icloud");
    fs::write(&ghost, b"evicted partial copy").unwrap();

    let (_, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
        ],
    );
    assert!(ok, "backup failed: {err}");
    assert!(!ghost.exists(), "evicted tmp placeholder survived");
    assert!(
        mirror.join(".subrosa-latest.db.enc.icloud").exists()
            || mirror.join("subrosa-latest.db.enc").is_file(),
        "the sealed mirror should still be there"
    );
}

/// Whitespace-only is the same as unset: it must not shadow the configured
/// folder the way an empty value already doesn't.
#[test]
fn a_blank_mirror_env_var_falls_through_to_the_config() {
    let env = setup("mirrorblankenv");
    let configured = env.data.join("from-the-config");
    run(&env, &["init"], None);
    fs::write(
        env.data.join("config"),
        format!("mirror={}\n", configured.display()),
    )
    .unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[("SUBROSA_MIRROR", "   ")],
    );
    assert!(ok && out.contains("+ mirror"), "backup failed: {out}{err}");
    assert!(
        configured.join("subrosa-latest.db").is_file(),
        "a blank env var shadowed the configured mirror: {out}"
    );
}

/// Same guarantee on the path that actually runs every day. The CLI backup is
/// the rare one; SessionEnd is the one nobody watches, so it gets its own pin.
#[test]
fn session_end_clears_the_mirror_even_with_an_unopenable_database() {
    let env = setup("hookbrokendb");
    let mirror = env.data.join("from-the-env");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    let sealed = mirror.join("subrosa-latest.db.enc");
    fs::write(&sealed, b"SUBROSA1 sealed").unwrap();
    let twin = mirror.join("subrosa-latest.db");
    fs::write(&twin, b"readable copy").unwrap();

    let broken = env.data.join("not-a-db");
    fs::create_dir_all(&broken).unwrap();
    let (out, err, ok) = run_env(
        &env,
        &["hook", "session-end"],
        Some(r#"{"session_id":"live-x","cwd":"/tmp/demo"}"#),
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
            ("SUBROSA_DB", broken.to_str().unwrap()),
        ],
    );
    assert!(ok, "hooks always exit 0: {err}");
    assert_eq!(out, "", "session-end must keep stdout empty, got:\n{out}");
    assert!(!twin.exists(), "the daily path left the twin exposed");
    assert!(sealed.is_file(), "the sealed mirror was removed");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let log = loop {
        let log = fs::read_to_string(env.data.join("hook.log")).unwrap_or_default();
        if log.contains("session-end worker error") || std::time::Instant::now() >= deadline {
            break log;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(
        log.contains("session-end worker error"),
        "the connect failure never reached the log:\n{log}"
    );
}

/// The purge takes no database, and runs before one is opened. An archive
/// that won't open is the longest a broken state lasts, so it must not also
/// be the state where a readable copy sits in the cloud.
#[test]
fn an_unopenable_database_still_clears_the_mirror() {
    let env = setup("brokendbpurge");
    let mirror = env.data.join("from-the-env");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    fs::write(mirror.join("subrosa-latest.db.enc"), b"SUBROSA1 sealed").unwrap();
    let twin = mirror.join("subrosa-latest.db");
    fs::write(&twin, b"readable copy").unwrap();

    // A directory where the database goes: nothing can open it.
    let broken = env.data.join("not-a-db");
    fs::create_dir_all(&broken).unwrap();
    let (_, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
            ("SUBROSA_DB", broken.to_str().unwrap()),
        ],
    );
    assert!(
        !ok && err.contains("cannot open DB"),
        "expected a DB error: {err}"
    );
    assert!(!twin.exists(), "a broken database left the twin exposed");
}

/// The purge runs before anything that can fail. A run that errors out on the
/// local snapshot is exactly the kind nobody looks at, so it must not be the
/// one that leaves a readable copy sitting in the cloud.
#[test]
fn a_failing_local_backup_still_clears_the_mirror() {
    let env = setup("degradedpurge");
    let mirror = env.data.join("from-the-env");
    run(&env, &["init"], None);
    fs::create_dir_all(&mirror).unwrap();
    fs::write(env.data.join("config"), "mirror=none\n").unwrap();
    // A file where the backups directory goes: create_dir_all fails, so the
    // local snapshot can never start.
    fs::write(env.data.join("backups"), b"not a directory").unwrap();
    let creds = [
        ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
        ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
    ];
    let twin = mirror.join("subrosa-latest.db");

    fs::write(mirror.join("subrosa-latest.db.enc"), b"SUBROSA1 sealed").unwrap();
    fs::write(&twin, b"readable copy").unwrap();
    let (_, err, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(
        !ok && err.contains("backup failed"),
        "expected a failure: {err}"
    );
    assert!(!twin.exists(), "a failing run left the twin exposed");

    // Still hands off when there's no sealed file to justify touching it.
    fs::remove_file(mirror.join("subrosa-latest.db.enc")).unwrap();
    fs::write(&twin, b"not ours to delete").unwrap();
    let (_, _, ok) = run_env(&env, &["backup", "--force"], None, &creds);
    assert!(!ok, "expected a failure");
    assert!(twin.exists(), "deleted a file we were told to stay out of");
}

#[test]
fn restore_rejects_a_wrong_passphrase_and_a_plain_db() {
    let env = setup("restorebad");
    let mirror = env.data.join("mirror");
    run(&env, &["init"], None);
    let (_, err, ok) = run_env(
        &env,
        &["backup", "--force"],
        None,
        &[
            ("SUBROSA_MIRROR", mirror.to_str().unwrap()),
            ("SUBROSA_MIRROR_PASSPHRASE", "correct horse"),
        ],
    );
    assert!(ok, "backup failed: {err}");
    let enc = mirror.join("subrosa-latest.db.enc");
    let out = env.data.join("nope.db");

    let (_, err, ok) = run_env(
        &env,
        &[
            "restore",
            enc.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
        None,
        &[("SUBROSA_MIRROR_PASSPHRASE", "wrong horse")],
    );
    assert!(
        !ok && err.contains("wrong passphrase or corrupted file"),
        "wrong passphrase should fail: {err}"
    );
    assert!(!out.exists(), "a failed restore left a file behind");

    // A plain .db is not a sealed snapshot.
    let plain = env.data.join("memory.db");
    let (_, err, ok) = run_env(
        &env,
        &[
            "restore",
            plain.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
        None,
        &[("SUBROSA_MIRROR_PASSPHRASE", "correct horse")],
    );
    assert!(
        !ok && err.contains("not a subrosa encrypted snapshot"),
        "plain .db should be rejected by magic: {err}"
    );
}

#[test]
fn recall_matches_word_forms() {
    let env = setup("stem");
    ingest(
        &env,
        "ffff-4444",
        &[user_rec(
            "2026-06-12T01:00:00Z",
            "u1",
            "we deployed those services yesterday evening",
        )],
    );
    // No exact token overlap — only the stems match (deploy/deployed,
    // service/services), so this fails without the porter index.
    let payload =
        r#"{"prompt":"deploy the service again maybe","cwd":"/tmp/demo","session_id":"live-4"}"#;
    let (out, _) = run(&env, &["hook", "user-prompt-submit"], Some(payload));
    assert!(
        out.starts_with("[subrosa recall]"),
        "stemmed recall should match word forms, got:\n{out}"
    );
}

#[test]
fn pre_compact_archives_and_resets_dedup() {
    let env = setup("precompact");
    ingest(
        &env,
        "cccc-5555",
        &[user_rec(
            "2026-06-12T01:00:00Z",
            "u1",
            "TICKET-456 cache-prod rollout finished cleanly",
        )],
    );
    let prompt = r#"{"prompt":"status of the cache-prod TICKET-456 rollout","cwd":"/tmp/demo","session_id":"live-9"}"#;
    let (first, _) = run(&env, &["hook", "user-prompt-submit"], Some(prompt));
    assert!(first.starts_with("[subrosa recall]"), "{first}");
    let (repeat, _) = run(&env, &["hook", "user-prompt-submit"], Some(prompt));
    assert_eq!(repeat, "", "dedup should silence the repeat");

    // Compaction fires: the live transcript is archived, dedup forgets live-9.
    let live = env.projects.join("-tmp-demo/live-9.jsonl");
    fs::write(
        &live,
        user_rec(
            "2026-06-12T02:00:00Z",
            "u9",
            "MIGRATE-77 schema migration plan drafted",
        ) + "\n",
    )
    .unwrap();
    let payload = format!(
        r#"{{"session_id":"live-9","transcript_path":"{}","cwd":"/tmp/demo","trigger":"auto"}}"#,
        live.to_str().unwrap()
    );
    let (out, _) = run(&env, &["hook", "pre-compact"], Some(&payload));
    assert_eq!(out, "", "pre-compact must keep stdout empty");
    let (dump, _) = run(&env, &["session", "live-9"], None);
    assert!(
        dump.contains("MIGRATE-77"),
        "mid-session turns not archived:\n{dump}"
    );
    let (again, _) = run(&env, &["hook", "user-prompt-submit"], Some(prompt));
    assert!(
        again.starts_with("[subrosa recall]"),
        "dedup must reset after compaction, got:\n{again}"
    );
}
