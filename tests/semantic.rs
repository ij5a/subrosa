//! Opt-in semantic search, with the model NOT on disk — the half CI can run.
//! Nothing here downloads or loads the 1.3 GB weights: the child gets a PATH
//! with no curl on it, so the fetch fails exactly the way it does offline.
//! The ranking itself is covered by the `#[ignore]`d test in `src/embed.rs`,
//! which needs the real weights.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_subrosa")
}

/// No curl (nor anything else) is reachable through this.
const NO_CURL_PATH: &str = "/nonexistent-subrosa-test";

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

/// A child pointed at the throwaway dirs, with EVERY inherited `SUBROSA_*`
/// dropped first — `SUBROSA_DB` in a developer's shell outranks `SUBROSA_DIR`
/// and would aim the suite at a real database.
fn run(env: &TestEnv, args: &[&str]) -> (String, String, bool) {
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
        .env("PATH", NO_CURL_PATH)
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

/// Three turns on three topics, the same archive the ranking tests use.
fn ingest_demo(env: &TestEnv) {
    let dir = env.projects.join("-tmp-demo");
    fs::create_dir_all(&dir).unwrap();
    let records: Vec<String> = [
        "the database failover took ten minutes to settle",
        "the pod restarted after the node was drained",
        "we rewrote the invoice totals report",
    ]
    .iter()
    .enumerate()
    .map(|(i, text)| {
        format!(
            r#"{{"type":"user","timestamp":"2026-06-12T01:0{i}:00Z","uuid":"u{i}","cwd":"/tmp/demo","message":{{"role":"user","content":"{text}"}}}}"#
        )
    })
    .collect();
    let t = dir.join("aaaa-bbbb-2222.jsonl");
    fs::write(&t, records.join("\n") + "\n").unwrap();
    assert!(run(env, &["ingest", t.to_str().unwrap()]).2);
}

/// Open the archive directly, to seed what a backfill would have stored.
fn db(env: &TestEnv) -> rusqlite::Connection {
    rusqlite::Connection::open(env.data.join("memory.db")).unwrap()
}

/// A missing model is a hard stop that names both ways forward: the command
/// can't fetch it, so the manual URLs and the folder they go in are the answer.
#[test]
fn embed_without_the_model_says_where_to_get_it() {
    let env = setup("nomodel");
    ingest_demo(&env);

    let (_, err, ok) = run(&env, &["embed"]);
    assert!(!ok, "embed can't work without the model");
    assert!(
        err.contains("[subrosa] downloading bge-large-en-v1.5 (~1.3 GB, one time)"),
        "{err}"
    );
    assert!(err.contains("cannot run curl"), "{err}");
    // The folder is named for the pinned revision, so re-pinning starts empty
    // instead of loading whatever the old revision left there.
    assert!(
        err.contains(&format!(
            "download these into {}",
            env.data
                .join("models")
                .join("bge-large-en-v1.5@d4aa6901")
                .display()
        )),
        "{err}"
    );
    for file in ["model.safetensors", "config.json", "vocab.txt"] {
        assert!(
            err.contains(&format!(
                "https://huggingface.co/BAAI/bge-large-en-v1.5/resolve/\
                 d4aa6901d3a41ba39fb536a557fa166f842b0e09/{file}"
            )),
            "{file} is not in the manual instructions: {err}"
        );
        // Saved under the .part name, a hand-download is checksummed on the
        // next run; saved at its final name it would be trusted on size.
        assert!(
            err.contains(&format!("{file}.part")),
            "{file} is not named as a .part: {err}"
        );
    }
}

/// Nothing to embed means nothing to load: an archive that's already done must
/// not trigger a 1.3 GB download.
#[test]
fn an_empty_backfill_never_reaches_for_the_model() {
    let env = setup("nowork");
    let (out, err, ok) = run(&env, &["embed"]);
    assert!(ok, "{err}");
    assert_eq!(
        out,
        "[subrosa] embed: every turn is already embedded with bge-large-en-v1.5\n"
    );
    assert!(!err.contains("downloading"), "{err}");
}

/// `--semantic` before any backfill says what to run, exits clean, and doesn't
/// load the model to say it.
#[test]
fn semantic_without_embeddings_points_at_the_backfill() {
    let env = setup("empty");
    ingest_demo(&env);

    let (out, err, ok) = run(&env, &["search", "--semantic", "failover"]);
    assert!(ok, "a missing backfill is not an error: {err}");
    assert_eq!(out, "[subrosa] no embeddings yet — run: subrosa embed\n");
    assert!(!err.contains("downloading"), "{err}");
    assert!(!env.data.join("models").exists(), "the model was fetched");
}

/// With vectors stored but no model to embed the query, `--semantic` fails
/// loudly — it never quietly falls back to keyword ranking.
#[test]
fn semantic_with_stored_vectors_but_no_model_fails_loudly() {
    let env = setup("noquery");
    ingest_demo(&env);
    let conn = db(&env);
    // Rows are keyed by model AND pinned revision — the bare name is a v0.22
    // Ollama-era key and must not be mistaken for this model's vectors.
    conn.execute_batch(
        "CREATE TABLE turn_embeddings (turn_id INTEGER NOT NULL, model TEXT NOT NULL, \
         dim INTEGER NOT NULL, vec BLOB NOT NULL, PRIMARY KEY (turn_id, model));\
         INSERT INTO turn_embeddings(turn_id, model, dim, vec) \
         SELECT id, 'bge-large-en-v1.5@d4aa6901', 2, x'0000803f00000000' FROM turns;",
    )
    .unwrap();
    drop(conn);

    let (out, err, ok) = run(&env, &["search", "--semantic", "failover"]);
    assert!(!ok, "search --semantic should fail, not degrade");
    assert!(err.contains("cannot run curl"), "{err}");
    assert_eq!(out, "", "no results without a query vector");
}

/// A v0.22 archive backfilled through Ollama can hold rows under the bare model
/// name. That's a different vector space, so it must not be ranked against, and
/// must not read as a finished backfill either.
#[test]
fn vectors_stored_under_the_old_bare_name_are_ignored() {
    let env = setup("oldkey");
    ingest_demo(&env);
    let conn = db(&env);
    conn.execute_batch(
        "CREATE TABLE turn_embeddings (turn_id INTEGER NOT NULL, model TEXT NOT NULL, \
         dim INTEGER NOT NULL, vec BLOB NOT NULL, PRIMARY KEY (turn_id, model));\
         INSERT INTO turn_embeddings(turn_id, model, dim, vec) \
         SELECT id, 'bge-large-en-v1.5', 2, x'0000803f00000000' FROM turns;",
    )
    .unwrap();
    drop(conn);

    let (out, err, ok) = run(&env, &["search", "--semantic", "failover"]);
    assert!(ok, "{err}");
    assert_eq!(out, "[subrosa] no embeddings yet — run: subrosa embed\n");
    // And the backfill still has every turn to do, so it reaches for the model.
    let (_, err, ok) = run(&env, &["embed"]);
    assert!(!ok, "old rows must not count as an embedded archive");
    assert!(err.contains("downloading bge-large-en-v1.5"), "{err}");
}

/// `--semantic` builds no FTS5 query, so the flags that shape one are rejected
/// instead of quietly ignored.
#[test]
fn semantic_refuses_the_fts_only_flags() {
    let env = setup("conflict");
    ingest_demo(&env);

    for flag in ["--fuzzy", "--raw", "--any"] {
        let (_, err, ok) = run(&env, &["search", "--semantic", flag, "failover"]);
        assert!(!ok, "{flag} should be rejected");
        assert!(
            err.contains("can't combine with --fuzzy/--raw/--any"),
            "{err}"
        );
    }
}
