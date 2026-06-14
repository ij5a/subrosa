//! Golden-file tests: the stored-text, session-dump, MEMORY.md, and recall
//! output formats are compatibility-critical — these pin them byte-for-byte.
//! If one fails, the format changed; that needs a deliberate decision, not a fix
//! to the golden file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_subrosa")
}

struct TestEnv {
    data: PathBuf,
    projects: PathBuf,
}

fn setup(tag: &str) -> TestEnv {
    let root = std::env::temp_dir().join(format!("subrosa-golden-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let data = root.join("data");
    let projects = root.join("projects");
    fs::create_dir_all(projects.join("-tmp-demo")).unwrap();
    fs::create_dir_all(&data).unwrap();
    TestEnv { data, projects }
}

fn run(env: &TestEnv, args: &[&str], stdin: Option<&str>) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn golden(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name),
    )
    .unwrap()
}

fn ingest_golden_transcript(env: &TestEnv) -> PathBuf {
    let t = env.projects.join("-tmp-demo/aaaa-bbbb-1111.jsonl");
    fs::write(&t, golden("transcript.jsonl")).unwrap();
    run(env, &["ingest", t.to_str().unwrap()], None);
    t
}

#[test]
fn session_dump_matches_golden() {
    let env = setup("dump");
    ingest_golden_transcript(&env);
    let out = run(&env, &["session", "aaaa-bbbb-1111"], None);
    let want =
        golden("session_dump.golden").replace("{{PROJECTS_DIR}}", env.projects.to_str().unwrap());
    assert_eq!(out, want);
}

#[test]
fn memory_md_matches_golden() {
    let env = setup("gen");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(
        memdir.join("reference_foo.md"),
        "---\nname: foo-endpoints\ndescription: \"Where the foo service endpoints live\"\n\
         metadata:\n  type: reference\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        memdir.join("feedback_bar.md"),
        "---\nname: bar-rule\ndescription: Always do bar before baz\ntype: feedback\n\
         pinned: true\n---\nbody\n",
    )
    .unwrap();
    let md = memdir.to_str().unwrap();
    run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "reference_foo.md",
            "--memdir",
            md,
        ],
        None,
    );
    run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "feedback_bar.md",
            "--memdir",
            md,
            "--pin",
        ],
        None,
    );
    run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "reference_foo.md",
            "--memdir",
            md,
            "--hook",
            "curated hook wins",
        ],
        None,
    );
    let out = run(&env, &["generate", "--memdir", md, "--dry-run"], None);
    assert_eq!(out, golden("memory_md.golden"));
}

#[test]
fn recall_matches_golden() {
    let env = setup("recall");
    ingest_golden_transcript(&env);
    let payload = r#"{"prompt":"how did we handle the cache-prod TICKET-123 rollout?","cwd":"/tmp/demo","session_id":"zzzz-9999"}"#;
    let out = run(&env, &["hook", "user-prompt-submit"], Some(payload));
    assert_eq!(out, golden("recall.golden"));
}

#[test]
fn redaction_and_noise_filtering_hold() {
    let env = setup("filter");
    ingest_golden_transcript(&env);
    let dump = run(&env, &["session", "aaaa-bbbb-1111"], None);
    assert!(!dump.contains("hunter2"), "secret leaked into archive");
    assert!(!dump.contains("injected block"), "recall header archived");
    assert!(!dump.contains("<command-name>"), "command noise archived");
}

#[test]
fn related_matches_golden() {
    let env = setup("related");
    ingest_golden_transcript(&env);
    // A second session sharing cache-prod + TICKET-123 + rollout + cluster, so
    // co-occurrence ranks (one session alone makes every co-occurring term a singleton).
    let t2 = env.projects.join("-tmp-demo/cccc-dddd-2222.jsonl");
    fs::write(
        &t2,
        "{\"type\":\"user\",\"timestamp\":\"2026-06-12T02:00:00Z\",\"uuid\":\"s2u1\",\
         \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"user\",\"content\":\
         \"the cache-prod rollout for TICKET-123 hit the cluster again\"}}\n",
    )
    .unwrap();
    run(&env, &["ingest", t2.to_str().unwrap()], None);
    let out = run(&env, &["related", "cache-prod"], None);
    let want = golden("related.golden").replace("{{PROJECTS_DIR}}", env.projects.to_str().unwrap());
    assert_eq!(out, want);
}
