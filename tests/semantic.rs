//! Opt-in semantic search, end to end against a stub Ollama on loopback — no
//! live model, no real network, anywhere in this suite.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_subrosa")
}

/// Nothing listens on port 1, so a connect there is refused straight away —
/// the "Ollama isn't running" case without racing a real teardown.
const DEAD_HOST: &str = "127.0.0.1:1";

//--- stub Ollama --------------------------------------------------------------

/// Deterministic stand-in for an embedding model: text lands in one of three
/// topic directions by the words it holds. A query and its match need no word
/// in common, which is the whole point of the feature.
fn canned(text: &str) -> [f32; 3] {
    let t = text.to_lowercase();
    if t.contains("failover") || t.contains("promotion") {
        [1.0, 0.0, 0.0]
    } else if t.contains("pod") || t.contains("drained") {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    }
}

/// What the stub answers with. Switchable mid-test so one archive can be built
/// with good vectors and then meet a model that has changed under it.
#[derive(Clone, Copy)]
enum Mode {
    Ok,
    /// The "model was never pulled" reply.
    NoModel,
    /// A row holding a non-numeric value — unusable, must never be stored.
    BadVector,
    /// Two dimensions instead of three, which must never mix with what's stored.
    ShortVector,
}

struct Stub {
    port: u16,
    /// Every string the client asked to embed, in arrival order.
    seen: Arc<Mutex<Vec<String>>>,
    mode: Arc<Mutex<Mode>>,
}

impl Stub {
    fn start(mode: Mode) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mode = Arc::new(Mutex::new(mode));
        let (recorder, answers) = (Arc::clone(&seen), Arc::clone(&mode));
        std::thread::spawn(move || {
            for sock in listener.incoming().flatten() {
                let m = *answers.lock().unwrap();
                serve(sock, m, &recorder);
            }
        });
        Stub { port, seen, mode }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }
}

