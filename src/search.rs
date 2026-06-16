//! Keyword search over the archived transcripts (FTS5, bm25-ranked).

use std::process::ExitCode;

use rusqlite::params_from_iter;

use crate::{db, paths, timeutil};

/// Quote each whitespace term as a phrase so identifiers like `my-app-prod` /
/// `TICKET-123` match instead of tripping FTS5's column/NOT operators on the hyphen.
pub fn build_match(terms: &[String], raw: bool, fuzzy: bool) -> String {
    let q = terms.join(" ").trim().to_string();
    if raw {
        return q;
    }
    q.split_whitespace()
        // The trigram tokenizer (--fuzzy) can't index a token shorter than 3 chars; drop those.
        .filter(|tok| !fuzzy || tok.chars().count() >= 3)
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

#[allow(clippy::too_many_arguments)]
pub fn run(
    terms: &[String],
    limit: i64,
    raw: bool,
    project: Option<&str>,
    session: Option<&str>,
    fuzzy: bool,
    after: Option<&str>,
    before: Option<&str>,
    tags: &[String],
) -> ExitCode {
    if terms.is_empty() {
        eprintln!("[subrosa] give search terms");
        return ExitCode::from(2);
    }
    // Validate + normalize the date bounds up front (mirrors the empty-terms guard).
    // Normalizing to zero-padded YYYY-MM-DD keeps the lexical ts comparison correct.
    let after_bound = match after {
        None => None,
        Some(s) => match timeutil::parse_ymd(s) {
            Some((y, mo, d)) => Some(format!("{y:04}-{mo:02}-{d:02}")),
            None => {
                eprintln!("[subrosa] bad --after date (want YYYY-MM-DD): {s}");
                return ExitCode::from(2);
            }
        },
    };
    let before_bound = match before {
        None => None,
        Some(s) => match timeutil::parse_ymd(s).and_then(|(y, mo, d)| timeutil::next_day(y, mo, d))
        {
            Some(nd) => Some(nd),
            None => {
                eprintln!("[subrosa] bad --before date (want YYYY-MM-DD): {s}");
                return ExitCode::from(2);
            }
        },
    };
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    // --fuzzy queries a separate trigram index (substring/typo matching), built on first use.
    // Recall never uses it; the porter table stays default and backs the per-prompt gate.
    let table = if fuzzy { "turns_fts_tri" } else { "turns_fts" };
    if fuzzy {
        match db::ensure_trigram_index(&conn) {
            Ok(true) => eprintln!(
                "[subrosa] built the fuzzy index (one-time); future --fuzzy searches are instant"
            ),
            Ok(false) => {}
            Err(e) => {
                eprintln!("[subrosa] cannot build the fuzzy index: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let m = build_match(terms, raw, fuzzy);
    if fuzzy && m.trim().is_empty() {
        eprintln!("[subrosa] --fuzzy needs at least one term of 3+ characters");
        return ExitCode::from(2);
    }

    // The table name is a fixed literal chosen by --fuzzy, never user input.
    let mut sql = format!(
        "SELECT t.session_id, t.ts, t.role, t.project, \
                snippet({table}, 0, '«', '»', '…', 12) AS snip \
         FROM {table} JOIN turns t ON t.id = {table}.rowid \
         WHERE {table} MATCH ?"
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
    // Date bounds: ISO timestamps sort lexically, so a string compare is correct.
    if let Some(a) = &after_bound {
        sql.push_str(" AND t.ts >= ?");
        binds.push(a.clone());
    }
    if let Some(b) = &before_bound {
        sql.push_str(" AND t.ts < ?");
        binds.push(b.clone());
    }
    // EXISTS, not JOIN: a JOIN would multiply result rows per matching tag, which
    // corrupts bm25() ranking and LIMIT. Repeated --tag is ANDed.
    for tg in tags {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM session_tags st \
             WHERE st.session_id = t.session_id AND st.tag = ?)",
        );
        binds.push(tg.clone());
    }
    // limit is a typed integer from clap — safe to inline; strings stay parameterized.
    sql.push_str(&format!(" ORDER BY bm25({table}) LIMIT {limit}"));

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
        // Nudge toward the fuzzy fallback when an exact search finds nothing.
        if !fuzzy && !raw {
            println!(
                "[subrosa] no exact match — try fuzzy: subrosa search --fuzzy {}",
                terms.join(" ")
            );
        }
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
