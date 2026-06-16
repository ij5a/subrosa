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
        // Pin the clock (2027-01-12Z) so recall's age hints render deterministically.
        .env("SUBROSA_NOW", "1799712000")
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

#[test]
fn session_resolves_a_unique_prefix() {
    let env = setup("sessprefix");
    ingest_golden_transcript(&env); // session aaaa-bbbb-1111
    let full = run(&env, &["session", "aaaa-bbbb-1111"], None);
    let pref = run(&env, &["session", "aaaa"], None);
    assert!(!full.is_empty(), "full id must dump");
    assert_eq!(
        pref, full,
        "a unique prefix must dump the same as the full id"
    );
    assert!(
        pref.contains("# session aaaa-bbbb-1111"),
        "header shows the resolved full id, got:\n{pref}"
    );
}

#[test]
fn session_ambiguous_prefix_dumps_nothing() {
    let env = setup("sessambig");
    // Two sessions sharing the prefix "dupe-0000-4000-8000-00000000".
    for sfx in ["1111", "2222"] {
        let f = env
            .projects
            .join(format!("-tmp-demo/dupe-0000-4000-8000-00000000{sfx}.jsonl"));
        fs::write(
            &f,
            format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-06-12T01:00:00Z\",\"uuid\":\"u{sfx}\",\
                 \"cwd\":\"/tmp/demo\",\"message\":{{\"role\":\"user\",\"content\":\"dup note {sfx}\"}}}}\n"
            ),
        )
        .unwrap();
        run(&env, &["ingest", f.to_str().unwrap()], None);
    }
    // The shared prefix matches both → ambiguous → nothing on stdout (it errors to stderr).
    let ambiguous = run(&env, &["session", "dupe"], None);
    assert!(
        ambiguous.is_empty(),
        "an ambiguous prefix must not dump a session, got:\n{ambiguous}"
    );
    // The full id still resolves and dumps.
    let full = run(&env, &["session", "dupe-0000-4000-8000-000000001111"], None);
    assert!(full.contains("dup note 1111"), "full id dumps its turns");
}

#[test]
fn fact_link_matches_golden() {
    let env = setup("factlink");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();

    // alpha is the anchor: out to gamma (resolves), missing (dangling), itself
    // (self), and a fenced link that must be ignored.
    fs::write(
        memdir.join("project_alpha.md"),
        "---\nname: alpha\ndescription: Anchor fact\ntype: project\n---\n\
         Alpha links to [[gamma]] and a [[missing]] one, and to [[alpha]] itself.\n\
         A fenced example that must be ignored:\n\
         ```\n[[fenced-link]]\n```\n",
    )
    .unwrap();
    // beta and gamma both link back to alpha (inbound).
    fs::write(
        memdir.join("reference_beta.md"),
        "---\nname: beta-thing\ndescription: Beta\ntype: reference\n---\n\
         Beta points at [[alpha]].\n",
    )
    .unwrap();
    fs::write(
        memdir.join("project_gamma.md"),
        "---\nname: gamma\ndescription: Gamma\ntype: project\n---\n\
         Gamma points back at [[alpha]] too.\n",
    )
    .unwrap();

    let md = memdir.to_str().unwrap();
    for (leaf, hook) in [
        ("project_alpha.md", "the anchor fact"),
        ("reference_beta.md", "beta points at alpha"),
        ("project_gamma.md", "gamma both ways"),
    ] {
        run(
            &env,
            &[
                "fact", "upsert", "--leaf", leaf, "--memdir", md, "--hook", hook,
            ],
            None,
        );
    }

    let out = run(&env, &["fact", "link", "alpha", "--memdir", md], None);
    assert_eq!(out, golden("fact_link.golden"));
}
