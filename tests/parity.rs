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

// Like run(), but keeps the full Output so a test can assert the exit code.
fn run_full(env: &TestEnv, args: &[&str], stdin: Option<&str>) -> std::process::Output {
    let mut child = Command::new(bin())
        .args(args)
        .env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
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
    child.wait_with_output().unwrap()
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

#[test]
fn fact_search_matches_content_and_respects_status() {
    let env = setup("factsearch");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(
        memdir.join("reference_pg.md"),
        "---\nname: pgbouncer-pool\ndescription: pgbouncer max_client_conn tuning for cache-prod\n\
         type: reference\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        memdir.join("reference_dns.md"),
        "---\nname: dns-ttl\ndescription: route53 ttl defaults for the api zone\n\
         type: reference\n---\nbody\n",
    )
    .unwrap();
    let md = memdir.to_str().unwrap();
    run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "reference_pg.md",
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
            "reference_dns.md",
            "--memdir",
            md,
        ],
        None,
    );

    // A content word returns its fact and leaves the other one out.
    let hit = run(&env, &["fact", "search", "pgbouncer", "--memdir", md], None);
    assert!(
        hit.contains("pgbouncer-pool"),
        "content match returned, got:\n{hit}"
    );
    assert!(
        !hit.contains("dns-ttl"),
        "the non-matching fact stays out, got:\n{hit}"
    );

    // Porter stemming: the hook says "defaults", a search for "default" still hits.
    let stem = run(&env, &["fact", "search", "default", "--memdir", md], None);
    assert!(
        stem.contains("dns-ttl"),
        "stemmed match returned, got:\n{stem}"
    );

    // Archived facts drop out of the default (active) search; --status archived finds them.
    run(
        &env,
        &[
            "fact",
            "archive",
            "--leaf",
            "reference_pg.md",
            "--memdir",
            md,
        ],
        None,
    );
    let active = run(&env, &["fact", "search", "pgbouncer", "--memdir", md], None);
    assert!(
        !active.contains("pgbouncer-pool"),
        "archived fact excluded by default, got:\n{active}"
    );
    let arch = run(
        &env,
        &[
            "fact",
            "search",
            "pgbouncer",
            "--memdir",
            md,
            "--status",
            "archived",
        ],
        None,
    );
    assert!(
        arch.contains("pgbouncer-pool"),
        "archived fact found with --status archived, got:\n{arch}"
    );
}

#[test]
fn sessions_matches_golden() {
    let env = setup("sessions");
    ingest_golden_transcript(&env);
    let out = run(&env, &["sessions"], None);
    let want =
        golden("sessions.golden").replace("{{PROJECTS_DIR}}", env.projects.to_str().unwrap());
    assert_eq!(out, want);
}

#[test]
fn session_tags_line_is_opt_in() {
    let env = setup("sesstags");
    ingest_golden_transcript(&env);
    // --tags adds a `# tags:` header line. A contains-assert (not a golden) so the
    // exact derived set can evolve without touching the pinned session_dump.golden.
    let with = run(&env, &["session", "aaaa-bbbb-1111", "--tags"], None);
    assert!(
        with.contains("# tags: tool:bash"),
        "tags line present, got:\n{with}"
    );
    // The default dump never carries it (keeps session_dump.golden byte-identical).
    let without = run(&env, &["session", "aaaa-bbbb-1111"], None);
    assert!(!without.contains("# tags:"), "default dump shows no tags");
}

#[test]
fn search_tag_filter_composes() {
    let env = setup("searchtag");
    ingest_golden_transcript(&env);
    // The rollout turn lives in a tool:bash session → the tag-scoped search keeps it.
    let hit = run(&env, &["search", "rollout", "--tag", "tool:bash"], None);
    assert!(
        hit.contains("\u{ab}rollout\u{bb}"),
        "tag-scoped search returns the hit, got:\n{hit}"
    );
    // A tag the session lacks filters the hit out entirely.
    let miss = run(
        &env,
        &["search", "rollout", "--tag", "topic:nonexistent"],
        None,
    );
    assert!(
        miss.contains("no matches"),
        "absent tag filters the hit out, got:\n{miss}"
    );
}

