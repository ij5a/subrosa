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
}

fn setup(tag: &str) -> TestEnv {
    let root = std::env::temp_dir().join(format!("subrosa-quiet-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let data = root.join("data");
    let projects = root.join("projects");
    fs::create_dir_all(projects.join("-tmp-demo")).unwrap();
    fs::create_dir_all(&data).unwrap();
    TestEnv { data, projects }
}

fn run(env: &TestEnv, args: &[&str], stdin: Option<&str>) -> (String, String) {
    let mut child = Command::new(bin())
        .args(args)
        .env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
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
    )
}

fn user_rec(ts: &str, uuid: &str, text: &str) -> String {
    format!(
        r#"{{"type":"user","timestamp":"{ts}","uuid":"{uuid}","cwd":"/tmp/demo","message":{{"role":"user","content":"{text}"}}}}"#
    )
}

fn ingest(env: &TestEnv, stem: &str, records: &[String]) {
    let t = env.projects.join(format!("-tmp-demo/{stem}.jsonl"));
    fs::write(&t, records.join("\n") + "\n").unwrap();
    run(env, &["ingest", t.to_str().unwrap()], None);
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
    // Seed the checkpoint queue with two sessions (oldest enqueue first).
    fs::write(
        env.data.join("pending-checkpoint.log"),
        "2026-06-12T01:00:00Z\taaaaaaaa-1111\n2026-06-12T02:00:00Z\tbbbbbbbb-2222\n",
    )
    .unwrap();
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
        ids_line.find("bbbbbbbb-2222").unwrap() < ids_line.find("aaaaaaaa-1111").unwrap(),
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
    fs::write(
        env.data.join("pending-checkpoint.log"),
        "2026-06-12T01:00:00Z\taaaaaaaa-1111\n",
    )
    .unwrap();
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
    fs::write(
        env.data.join("pending-checkpoint.log"),
        "2026-06-12T01:00:00Z\taaaaaaaa-1111\n",
    )
    .unwrap();
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
    fs::write(
        env.data.join("pending-checkpoint.log"),
        "2026-06-12T01:00:00Z\taaaaaaaa-1111\n",
    )
    .unwrap();
    let mut child = Command::new(bin())
        .args(["hook", "session-start"])
        .env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
        .env("SUBROSA_CHECKPOINT_NUDGE", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"cwd":"/tmp/demo","session_id":"live-env"}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 session(s) queued for checkpoint")
            && !stdout.contains("ACTION REQUIRED"),
        "empty env must fall back to config (quiet), got:\n{stdout}"
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