fn serve(mut sock: TcpStream, mode: Mode, seen: &Arc<Mutex<Vec<String>>>) {
    // The client never half-closes, so the body ends where Content-Length says.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let body_at = loop {
        match sock.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        let Some(sep) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..sep]).into_owned();
        let len: usize = head
            .lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or(0);
        if buf.len() >= sep + 4 + len {
            assert!(
                head.starts_with("POST /api/embed "),
                "unexpected request: {head}"
            );
            break sep + 4;
        }
    };

    let req: Value = serde_json::from_slice(&buf[body_at..]).unwrap();
    let inputs: Vec<String> = req["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    seen.lock().unwrap().extend(inputs.iter().cloned());

    let ok = |rows: Vec<Value>| {
        (
            "200 OK",
            serde_json::json!({ "embeddings": rows }).to_string(),
        )
    };
    let (line, body) = match mode {
        Mode::Ok => ok(inputs
            .iter()
            .map(|t| serde_json::json!(canned(t)))
            .collect()),
        Mode::BadVector => ok(inputs
            .iter()
            .map(|_| serde_json::json!(["oops", 0, 0]))
            .collect()),
        Mode::ShortVector => ok(inputs
            .iter()
            .map(|t| serde_json::json!(canned(t)[..2]))
            .collect()),
        Mode::NoModel => (
            "404 Not Found",
            serde_json::json!({ "error": "model \"nomic-embed-text\" not found" }).to_string(),
        ),
    };
    let _ = sock.write_all(
        format!(
            "HTTP/1.1 {line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
}

//--- test env -----------------------------------------------------------------

struct TestEnv {
    data: PathBuf,
    projects: PathBuf,
}

fn setup(tag: &str) -> TestEnv {
    let root = std::env::temp_dir().join(format!("subrosa-semantic-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let data = root.join("data");
    let projects = root.join("projects");
    fs::create_dir_all(&data).unwrap();
    TestEnv { data, projects }
}

/// A child pointed at the throwaway dirs with a pinned clock, and EVERY
/// inherited SUBROSA_* dropped first — SUBROSA_DB in a developer's shell
/// outranks SUBROSA_DIR and would aim the suite at a real database.
fn run(env: &TestEnv, host: &str, args: &[&str]) -> (String, String, bool) {
    let mut cmd = Command::new(bin());
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("SUBROSA_") {
            cmd.env_remove(&k);
        }
    }
    let out = cmd
        .args(args)
        .env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
        .env("SUBROSA_OLLAMA_HOST", host)
        // Pinned to 2027-01-12Z so the age hints render deterministically.
        .env("SUBROSA_NOW", "1799712000")
        .current_dir(&env.data)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn ingest(env: &TestEnv, project: &str, stem: &str, texts: &[&str]) {
    let dir = env.projects.join(project);
    fs::create_dir_all(&dir).unwrap();
    let records: Vec<String> = texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            format!(
                r#"{{"type":"user","timestamp":"2026-06-12T01:0{i}:00Z","uuid":"u{i}","cwd":"/tmp/demo","message":{{"role":"user","content":"{text}"}}}}"#
            )
        })
        .collect();
    let t = dir.join(format!("{stem}.jsonl"));
    fs::write(&t, records.join("\n") + "\n").unwrap();
    run(env, DEAD_HOST, &["ingest", t.to_str().unwrap()]);
}

/// Three turns on three different topics, so ranking has something to separate.
fn ingest_demo(env: &TestEnv) {
    ingest(
        env,
        "-tmp-demo",
        "aaaa-bbbb-2222",
        &[
            "the database failover took ten minutes to settle",
            "the pod restarted after the node was drained",
            "we rewrote the invoice totals report",
        ],
    );
}

//--- tests --------------------------------------------------------------------

/// The headline claim: a query that shares no word with the turn still ranks it
/// first, and the result renders in the same format keyword search uses.
#[test]
fn semantic_search_ranks_by_meaning_not_shared_words() {
    let env = setup("happy");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);

    let (out, err, ok) = run(&env, &stub.host(), &["embed"]);
    assert!(ok, "embed failed: {err}");
    assert_eq!(
        out, "[subrosa] embed: 3 turn(s) embedded with nomic-embed-text\n",
        "stderr was:\n{err}"
    );
    assert!(err.contains("[subrosa] embedded 3/3"), "no progress: {err}");
    // Backfilled text carries nomic's document-side task prefix.
    assert!(
        stub.seen()
            .iter()
            .all(|s| s.starts_with("search_document: ")),
        "stored text lost its prefix: {:?}",
        stub.seen()
    );

    // "storage cluster promotion" shares no word with the failover turn.
    let (out, err, ok) = run(
        &env,
        &stub.host(),
        &[
            "search",
            "--semantic",
            "-n",
            "1",
            "storage cluster promotion",
        ],
    );
    assert!(ok, "search failed: {err}");
    assert_eq!(
        out,
        format!(
            " 1. [2026-06-12 01:00] (7mo old) user · -tmp-demo · aaaa-bbb\n\
             \x20   the database failover took ten minutes to settle\n\
             \n\
             [subrosa] 1 result(s). Open a session: {}/<project>/<session_id>.jsonl\n",
            env.projects.display()
        )
    );
    assert_eq!(
        err, "[subrosa] semantic: ranked 3 turns via nomic-embed-text\n",
        "the ranking note belongs on stderr, alone"
    );
    // The query side gets nomic's other task prefix.
    assert_eq!(
        stub.seen().last().unwrap(),
        "search_query: storage cluster promotion"
    );

    // Re-running the backfill is a no-op — the model is part of the key.
    let (out, _, ok) = run(&env, &stub.host(), &["embed"]);
    assert!(ok);
    assert_eq!(
        out,
        "[subrosa] embed: every turn is already embedded with nomic-embed-text\n"
    );
}

/// Secret shapes in the query never reach the model. Stored turns are already
/// redacted at ingest; the query is the one piece of live user input here.
#[test]
fn the_query_is_redacted_before_it_reaches_ollama() {
    let env = setup("redact");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);
    run(&env, &stub.host(), &["embed"]);

    let (_, _, ok) = run(
        &env,
        &stub.host(),
        &[
            "search",
            "--semantic",
            "-n",
            "1",
            "password=hunter2 failover",
        ],
    );
    assert!(ok);
    let asked = stub.seen();
    assert!(
        !asked.iter().any(|s| s.contains("hunter2")),
        "the secret reached the model: {asked:?}"
    );
    assert_eq!(
        asked.last().unwrap(),
        "search_query: password=‹redacted› failover"
    );
}

/// Ollama down is a hard failure on both paths — never a silent fall back to
/// keyword search.
#[test]
fn ollama_down_fails_loudly() {
    let env = setup("down");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);

    let (_, err, ok) = run(&env, DEAD_HOST, &["embed"]);
    assert!(!ok, "embed should fail when nothing is listening");
    assert_eq!(
        err,
        format!("[subrosa] cannot reach Ollama at {DEAD_HOST} — is it running? (ollama serve)\n")
    );

    // Same again once the vectors exist: only the query needs Ollama, and it
    // still can't reach it.
    assert!(run(&env, &stub.host(), &["embed"]).2);
    let (out, err, ok) = run(&env, DEAD_HOST, &["search", "--semantic", "failover"]);
    assert!(!ok, "search --semantic should fail, not degrade");
    assert_eq!(
        err,
        format!("[subrosa] cannot reach Ollama at {DEAD_HOST} — is it running? (ollama serve)\n")
    );
    assert_eq!(out, "", "no results on a failed embed");
}