#[test]
fn date_filters_inclusive_and_validated() {
    let env = setup("dates");
    ingest_golden_transcript(&env);
    // --before D is inclusive of all of D (next-day `<` boundary). The fixture turns
    // are at 2026-06-12T01:0x:00Z; a naive `ts <= '2026-06-12'` would wrongly drop them.
    let incl = run(&env, &["sessions", "--before", "2026-06-12"], None);
    assert!(
        incl.contains("aaaa-bbb"),
        "the 06-12 session is included by --before 2026-06-12, got:\n{incl}"
    );
    let incl_s = run(
        &env,
        &["search", "cache-prod", "--before", "2026-06-12"],
        None,
    );
    assert!(
        incl_s.contains("\u{ab}cache-prod\u{bb}"),
        "search --before is inclusive of day D, got:\n{incl_s}"
    );
    // A window entirely after the archive is empty.
    let empty = run(&env, &["sessions", "--after", "2030-01-01"], None);
    assert!(
        empty.contains("no sessions match"),
        "a future --after is empty, got:\n{empty}"
    );
    // A bad date is rejected with exit 2 (mirrors the empty-terms guard).
    let bad = run_full(&env, &["search", "rollout", "--after", "2026-13-99"], None);
    assert_eq!(bad.status.code(), Some(2), "a bad --after date exits 2");
}

#[test]
fn search_line_carries_relative_age() {
    let env = setup("searchage");
    ingest_golden_transcript(&env);
    // Clock pinned to 2027-01-12; the fixture turns are 2026-06-12 → 7 months old.
    // Same suffix recall renders, so the two surfaces stay consistent.
    let hit = run(&env, &["search", "rollout"], None);
    assert!(
        hit.contains("(7mo old)"),
        "search line carries the relative age after the timestamp, got:\n{hit}"
    );
}

#[test]
fn search_context_window_shows_surrounding_turns() {
    let env = setup("searchctx");
    // A self-contained 5-turn session with distinctive tokens so the ±N window is
    // unambiguous — plain user strings + assistant text blocks, the shapes ingest flattens.
    let sid = "ctx0-1111-2222-3333";
    let tp = env.projects.join(format!("-tmp-demo/{sid}.jsonl"));
    let turns = [
        ("user", "alpha aardvark the opening note"),
        ("assistant", "beta bumblebee acknowledging"),
        ("user", "gamma wombatbridge the matched middle"),
        ("assistant", "delta dragonfly the considered reply"),
        ("user", "epsilon elephant the closing note"),
    ];
    let mut body = String::new();
    for (i, (role, text)) in turns.iter().enumerate() {
        let content = if *role == "assistant" {
            format!("[{{\"type\":\"text\",\"text\":\"{text}\"}}]")
        } else {
            format!("\"{text}\"")
        };
        body.push_str(&format!(
            "{{\"type\":\"{role}\",\"timestamp\":\"2026-06-12T01:0{i}:00Z\",\"uuid\":\"x{i}\",\
             \"cwd\":\"/tmp/demo\",\"message\":{{\"role\":\"{role}\",\"content\":{content}}}}}\n"
        ));
    }
    fs::write(&tp, body).unwrap();
    run(&env, &["ingest", tp.to_str().unwrap()], None);

    // Default (no flag): the snippet only — no neighbouring turns leak into the output.
    let plain = run(&env, &["search", "wombatbridge"], None);
    assert!(
        plain.contains("\u{ab}wombatbridge\u{bb}"),
        "default hit present, got:\n{plain}"
    );
    assert!(
        !plain.contains("bumblebee") && !plain.contains("dragonfly"),
        "default search shows no context turns, got:\n{plain}"
    );

    // --context 1: both immediate neighbours show; the distance-2 turns stay out.
    let ctx = run(&env, &["search", "wombatbridge", "--context", "1"], None);
    assert!(
        ctx.contains("bumblebee") && ctx.contains("dragonfly"),
        "both immediate neighbours are shown, got:\n{ctx}"
    );
    assert!(
        !ctx.contains("aardvark") && !ctx.contains("elephant"),
        "turns outside the ±1 window stay out, got:\n{ctx}"
    );

    // First-turn hit: nothing before it; the next turn still shows (and the -C alias works).
    let head = run(&env, &["search", "aardvark", "-C", "1"], None);
    assert!(
        head.contains("bumblebee"),
        "after-context shows for a first-turn hit, got:\n{head}"
    );
}

