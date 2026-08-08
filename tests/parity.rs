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

/// A child pointed at the throwaway dirs with a pinned clock, and EVERY
/// inherited SUBROSA_* dropped first — SUBROSA_DB in a developer's shell
/// outranks SUBROSA_DIR and would aim the suite at a real database.
fn base_cmd(env: &TestEnv) -> Command {
    let mut cmd = Command::new(bin());
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("SUBROSA_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
        // Pin the clock (2027-01-12Z) so recall's age hints render deterministically.
        .env("SUBROSA_NOW", "1799712000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn run(env: &TestEnv, args: &[&str], stdin: Option<&str>) -> String {
    let mut child = base_cmd(env).args(args).spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.unwrap_or("").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// Like run(), but with an explicit working directory (checkpoint-mark scopes
// its live-session lookup to the cwd's project).
fn run_in(env: &TestEnv, cwd: &Path, args: &[&str]) -> String {
    let mut child = base_cmd(env).args(args).current_dir(cwd).spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(b"").unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// Like run(), but keeps the full Output so a test can assert the exit code.
fn run_full(env: &TestEnv, args: &[&str], stdin: Option<&str>) -> std::process::Output {
    let mut child = base_cmd(env).args(args).spawn().unwrap();
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

// checkpoint-mark must stamp the cwd project's own session even when another
// project holds a more recently modified transcript (the pre-fix failure).
#[test]
fn checkpoint_mark_scopes_to_cwd_project() {
    let env = setup("mark-cwd");
    // A working dir whose encoded name is a project dir with "our" session.
    let cwd = env.data.parent().unwrap().join("work");
    fs::create_dir_all(&cwd).unwrap();
    // getcwd() hands the binary the physical path (macOS tempdir rides the
    // /var -> /private/var symlink), and Claude Code names project dirs after
    // the resolved path too — so encode the canonicalized form.
    let cwd = cwd.canonicalize().unwrap();
    let encoded: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let ours_dir = env.projects.join(&encoded);
    fs::create_dir_all(&ours_dir).unwrap();
    let ours = ours_dir.join("aaaa-cwd-1111.jsonl");
    fs::write(&ours, golden("transcript.jsonl")).unwrap();
    run(&env, &["ingest", ours.to_str().unwrap()], None);
    // A second project whose transcript is strictly newer on disk.
    let other_dir = env.projects.join("-tmp-other");
    fs::create_dir_all(&other_dir).unwrap();
    let other = other_dir.join("bbbb-other-2222.jsonl");
    std::thread::sleep(std::time::Duration::from_millis(25));
    fs::write(&other, golden("transcript.jsonl")).unwrap();
    run(&env, &["ingest", other.to_str().unwrap()], None);

    let out = run_in(&env, &cwd, &["checkpoint-mark"]);
    assert!(
        out.contains("marked current session aaaa-cwd-1111"),
        "mark must stay in the cwd project:\n{out}"
    );
}

// An explicit id/prefix pins the mark regardless of cwd or mtimes.
#[test]
fn checkpoint_mark_explicit_id_wins() {
    let env = setup("mark-explicit");
    let cwd = env.data.parent().unwrap().join("work");
    fs::create_dir_all(&cwd).unwrap();
    let other_dir = env.projects.join("-tmp-other");
    fs::create_dir_all(&other_dir).unwrap();
    let other = other_dir.join("bbbb-other-2222.jsonl");
    fs::write(&other, golden("transcript.jsonl")).unwrap();
    run(&env, &["ingest", other.to_str().unwrap()], None);

    let out = run_in(&env, &cwd, &["checkpoint-mark", "bbbb-oth"]);
    assert!(
        out.contains("marked current session bbbb-other-2222"),
        "explicit prefix must win:\n{out}"
    );
}

#[test]
fn search_fuzzy_typo_finds_nearest_match() {
    let env = setup("fuzzy-typo");
    ingest_golden_transcript(&env);
    // "latecny" is "latency" (present in the transcript) with two adjacent chars
    // swapped: the substring pass finds nothing, the trigram fallback must.
    let out = run(&env, &["search", "--fuzzy", "latecny"], None);
    assert!(
        out.contains("nearest matches (within one edit)"),
        "missing fallback header:\n{out}"
    );
    // The snippet keeps its «» highlight markers, so match on the session id
    // and the footer instead of the literal word.
    assert!(
        out.contains("aaaa-bbb"),
        "expected the golden session:\n{out}"
    );
    assert!(out.contains("1 result(s)"), "expected one hit:\n{out}");
}

#[test]
fn search_fuzzy_multi_term_and_rescues_one_typo() {
    let env = setup("fuzzy-multi");
    ingest_golden_transcript(&env);
    // One typo'd term + one exact term (same turn): the per-term relaxation must
    // keep "rollout" as a hard phrase and still rescue "latecny" → latency.
    let out = run(&env, &["search", "--fuzzy", "latecny", "rollout"], None);
    assert!(
        out.contains("nearest matches (within one edit)"),
        "missing fallback header:\n{out}"
    );
    assert!(out.contains("1 result(s)"), "expected one hit:\n{out}");
}

#[test]
fn search_fuzzy_true_miss_output_unchanged() {
    let env = setup("fuzzy-miss");
    ingest_golden_transcript(&env);
    // Nothing within one edit of this exists; the pre-fallback output is pinned.
    let out = run(&env, &["search", "--fuzzy", "qqqqqqq"], None);
    assert!(
        out.contains("[subrosa] no matches for: \"qqqqqqq\""),
        "true miss must keep the no-match line:\n{out}"
    );
    assert!(
        !out.contains("nearest matches"),
        "no fallback header on a true miss:\n{out}"
    );
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
fn fact_doctor_matches_golden() {
    let env = setup("factdoctor");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    let md = memdir.to_str().unwrap();

    // One registered leaf per finding kind. The corrupted one is the shape that
    // actually happened: a frontmatter tail spliced in below the body, no leading
    // `---`, so the leaf still looks fine to a body-start check.
    let registered = [
        (
            "feedback_spliced.md",
            "---\nname: spliced-rule\ndescription: A live rule that quietly stopped loading\n\
             type: feedback\n---\nAlways check the thing before the other thing.\n\n\
             metadata:\n  type: feedback\noriginSessionId: 0f0f\n---\n",
        ),
        // Prose that merely starts with `---` is no closing delimiter: the loose
        // `\n---` scan used to read this whole leaf as closed and clean.
        (
            "feedback_unclosed.md",
            "---\nname: unclosed-rule\ndescription: The block never closes\ntype: feedback\n\
             --- banner text, not a closer\nThe rule body starts here.\n",
        ),
        (
            "project_missing_field.md",
            "---\nname: no-description\ntype: project\n---\nBody.\n",
        ),
        ("project_nofrontmatter.md", "Just a body, no frontmatter.\n"),
        (
            "reference_badtype.md",
            "---\nname: bad-type\ndescription: Type is not one of the four\ntype: guardrail\n\
             ---\nBody.\n",
        ),
        (
            "reference_dupe_a.md",
            "---\nname: shared-slug\ndescription: First claim on the slug\ntype: reference\n\
             ---\nBody.\n",
        ),
        (
            "reference_dupe_b.md",
            "---\nname: shared-slug\ndescription: Second claim on the same slug\n\
             type: reference\n---\nBody.\n",
        ),
        (
            "reference_links.md",
            "---\nname: link-holder\ndescription: Links out to one live and one dead slug\n\
             type: reference\n---\nPoints at [[shared-slug]] and at [[nope-missing]].\n",
        ),
    ];
    for (leaf, text) in registered {
        fs::write(memdir.join(leaf), text).unwrap();
        run(
            &env,
            &["fact", "upsert", "--leaf", leaf, "--memdir", md],
            None,
        );
    }

    // Unregistered: one well-formed (a plain orphan) and one with no subrosa
    // frontmatter at all — the shape Claude Code's own auto-memory leaves take.
    fs::write(
        memdir.join("reference_orphan.md"),
        "---\nname: never-registered\ndescription: Well-formed but no fact row\n\
         type: reference\n---\nBody.\n",
    )
    .unwrap();
    fs::write(
        memdir.join("foreign_cc_leaf.md"),
        "Notes Claude Code wrote on its own.\n",
    )
    .unwrap();

    // Name drift: registered under the old name, then the leaf renamed in place.
    let drift = |name: &str| {
        format!("---\nname: {name}\ndescription: Renamed after registering\ntype: reference\n---\nBody.\n")
    };
    fs::write(memdir.join("reference_drift.md"), drift("original-name")).unwrap();
    run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "reference_drift.md",
            "--memdir",
            md,
        ],
        None,
    );
    fs::write(memdir.join("reference_drift.md"), drift("renamed-thing")).unwrap();

    // Two active rows left holding one stored slug after both leaves were renamed
    // apart. The leaves no longer collide, but the rows still shadow each other in
    // the link map — frontmatter alone can't see this.
    for (leaf, renamed) in [
        ("reference_dbdupe_a.md", "dbdupe-a-now"),
        ("reference_dbdupe_b.md", "dbdupe-b-now"),
    ] {
        fs::write(
            memdir.join(leaf),
            "---\nname: db-shared-slug\ndescription: Registered under a shared slug\n\
             type: reference\n---\nBody.\n",
        )
        .unwrap();
        run(
            &env,
            &["fact", "upsert", "--leaf", leaf, "--memdir", md],
            None,
        );
        fs::write(
            memdir.join(leaf),
            format!(
                "---\nname: {renamed}\ndescription: Registered under a shared slug\n\
                 type: reference\n---\nBody.\n"
            ),
        )
        .unwrap();
    }

    // Two facts whose leaf file isn't there: one still active, one archived.
    for leaf in ["reference_gone.md", "reference_archived_gone.md"] {
        run(
            &env,
            &["fact", "upsert", "--leaf", leaf, "--memdir", md],
            None,
        );
    }
    run(
        &env,
        &[
            "fact",
            "archive",
            "--leaf",
            "reference_archived_gone.md",
            "--memdir",
            md,
        ],
        None,
    );

    let out = run_full(&env, &["fact", "doctor", "--memdir", md], None);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        golden("fact_doctor.golden")
    );
    assert_eq!(out.status.code(), Some(1), "any error exits 1");
}

#[test]
fn fact_doctor_clean_then_warning_only_stay_exit_zero() {
    let env = setup("factdoctorok");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    let md = memdir.to_str().unwrap();
    fs::write(
        memdir.join("reference_clean.md"),
        "---\nname: clean-fact\ndescription: Nothing wrong with this one\ntype: reference\n\
         ---\nBody.\n",
    )
    .unwrap();
    run(
        &env,
        &[
            "fact",
            "upsert",
            "--leaf",
            "reference_clean.md",
            "--memdir",
            md,
        ],
        None,
    );

    let clean = run_full(&env, &["fact", "doctor", "--memdir", md], None);
    assert_eq!(
        String::from_utf8_lossy(&clean.stdout),
        "[subrosa] doctor: 1 leaf(s), 1 fact(s) \u{2014} clean\n"
    );
    assert_eq!(clean.status.code(), Some(0), "a clean memdir exits 0");

    // An unregistered leaf is bookkeeping, not corruption: warn, and still exit 0.
    fs::write(
        memdir.join("reference_extra.md"),
        "---\nname: extra-fact\ndescription: On disk but never registered\ntype: reference\n\
         ---\nBody.\n",
    )
    .unwrap();
    let warned = run_full(&env, &["fact", "doctor", "--memdir", md], None);
    let text = String::from_utf8_lossy(&warned.stdout);
    assert!(
        text.contains("warn  reference_extra.md: not registered"),
        "the orphan leaf is named, got:\n{text}"
    );
    assert!(
        text.contains("[subrosa] doctor: 0 error(s), 1 warning(s)"),
        "the summary counts one warning, got:\n{text}"
    );
    assert_eq!(
        warned.status.code(),
        Some(0),
        "warnings alone stay exit 0, got:\n{text}"
    );
}

// A memdir it could not read must never print "clean" — false reassurance is the
// worst outcome for an integrity check, so an unverifiable path exits 1.
#[test]
fn fact_doctor_missing_memdir_is_not_clean() {
    let env = setup("factdoctorpath");
    let missing = env.data.join("no-such-memdir");
    let out = run_full(
        &env,
        &["fact", "doctor", "--memdir", missing.to_str().unwrap()],
        None,
    );
    assert_eq!(out.status.code(), Some(1), "an unverifiable path exits 1");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not a directory") && err.contains("no-such-memdir"),
        "the path is named on stderr, got:\n{err}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("clean"),
        "nothing is called clean"
    );
}

#[cfg(unix)]
#[test]
fn fact_doctor_unreadable_memdir_is_not_clean() {
    use std::os::unix::fs::PermissionsExt;
    let env = setup("factdoctorperm");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::set_permissions(&memdir, fs::Permissions::from_mode(0o000)).unwrap();
    let out = run_full(
        &env,
        &["fact", "doctor", "--memdir", memdir.to_str().unwrap()],
        None,
    );
    fs::set_permissions(&memdir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1), "an unreadable memdir exits 1");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot read"),
        "the read failure is named on stderr, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("clean"),
        "an unreadable folder is never reported clean"
    );
}