#[test]
fn a_model_that_was_never_pulled_names_the_pull_command() {
    let env = setup("nomodel");
    let stub = Stub::start(Mode::NoModel);
    ingest_demo(&env);

    let (_, err, ok) = run(&env, &stub.host(), &["embed"]);
    assert!(!ok);
    assert_eq!(
        err,
        "[subrosa] model 'nomic-embed-text' not found — run: ollama pull nomic-embed-text\n"
    );
}

/// `--semantic` before any backfill says what to run, and exits clean.
#[test]
fn semantic_without_embeddings_points_at_the_backfill() {
    let env = setup("empty");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);

    let (out, err, ok) = run(&env, &stub.host(), &["search", "--semantic", "failover"]);
    assert!(ok, "a missing backfill is not an error: {err}");
    assert_eq!(out, "[subrosa] no embeddings yet — run: subrosa embed\n");
    assert!(
        stub.seen().is_empty(),
        "nothing should be embedded before the check: {:?}",
        stub.seen()
    );
}

/// The existing filters narrow the ranked set the same way they narrow keyword
/// search — the candidate count on stderr is the proof.
#[test]
fn filters_compose_with_semantic_ranking() {
    let env = setup("filters");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);
    ingest(
        &env,
        "-tmp-other",
        "cccc-dddd-3333",
        &["a second failover drill, this time in staging"],
    );
    assert!(run(&env, &stub.host(), &["embed"]).2);

    let (out, err, ok) = run(
        &env,
        &stub.host(),
        &[
            "search",
            "--semantic",
            "--project",
            "other",
            "-n",
            "5",
            "promotion",
        ],
    );
    assert!(ok, "{err}");
    assert!(
        err.contains("ranked 1 turns"),
        "--project should cut the candidate set to one turn: {err}"
    );
    assert!(out.contains("a second failover drill"), "{out}");
    assert!(!out.contains("invoice totals"), "{out}");

    // --exclude drops candidates the keyword index says hold the term.
    let (out, err, ok) = run(
        &env,
        &stub.host(),
        &[
            "search",
            "--semantic",
            "--exclude",
            "invoice",
            "-n",
            "5",
            "promotion",
        ],
    );
    assert!(ok, "{err}");
    assert!(err.contains("ranked 3 turns"), "{err}");
    assert!(!out.contains("invoice totals"), "{out}");

    // A date bound that predates the archive leaves nothing to rank.
    let (out, _, ok) = run(
        &env,
        &stub.host(),
        &[
            "search",
            "--semantic",
            "--before",
            "2020-01-01",
            "promotion",
        ],
    );
    assert!(ok);
    assert_eq!(out, "[subrosa] no matches for: promotion\n");
}

/// `--semantic` builds no FTS5 query, so the flags that shape one are rejected
/// instead of quietly ignored.
#[test]
fn semantic_refuses_the_fts_only_flags() {
    let env = setup("conflict");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);

    for flag in ["--fuzzy", "--raw", "--any"] {
        let (_, err, ok) = run(
            &env,
            &stub.host(),
            &["search", "--semantic", flag, "failover"],
        );
        assert!(!ok, "{flag} should be rejected");
        assert!(
            err.contains("can't combine with --fuzzy/--raw/--any"),
            "{err}"
        );
    }
}

/// A vector we can't read is a hard stop, not a zero row cached forever.
#[test]
fn an_unusable_vector_is_rejected_and_nothing_is_stored() {
    let env = setup("badvec");
    let stub = Stub::start(Mode::BadVector);
    ingest_demo(&env);

    let (_, err, ok) = run(&env, &stub.host(), &["embed"]);
    assert!(!ok, "a non-numeric vector must fail the backfill");
    assert!(err.contains("unusable embedding value"), "{err}");

    // Nothing was cached, so semantic search still says the index is empty.
    let (out, _, ok) = run(&env, &stub.host(), &["search", "--semantic", "failover"]);
    assert!(ok);
    assert_eq!(out, "[subrosa] no embeddings yet — run: subrosa embed\n");
}