#[test]
fn search_exclude_drops_hits_carrying_the_term() {
    let env = setup("searchexcl");
    let sid = "excl-1111-2222-3333";
    let tp = env.projects.join(format!("-tmp-demo/{sid}.jsonl"));
    // Turn 1 carries the search term only; turn 2 carries the term AND the excluded
    // word. --exclude works per turn (each hit is a turn), so only turn 2 should drop.
    fs::write(
        &tp,
        "{\"type\":\"user\",\"timestamp\":\"2026-06-12T01:00:00Z\",\"uuid\":\"e1\",\
         \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"user\",\"content\":\
         \"deploy the wibble service\"}}\n\
         {\"type\":\"assistant\",\"timestamp\":\"2026-06-12T01:01:00Z\",\"uuid\":\"e2\",\
         \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\
         \"text\":\"the deploy triggered a rollback of snarf\"}]}}\n",
    )
    .unwrap();
    run(&env, &["ingest", tp.to_str().unwrap()], None);

    // Without --exclude both deploy turns match (the second carries the snarf token).
    let plain = run(&env, &["search", "deploy"], None);
    assert!(
        plain.contains("snarf"),
        "the rollback turn is present without --exclude, got:\n{plain}"
    );

    // --exclude rollback drops the turn that contains rollback, keeps the other.
    let excl = run(&env, &["search", "deploy", "--exclude", "rollback"], None);
    assert!(
        excl.contains("wibble"),
        "the non-excluded turn stays, got:\n{excl}"
    );
    assert!(
        !excl.contains("snarf") && !excl.contains("rollback"),
        "the turn carrying the excluded term is dropped, got:\n{excl}"
    );
}

#[test]
fn search_any_ors_terms_and_composes_with_exclude() {
    let env = setup("searchany");
    let sid = "any0-1111-2222-3333";
    let tp = env.projects.join(format!("-tmp-demo/{sid}.jsonl"));
    // turn 3 carries the first OR-term (alpha) AND the excluded token, so it tests
    // that `(alpha OR beta) NOT quuxroll` groups right — a mis-grouping would keep it.
    fs::write(
        &tp,
        "{\"type\":\"user\",\"timestamp\":\"2026-06-12T01:00:00Z\",\"uuid\":\"n1\",\
         \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"user\",\"content\":\"alpha apple\"}}\n\
         {\"type\":\"assistant\",\"timestamp\":\"2026-06-12T01:01:00Z\",\"uuid\":\"n2\",\
         \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\
         \"text\":\"beta banana\"}]}}\n\
         {\"type\":\"user\",\"timestamp\":\"2026-06-12T01:02:00Z\",\"uuid\":\"n3\",\
         \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"user\",\"content\":\"alpha cherry quuxroll\"}}\n",
    )
    .unwrap();
    run(&env, &["ingest", tp.to_str().unwrap()], None);

    // Default AND: no single turn carries both alpha and beta.
    let and = run(&env, &["search", "alpha", "beta"], None);
    assert!(
        and.contains("no matches"),
        "default AND finds nothing, got:\n{and}"
    );

    // --any ORs them: the alpha turns and the beta turn all match.
    let any = run(&env, &["search", "alpha", "beta", "--any"], None);
    assert!(
        any.contains("apple") && any.contains("banana") && any.contains("cherry"),
        "--any matches any term, got:\n{any}"
    );

    // --any composed with --exclude drops the alpha turn that also carries the token.
    let composed = run(
        &env,
        &["search", "alpha", "beta", "--any", "--exclude", "quuxroll"],
        None,
    );
    assert!(
        composed.contains("apple") && composed.contains("banana"),
        "the clean turns stay, got:\n{composed}"
    );
    assert!(
        !composed.contains("cherry"),
        "the alpha turn carrying the excluded token is dropped, got:\n{composed}"
    );
}