// An archive that exists but won't read is an integrity failure, not the same
// thing as having no archive yet.
#[test]
fn fact_doctor_unreadable_db_is_not_clean() {
    let env = setup("factdoctordb");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(
        memdir.join("reference_clean.md"),
        "---\nname: clean-fact\ndescription: Nothing wrong with this one\ntype: reference\n\
         ---\nBody.\n",
    )
    .unwrap();
    fs::write(env.data.join("memory.db"), b"this is not a database").unwrap();
    let out = run_full(
        &env,
        &["fact", "doctor", "--memdir", memdir.to_str().unwrap()],
        None,
    );
    assert_eq!(out.status.code(), Some(1), "a broken archive exits 1");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot read facts db"),
        "the db failure is named on stderr, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("clean") && !text.contains("no facts db"),
        "a corrupt db is neither clean nor 'no db yet', got:\n{text}"
    );
}

// A dangling db symlink is broken, not absent: Path::exists() follows the link and
// answers false, which used to downgrade this to a clean leaf-only run.
#[cfg(unix)]
#[test]
fn fact_doctor_dangling_db_symlink_is_not_clean() {
    let env = setup("factdoctorlink");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(
        memdir.join("reference_clean.md"),
        "---\nname: clean-fact\ndescription: Nothing wrong with this one\ntype: reference\n\
         ---\nBody.\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(env.data.join("gone.db"), env.data.join("memory.db")).unwrap();
    let out = run_full(
        &env,
        &["fact", "doctor", "--memdir", memdir.to_str().unwrap()],
        None,
    );
    assert_eq!(out.status.code(), Some(1), "a broken db link exits 1");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot read facts db"),
        "the db failure is named on stderr, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("clean") && !text.contains("no facts db"),
        "a dangling link is neither clean nor 'no db yet', got:\n{text}"
    );
}