/// Vectors of two different widths can never be compared, so a model that
/// changes shape under a stored archive is refused on both the write and the
/// read side rather than scored on a shared prefix.
#[test]
fn a_dimension_change_is_refused_on_both_sides() {
    let env = setup("dims");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);
    assert!(run(&env, &stub.host(), &["embed"]).2, "3-dim backfill");

    // The model now answers with 2 values. A new turn must not be stored at the
    // new width next to the 3-value rows already there.
    stub.set(Mode::ShortVector);
    ingest(
        &env,
        "-tmp-demo",
        "eeee-ffff-4444",
        &["one more failover to embed"],
    );
    let (_, err, ok) = run(&env, &stub.host(), &["embed"]);
    assert!(!ok, "a width change must stop the backfill");
    assert!(err.contains("refusing to mix dimensions"), "{err}");
    // The check lives inside the write transaction, so the refused batch left
    // nothing behind at the new width.
    let widths: Vec<i64> = db(&env)
        .prepare("SELECT DISTINCT dim FROM turn_embeddings ORDER BY dim")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(widths, vec![3], "a refused batch committed anyway");

    // And the query side refuses too, instead of ranking on a shared prefix.
    let (_, err, ok) = run(&env, &stub.host(), &["search", "--semantic", "failover"]);
    assert!(!ok, "a stale index must not be ranked against");
    assert!(err.contains("Re-run: subrosa embed"), "{err}");
}

/// Open the archive directly, to inspect or corrupt what the backfill stored.
fn db(env: &TestEnv) -> rusqlite::Connection {
    rusqlite::Connection::open(env.data.join("memory.db")).unwrap()
}

/// Damage the stored vector of the one turn whose text matches `like`, the way
/// a bad disk or an interrupted write would. `dim` is what the row claims.
fn corrupt_row(env: &TestEnv, like: &str, dim: i64, vec: Vec<u8>) {
    let conn = db(env);
    let id: i64 = conn
        .query_row("SELECT id FROM turns WHERE text LIKE ?1", [like], |r| {
            r.get(0)
        })
        .unwrap();
    conn.execute(
        "UPDATE turn_embeddings SET dim = ?1, vec = ?2 WHERE turn_id = ?3",
        rusqlite::params![dim, vec, id],
    )
    .unwrap();
}

/// A row whose stored width isn't the model's must be counted as pending and
/// NOT ranked. Counting it in both is how "N of M" starts lying — and the byte
/// length of its blob must not change that either way.
#[test]
fn a_stale_width_row_is_pending_not_ranked() {
    for (label, blob) in [
        // Blob length still matches the model's width — the double-count case.
        ("same byte length", vec![0u8; 12]),
        ("short blob", vec![0u8; 8]),
    ] {
        let env = setup(&format!("stale-{}", blob.len()));
        let stub = Stub::start(Mode::Ok);
        ingest_demo(&env);
        assert!(run(&env, &stub.host(), &["embed"]).2);
        corrupt_row(&env, "%invoice%", 2, blob);

        let (out, err, ok) = run(&env, &stub.host(), &["search", "--semantic", "promotion"]);
        assert!(ok, "{label}: {err}");
        assert!(
            err.contains("INCOMPLETE INDEX: 1 of 3 matching turns"),
            "{label}: the stale row must be counted once, as pending: {err}"
        );
        assert!(err.contains("ranked 2 turns"), "{label}: {err}");
        assert!(!out.contains("invoice totals"), "{label}: {out}");

        // Same row, now the only one the filters select: no results at all, and
        // the warning is the only thing saying why.
        let (out, err, ok) = run(
            &env,
            &stub.host(),
            &[
                "search",
                "--semantic",
                "--exclude",
                "failover",
                "--exclude",
                "pod",
                "promotion",
            ],
        );
        assert!(ok, "{label}: {err}");
        assert_eq!(out, "[subrosa] no matches for: promotion\n", "{label}");
        assert!(
            err.contains("INCOMPLETE INDEX: 1 of 1 matching turns"),
            "{label}: {err}"
        );
    }
}

/// A corrupt vector of the right width but huge values scores far outside the
/// [-1, 1] a unit vector can reach. It must be dropped, not put on top.
#[test]
fn a_huge_stored_vector_cannot_outrank_real_hits() {
    let env = setup("huge");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);
    assert!(run(&env, &stub.host(), &["embed"]).2);
    let huge: Vec<u8> = std::iter::repeat_n(f32::MAX.to_le_bytes(), 3)
        .flatten()
        .collect();
    corrupt_row(&env, "%invoice%", 3, huge);

    let (out, err, ok) = run(
        &env,
        &stub.host(),
        &[
            "search",
            "--semantic",
            "-n",
            "1",
            "storage cluster promotion",
        ],
    );
    assert!(ok, "{err}");
    assert!(err.contains("skipped 1 unreadable embedding(s)"), "{err}");
    assert!(
        !out.contains("invoice totals"),
        "it outranked a real hit: {out}"
    );
    assert!(out.contains("the database failover"), "{out}");
}