#[test]
fn hook_stop_ingests_in_progress_session_incrementally() {
    let env = setup("hookstop");
    // An in-progress transcript: on disk but never run through `ingest`. Only the
    // Stop hook archives it, the way it happens mid-session in real use.
    let sid = "live-7777-8888-9999";
    let tp = env.projects.join(format!("-tmp-demo/{sid}.jsonl"));
    fs::write(
        &tp,
        "{\"type\":\"user\",\"timestamp\":\"2026-06-17T05:00:00Z\",\"uuid\":\"l1\",\
         \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"user\",\"content\":\
         \"chase the wombatprobe regression\"}}\n",
    )
    .unwrap();
    let payload = format!(
        "{{\"transcript_path\":\"{}\",\"session_id\":\"{sid}\",\"cwd\":\"/tmp/demo\",\
         \"hook_event_name\":\"Stop\"}}",
        tp.to_str().unwrap()
    );

    // First Stop: the live session is searchable before it ever ends, and exit is 0.
    let first = run_full(&env, &["hook", "stop"], Some(&payload));
    assert_eq!(first.status.code(), Some(0), "the Stop hook always exits 0");
    let hit = run(&env, &["search", "wombatprobe"], None);
    assert!(
        hit.contains("\u{ab}wombatprobe\u{bb}"),
        "the in-progress session is searchable after Stop, got:\n{hit}"
    );

    // Append a turn and fire Stop again: the new turn lands, the old one isn't re-inserted.
    let mut f = fs::OpenOptions::new().append(true).open(&tp).unwrap();
    f.write_all(
        b"{\"type\":\"assistant\",\"timestamp\":\"2026-06-17T05:01:00Z\",\"uuid\":\"l2\",\
          \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\
          \"text\",\"text\":\"the carrotlatch held after the patch\"}]}}\n",
    )
    .unwrap();
    run(&env, &["hook", "stop"], Some(&payload));
    let hit2 = run(&env, &["search", "carrotlatch"], None);
    assert!(
        hit2.contains("\u{ab}carrotlatch\u{bb}"),
        "the appended turn is searchable after the next Stop, got:\n{hit2}"
    );
    // Idempotent re-read: the first turn appears exactly once, not duplicated.
    let again = run(&env, &["search", "wombatprobe", "-n", "10"], None);
    assert_eq!(
        again.matches("\u{ab}wombatprobe\u{bb}").count(),
        1,
        "the first turn is ingested once across two Stop passes, got:\n{again}"
    );

    // Stop must NOT queue the live session for checkpoint — that's SessionEnd's job.
    let pending = run(&env, &["pending"], None);
    assert!(
        !pending.contains(sid),
        "Stop leaves the in-progress session out of the checkpoint queue, got:\n{pending}"
    );

    // A payload with no transcript_path is a clean no-op, still exit 0.
    let noop = run_full(&env, &["hook", "stop"], Some("{\"session_id\":\"x\"}"));
    assert_eq!(
        noop.status.code(),
        Some(0),
        "a missing transcript_path no-ops at exit 0"
    );
}