// The active pair must break whichever leaf sorts first: an archived claimant
// landing at the top of the sort used to soften both live ones to warnings.
#[test]
fn fact_doctor_active_pair_errors_behind_an_archived_claimant() {
    let env = setup("factdoctorslug");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    let md = memdir.to_str().unwrap();
    // a_ sorts first and gets archived; b_ and c_ stay active on the same slug.
    for name in [
        "reference_a_old.md",
        "reference_b_live.md",
        "reference_c_live.md",
    ] {
        fs::write(
            memdir.join(name),
            "---\nname: contested-slug\ndescription: Claims the shared slug\n\
             type: reference\n---\nBody.\n",
        )
        .unwrap();
        run(
            &env,
            &["fact", "upsert", "--leaf", name, "--memdir", md],
            None,
        );
    }
    run(
        &env,
        &[
            "fact",
            "archive",
            "--leaf",
            "reference_a_old.md",
            "--memdir",
            md,
        ],
        None,
    );

    let out = run_full(&env, &["fact", "doctor", "--memdir", md], None);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(
            "error reference_c_live.md: duplicate name `contested-slug` — \
             reference_b_live.md claims it too"
        ),
        "the two live facts error against each other, got:\n{text}"
    );
    assert!(
        text.contains("warn  reference_b_live.md: duplicate name"),
        "the clash with the archived leaf stays a warning, got:\n{text}"
    );
    assert_eq!(out.status.code(), Some(1), "an active pair exits 1");
}

