//! `subrosa redact --apply` must leave no byte of a masked value in the
//! database file, its WAL, or either full-text index.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use rusqlite::Connection;

const PASSWORD: &str = "Zq9x7W4k2J8h5G3f1D60";
const PHRASE: &str = "Hx7Kp2Lm9Qw4Zt8Vb3Nc51";
const AWS: &str = "AKIAIOSFODNN7EXAMPLE";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_subrosa")
}

struct TestEnv {
    data: PathBuf,
    projects: PathBuf,
}

fn setup(tag: &str) -> TestEnv {
    let root = std::env::temp_dir().join(format!("subrosa-redact-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let data = root.join("data");
    let projects = root.join("projects");
    fs::create_dir_all(projects.join("-tmp-demo")).unwrap();
    fs::create_dir_all(&data).unwrap();
    TestEnv { data, projects }
}

/// A child pointed at the throwaway dirs, with EVERY inherited SUBROSA_*
/// dropped first, so an exported SUBROSA_DB can never aim it at a real archive.
fn base_cmd(env: &TestEnv) -> Command {
    let mut cmd = Command::new(bin());
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("SUBROSA_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("SUBROSA_DIR", &env.data)
        .env("SUBROSA_PROJECTS_DIR", &env.projects)
        .env("SUBROSA_SEMANTIC", "off");
    cmd
}

/// Run to completion, require exit 0, and return stdout plus stderr.
fn run(env: &TestEnv, args: &[&str]) -> String {
    let out = base_cmd(env).args(args).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{args:?} failed: {text}");
    text
}

/// The 5 byte strings a masked value can survive as: each value as written,
/// plus the lowercase form the porter index stores.
fn needles() -> [String; 5] {
    [
        PASSWORD.to_string(),
        PASSWORD.to_lowercase(),
        PHRASE.to_string(),
        PHRASE.to_lowercase(),
        AWS.to_string(),
    ]
}

fn disk_holds(env: &TestEnv, needle: &str) -> bool {
    let mut bytes = fs::read(env.data.join("memory.db")).unwrap();
    bytes.extend(fs::read(env.data.join("memory.db-wal")).unwrap_or_default());
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn text_of(conn: &Connection, id: i64) -> String {
    conn.query_row("SELECT text FROM turns WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Rows in one full-text index that match each old value.
fn index_hits(conn: &Connection) -> Vec<i64> {
    let mut hits = Vec::new();
    for (table, aws) in [
        ("turns_fts", "aakiaiosfodnn7example"),
        ("turns_fts_tri", AWS),
    ] {
        for term in [PASSWORD, PHRASE, aws] {
            hits.push(count(
                conn,
                &format!("SELECT count(*) FROM {table} WHERE {table} MATCH '\"{term}\"'"),
            ));
        }
    }
    hits
}

#[test]
fn apply_leaves_no_trace_of_a_masked_value() {
    let env = setup("apply");
    run(&env, &["init"]);

    // Stays open to the end: with a second connection attached, closing the
    // command's own connection cannot reset the WAL for it.
    let mut conn = Connection::open(env.data.join("memory.db")).unwrap();

    let password_turn = format!("login with password={PASSWORD} then continue");
    let phrase_turn = format!("passphrase: {}{PHRASE}", "lorem ipsum ".repeat(600));
    let aws_turn = format!("id A{AWS} ok");
    let control_turn = "xyzzycontrol stays as written";
    let tx = conn.transaction().unwrap();
    for i in 0..30 {
        tx.execute(
            "INSERT INTO turns(session_id, seq, role, text) VALUES('s1', ?1, 'user', ?2)",
            rusqlite::params![i, format!("filler turn {i}")],
        )
        .unwrap();
    }
    let mut ids = Vec::new();
    for (i, text) in [&password_turn, &phrase_turn, &aws_turn, control_turn]
        .iter()
        .enumerate()
    {
        tx.execute(
            "INSERT INTO turns(session_id, seq, role, text) VALUES('s2', ?1, 'user', ?2)",
            rusqlite::params![i as i64, text],
        )
        .unwrap();
        ids.push(tx.last_insert_rowid());
    }
    let (password_id, phrase_id, aws_id, control_id) = (ids[0], ids[1], ids[2], ids[3]);
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS turn_embeddings (\
           turn_id INTEGER NOT NULL,\
           model   TEXT NOT NULL,\
           dim     INTEGER NOT NULL,\
           vec     BLOB NOT NULL,\
           PRIMARY KEY (turn_id, model)\
         );",
    )
    .unwrap();
    for id in [password_id, control_id] {
        tx.execute(
            "INSERT INTO turn_embeddings(turn_id, model, dim, vec) VALUES(?1, 'm', 1, x'00')",
            [id],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    run(&env, &["search", "--fuzzy", "xyzzycontrol"]);

    for n in needles() {
        assert!(disk_holds(&env, &n), "control: {n} is not on disk");
    }
    assert_eq!(index_hits(&conn), [1; 6], "control: index hits");

    let dry = run(&env, &["redact"]);
    assert!(dry.starts_with("3 turn(s)"), "{dry}");
    for rule in ["labeled-secret: 1", "passphrase: 1", "aws-key: 1"] {
        assert!(dry.contains(rule), "{rule} missing: {dry}");
    }
    for n in needles() {
        assert!(!dry.contains(&n), "dry run printed {n}");
    }
    assert_eq!(text_of(&conn, password_id), password_turn);
    assert_eq!(text_of(&conn, phrase_id), phrase_turn);
    assert_eq!(text_of(&conn, aws_id), aws_turn);

    let applied = run(&env, &["redact", "--apply"]);
    for n in needles() {
        assert!(!applied.contains(&n), "--apply printed {n}");
        assert!(!disk_holds(&env, &n), "{n} is still on disk");
    }
    assert_eq!(
        text_of(&conn, password_id),
        "login with password=‹redacted› then continue"
    );
    assert_eq!(text_of(&conn, phrase_id), "passphrase: ‹redacted›");
    assert_eq!(text_of(&conn, aws_id), "id A‹redacted-aws-key› ok");
    assert_eq!(text_of(&conn, control_id), control_turn);
    assert_eq!(index_hits(&conn), [0; 6], "old values still match");
    for table in ["turns_fts", "turns_fts_tri"] {
        conn.execute(
            &format!("INSERT INTO {table}({table}, rank) VALUES('integrity-check', 1)"),
            [],
        )
        .unwrap();
    }
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM turns_fts_tri_config WHERE k='secure-delete' AND v=1"
        ),
        1
    );
    let vectors = |id: i64| {
        count(
            &conn,
            &format!("SELECT count(*) FROM turn_embeddings WHERE turn_id={id}"),
        )
    };
    assert_eq!(vectors(password_id), 0);
    assert_eq!(vectors(control_id), 1);

    let again = run(&env, &["redact", "--apply"]);
    assert!(again.starts_with("0 turn(s)"), "{again}");

    // A later ingest of the same transcript must not bring the raw line back.
    let raw = format!("deploy with password={PASSWORD} now");
    let file = env.projects.join("-tmp-demo").join("s9.jsonl");
    fs::write(
        &file,
        format!(
            r#"{{"type":"user","timestamp":"2026-06-12T01:00:00Z","uuid":"s9-u0","cwd":"/tmp/demo","message":{{"role":"user","content":"{raw}"}}}}"#
        ) + "\n",
    )
    .unwrap();
    let file = file.to_str().unwrap();
    run(&env, &["ingest", file]);
    let s9 = |conn: &Connection| -> String {
        conn.query_row("SELECT text FROM turns WHERE session_id='s9'", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    let masked = s9(&conn);
    assert!(!masked.contains(PASSWORD), "ingest stored the value");
    conn.execute("UPDATE turns SET text=?1 WHERE session_id='s9'", [&raw])
        .unwrap();
    run(&env, &["redact", "--apply"]);
    conn.execute(
        "UPDATE sessions SET scan_offset=0, scan_seq=0, file_size=0, file_mtime=0 \
         WHERE session_id='s9'",
        [],
    )
    .unwrap();
    run(&env, &["sweep"]);
    run(&env, &["ingest", "--require-complete", file]);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM turns WHERE session_id='s9'"),
        1
    );
    assert_eq!(s9(&conn), masked);
}
