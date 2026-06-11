//! Keyword search over the archived transcripts (FTS5, bm25-ranked).

use std::process::ExitCode;

use rusqlite::params_from_iter;

use crate::{db, paths};

/// Quote each whitespace term as a phrase so identifiers like `my-app-prod` /
/// `TICKET-123` match instead of tripping FTS5's column/NOT operators on the hyphen.
pub fn build_match(terms: &[String], raw: bool) -> String {
    let q = terms.join(" ").trim().to_string();
    if raw {
        return q;
    }
    q.split_whitespace()
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Display the stored ISO timestamp as `YYYY-MM-DD HH:MM` (stored zone, ~UTC).
fn fmt_ts(ts: &str) -> String {
    if ts.is_empty() {
        return "?".to_string();
    }
    ts.get(..16)
        .map(|s| s.replace('T', " "))
        .unwrap_or_else(|| ts.to_string())
}

pub fn run(
    terms: &[String],
    limit: i64,
    raw: bool,
    project: Option<&str>,
    session: Option<&str>,
) -> ExitCode {
    if terms.is_empty() {
        eprintln!("[subrosa] give search terms");
        return ExitCode::from(2);
    }
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    let m = build_match(terms, raw);

    let mut sql = String::from(
        "SELECT t.session_id, t.ts, t.role, t.project, \
                snippet(turns_fts, 0, '«', '»', '…', 12) AS snip \
         FROM turns_fts JOIN turns t ON t.id = turns_fts.rowid \
         WHERE turns_fts MATCH ?",
    );
    let mut binds: Vec<String> = vec![m.clone()];
    if let Some(p) = project {
        sql.push_str(" AND t.project LIKE ?");
        binds.push(format!("%{p}%"));
    }
    if let Some(s) = session {
        sql.push_str(" AND t.session_id LIKE ?");
        binds.push(format!("{s}%"));
    }
    // limit is a typed integer from clap — safe to inline; strings stay parameterized.
    sql.push_str(&format!(" ORDER BY bm25(turns_fts) LIMIT {limit}"));

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[subrosa] query error: {e}");
            eprintln!("[subrosa] tip: drop --raw, or wrap special characters in quotes");
            return ExitCode::FAILURE;
        }
    };
    // (session_id, ts, role, project, snippet)
    type Hit = (
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    );
    let rows: Result<Vec<Hit>, _> = stmt
        .query_map(params_from_iter(binds.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .and_then(|it| it.collect());
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[subrosa] query error: {e}");
            eprintln!("[subrosa] tip: drop --raw, or wrap special characters in quotes");
            return ExitCode::FAILURE;
        }
    };

    if rows.is_empty() {
        println!("[subrosa] no matches for: {m}");
        return ExitCode::SUCCESS;
    }
    for (i, (sid, ts, role, project, snip)) in rows.iter().enumerate() {
        let snip = snip
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let sid8: String = sid.chars().take(8).collect();
        println!(
            "{:>2}. [{}] {} · {} · {}",
            i + 1,
            fmt_ts(ts.as_deref().unwrap_or("")),
            role,
            project.as_deref().unwrap_or("?"),
            sid8
        );
        println!("    {snip}");
    }
    println!(
        "\n[subrosa] {} result(s). Open a session: {}/<project>/<session_id>.jsonl",
        rows.len(),
        paths::projects_dir().display()
    );
    ExitCode::SUCCESS
}