/// Filters can select only un-embedded turns, which returns nothing at all. That
/// reads as "nothing here" unless the incomplete-index warning fires too.
#[test]
fn filters_selecting_only_unembedded_turns_still_warn() {
    let env = setup("allpending");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);
    assert!(run(&env, &stub.host(), &["embed"]).2);

    // A whole project arrives after the index was built.
    ingest(
        &env,
        "-tmp-other",
        "cccc-dddd-3333",
        &["a later failover in staging"],
    );

    let (out, err, ok) = run(
        &env,
        &stub.host(),
        &["search", "--semantic", "--project", "other", "promotion"],
    );
    assert!(ok, "{err}");
    assert_eq!(out, "[subrosa] no matches for: promotion\n");
    assert!(
        err.contains("INCOMPLETE INDEX: 1 of 1 matching turns"),
        "an empty result still has to name what wasn't searched: {err}"
    );
}

/// A stored vector that went non-finite on disk would sort ABOVE every real hit
/// under total_cmp. It has to be skipped, counted, and repairable.
#[test]
fn a_stored_vector_gone_non_finite_is_skipped_and_rebuildable() {
    let env = setup("nanvec");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);
    assert!(run(&env, &stub.host(), &["embed"]).2);

    // Corrupt the turn that normally scores 0 against "promotion" — with a NaN
    // in it, an unguarded sort would put it first.
    let conn = db(&env);
    let id: i64 = conn
        .query_row(
            "SELECT id FROM turns WHERE text LIKE '%invoice%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let nan: Vec<u8> = std::iter::repeat_n(f32::NAN.to_le_bytes(), 3)
        .flatten()
        .collect();
    conn.execute(
        "UPDATE turn_embeddings SET vec = ?1 WHERE turn_id = ?2",
        rusqlite::params![nan, id],
    )
    .unwrap();
    drop(conn);

    let (out, err, ok) = run(
        &env,
        &stub.host(),
        &[
            "search",
            "--semantic",
            "-n",
            "1",
            "storage cluster promotion",
        ],
    );
    assert!(ok, "{err}");
    assert!(err.contains("skipped 1 unreadable embedding(s)"), "{err}");
    assert!(
        !out.contains("invoice totals"),
        "NaN took the top slot: {out}"
    );
    assert!(out.contains("the database failover"), "{out}");

    // --rebuild replaces what's stored, so the corruption clears.
    let (out, err, ok) = run(&env, &stub.host(), &["embed", "--rebuild"]);
    assert!(ok, "{err}");
    assert!(out.contains("cleared 3 stored vector(s)"), "{out}");
    let (_, err, ok) = run(&env, &stub.host(), &["search", "--semantic", "promotion"]);
    assert!(ok);
    assert!(
        !err.contains("unreadable"),
        "rebuild left corruption: {err}"
    );
}

/// Turns archived after the backfill are invisible to ranking, so a partial
/// index has to say so — loudly — while still showing what it does have.
#[test]
fn turns_archived_after_the_backfill_warn_about_an_incomplete_index() {
    let env = setup("partial");
    let stub = Stub::start(Mode::Ok);
    ingest_demo(&env);
    assert!(run(&env, &stub.host(), &["embed"]).2);

    // Two more turns land after the index was built.
    ingest(
        &env,
        "-tmp-demo",
        "eeee-ffff-4444",
        &["a later note about promotion", "and one about invoices"],
    );

    let (out, err, ok) = run(
        &env,
        &stub.host(),
        &[
            "search",
            "--semantic",
            "-n",
            "1",
            "storage cluster promotion",
        ],
    );
    assert!(ok, "a partial index still returns what it has: {err}");
    assert!(
        err.contains("INCOMPLETE INDEX: 2 of 5 matching turns"),
        "the pending count must be named: {err}"
    );
    assert!(
        err.contains("Run `subrosa embed` to finish the index"),
        "{err}"
    );
    // Warn and proceed: the ranking over what IS embedded still comes back.
    assert!(
        out.contains("the database failover took ten minutes"),
        "{out}"
    );

    // Finishing the backfill clears the warning.
    assert!(run(&env, &stub.host(), &["embed"]).2);
    let (_, err, ok) = run(
        &env,
        &stub.host(),
        &["search", "--semantic", "-n", "1", "promotion"],
    );
    assert!(ok);
    assert!(!err.contains("INCOMPLETE"), "warning should be gone: {err}");
}