#[test]
fn fact_doctor_without_a_db_lints_leaves_only() {
    let env = setup("factdoctornodb");
    let memdir = env.data.join("memdir");
    fs::create_dir_all(&memdir).unwrap();
    fs::write(
        memdir.join("reference_lonely.md"),
        "A plain note with no frontmatter.\n",
    )
    .unwrap();
    // Nothing has opened the DB in this env, so the row-dependent checks are off:
    // the same problem warns instead of erroring, and no leaf is called an orphan.
    let out = run_full(
        &env,
        &["fact", "doctor", "--memdir", memdir.to_str().unwrap()],
        None,
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("[subrosa] doctor: no facts db \u{2014} leaf checks only"),
        "the note line leads, got:\n{text}"
    );
    assert!(
        text.contains("warn  reference_lonely.md: no frontmatter"),
        "the leaf check still runs, at warn level, got:\n{text}"
    );
    assert!(
        !text.contains("not registered"),
        "the orphan check stays off without rows, got:\n{text}"
    );
    assert_eq!(out.status.code(), Some(0), "leaf-only mode never exits 1");
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

#[test]
fn incremental_ingest_matches_full_ingest() {
    // The whole point of the resume cursor: many Stop passes over a growing
    // transcript must land the exact same archive as one full ingest of the final
    // file — same turns, same first_ts/last_ts span, same derived tags.
    let env = setup("incr-equiv");
    let lines = [
        r#"{"type":"user","timestamp":"2026-06-17T05:00:00Z","uuid":"u1","cwd":"/tmp/demo","message":{"role":"user","content":"chase the wombatprobe regression in widgetcache"}}"#,
        r#"{"type":"assistant","timestamp":"2026-06-17T05:01:00Z","uuid":"a1","cwd":"/tmp/demo","message":{"role":"assistant","content":[{"type":"text","text":"the carrotlatch held after the patch on auth.ts"}]}}"#,
        r#"{"type":"user","timestamp":"2026-06-17T05:02:00Z","uuid":"u2","cwd":"/tmp/demo","message":{"role":"user","content":"now wire the fluxcapacitor into the gizmo"}}"#,
        r#"{"type":"assistant","timestamp":"2026-06-17T05:03:00Z","uuid":"a2","cwd":"/tmp/demo","message":{"role":"assistant","content":[{"type":"text","text":"inverted the register and the resonance is stable"}]}}"#,
    ];

    // full: write the whole file, ingest in one pass.
    let full_sid = "full0000-0000-0000-0000-000000000000";
    let full_tp = env.projects.join(format!("-tmp-demo/{full_sid}.jsonl"));
    fs::write(&full_tp, format!("{}\n", lines.join("\n"))).unwrap();
    run(&env, &["ingest", full_tp.to_str().unwrap()], None);

    // incr: grow the file across Stop passes, with a half-written line mid-stream.
    let incr_sid = "incr0000-0000-0000-0000-000000000000";
    let incr_tp = env.projects.join(format!("-tmp-demo/{incr_sid}.jsonl"));
    let payload = format!(
        "{{\"transcript_path\":\"{}\",\"session_id\":\"{incr_sid}\"}}",
        incr_tp.to_str().unwrap()
    );
    let append = |bytes: &[u8]| {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&incr_tp)
            .unwrap();
        f.write_all(bytes).unwrap();
    };
    append(format!("{}\n", lines[0]).as_bytes());
    run(&env, &["hook", "stop"], Some(&payload));
    append(format!("{}\n", lines[1]).as_bytes());
    run(&env, &["hook", "stop"], Some(&payload));
    // A half-written turn 3 (no trailing newline): a clean no-op that must not
    // advance the cursor past the incomplete line.
    let l3 = lines[2].as_bytes();
    append(&l3[..30]);
    run(&env, &["hook", "stop"], Some(&payload));
    append(&l3[30..]);
    append(b"\n");
    run(&env, &["hook", "stop"], Some(&payload));
    append(format!("{}\n", lines[3]).as_bytes());
    run(&env, &["hook", "stop"], Some(&payload));

    // Turn bodies (everything but the per-session "# ..." header lines) must match.
    let body = |dump: &str| {
        dump.lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let full_dump = run(&env, &["session", full_sid], None);
    let incr_dump = run(&env, &["session", incr_sid], None);
    assert_eq!(
        body(&incr_dump),
        body(&full_dump),
        "incremental turns must equal a single full ingest\nINCR:\n{incr_dump}\nFULL:\n{full_dump}"
    );

    // The first_ts..last_ts span must match — proving an incremental pass's local
    // min didn't clobber the true session start (the MIN/MAX guard).
    let span = |dump: &str| {
        dump.lines()
            .next()
            .unwrap_or("")
            .rsplit("  ")
            .next()
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(
        span(&incr_dump),
        span(&full_dump),
        "first_ts..last_ts must match the full ingest"
    );
    assert!(
        span(&incr_dump).starts_with("2026-06-17T05:00:00"),
        "first_ts stays the earliest record, got span: {}",
        span(&incr_dump)
    );

    // Tags were derived during the incremental build (the inserted>0 passes ran it).
    let tags = run(&env, &["session", incr_sid, "--tags"], None);
    assert!(
        tags.contains("topic:"),
        "incremental session has derived tags, got:\n{tags}"
    );
}

#[test]
fn incremental_ingest_survives_a_shorter_file() {
    // A transcript that shrinks (truncation / replacement) must not break the Stop
    // hook: the cursor guard re-reads from the top instead of seeking past EOF, and
    // the already-stored turns are never lost.
    let env = setup("incr-trunc");
    let sid = "trunc000-0000-0000-0000-000000000000";
    let tp = env.projects.join(format!("-tmp-demo/{sid}.jsonl"));
    let payload = format!(
        "{{\"transcript_path\":\"{}\",\"session_id\":\"{sid}\"}}",
        tp.to_str().unwrap()
    );
    let long: String = (0..6)
        .map(|i| {
            format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-06-17T05:0{i}:00Z\",\"uuid\":\"u{i}\",\
                 \"cwd\":\"/tmp/demo\",\"message\":{{\"role\":\"user\",\"content\":\
                 \"baseline crocodilethump line {i}\"}}}}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&tp, format!("{long}\n")).unwrap();
    run(&env, &["hook", "stop"], Some(&payload));
    assert!(run(&env, &["search", "crocodilethump"], None).contains("crocodilethump"));

    // Replace with a single, shorter line — the stored cursor now points past EOF.
    fs::write(
        &tp,
        "{\"type\":\"user\",\"timestamp\":\"2026-06-17T06:00:00Z\",\"uuid\":\"n1\",\
         \"cwd\":\"/tmp/demo\",\"message\":{\"role\":\"user\",\"content\":\"snapfizzle reset\"}}\n",
    )
    .unwrap();
    let res = run_full(&env, &["hook", "stop"], Some(&payload));
    assert_eq!(
        res.status.code(),
        Some(0),
        "a shrinking transcript still exits 0"
    );
    assert!(
        run(&env, &["search", "crocodilethump"], None).contains("crocodilethump"),
        "existing turns survive a shorter file"
    );
}
