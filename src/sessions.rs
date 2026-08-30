//! `subrosa sessions` — list past sessions newest-first, filterable by project,
//! date span, and auto-derived tag. The archive's by-session view: it answers
//! "what was I working on last month" without a keyword. Reads the `sessions`
//! table (and `session_tags` for the per-page tag fetch); uses the read-write
//! `connect()` so the v3 schema is guaranteed present — not a hot path.

use std::collections::HashMap;
use std::process::ExitCode;

use rusqlite::params_from_iter;

use crate::{db, paths, tags, timeutil};

/// One listed session: span + counts, plus its display-ordered tags.
struct Row {
    sid: String,
    project: String,
    first_ts: String,
    last_ts: String,
    num_turns: i64,
    tags: Vec<String>,
}

pub fn run(
    project: Option<&str>,
    after: Option<&str>,
    before: Option<&str>,
    tag: &[String],
    limit: i64,
) -> ExitCode {
    let (after_bound, before_bound) = match timeutil::date_bounds(after, before) {
        Ok(bounds) => bounds,
        Err(flag) => {
            let s = if flag == "--after" {
                after.unwrap_or("")
            } else {
                before.unwrap_or("")
            };
            eprintln!("[subrosa] bad {flag} date (want YYYY-MM-DD): {s}");
            return ExitCode::from(2);
        }
    };

    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Date filters span the session: --after keeps sessions ending on/after D,
    // --before keeps sessions starting before D+1, so a straddling session shows.
    let mut sql = String::from(
        "SELECT session_id, project, first_ts, last_ts, num_turns FROM sessions WHERE 1=1",
    );
    let mut binds: Vec<String> = Vec::new();
    if let Some(p) = project {
        sql.push_str(" AND project LIKE ?");
        binds.push(format!("%{p}%"));
    }
    if let Some(a) = &after_bound {
        sql.push_str(" AND last_ts >= ?");
        binds.push(a.clone());
    }
    if let Some(b) = &before_bound {
        sql.push_str(" AND first_ts < ?");
        binds.push(b.clone());
    }
    // EXISTS, not JOIN: a JOIN would multiply session rows per matching tag. ANDed.
    for tg in tag {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM session_tags st \
             WHERE st.session_id = sessions.session_id AND st.tag = ?)",
        );
        binds.push(tg.clone());
    }
    sql.push_str(&format!(" ORDER BY last_ts DESC LIMIT {limit}"));

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return query_error(e),
    };
    type Raw = (String, Option<String>, Option<String>, Option<String>, i64);
    let raw: Result<Vec<Raw>, _> = stmt
        .query_map(params_from_iter(binds.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .and_then(|it| it.collect());
    let raw = match raw {
        Ok(r) => r,
        Err(e) => return query_error(e),
    };
    if raw.is_empty() {
        println!("[subrosa] no sessions match.");
        return ExitCode::SUCCESS;
    }

    // One batched fetch of every tag for the page (no N+1), grouped per session.
    let sids: Vec<String> = raw.iter().map(|r| r.0.clone()).collect();
    let tags_by_sid = match fetch_tags(&conn, &sids) {
        Ok(m) => m,
        Err(e) => return query_error(e),
    };

    let rows: Vec<Row> = raw
        .into_iter()
        .map(|(sid, project, first_ts, last_ts, num_turns)| {
            let tags = tags_by_sid.get(&sid).cloned().unwrap_or_default();
            Row {
                sid,
                project: project.unwrap_or_default(),
                first_ts: first_ts.unwrap_or_default(),
                last_ts: last_ts.unwrap_or_default(),
                num_turns,
                tags,
            }
        })
        .collect();

    for (i, r) in rows.iter().enumerate() {
        println!(
            "{:>2}. [{}] {} · {} · {} turns",
            i + 1,
            fmt_span(&r.first_ts, &r.last_ts),
            if r.project.is_empty() {
                "?"
            } else {
                &r.project
            },
            crate::text::sid8(&r.sid),
            r.num_turns
        );
        if r.tags.is_empty() {
            println!("    —");
        } else {
            println!("    tags: {}", r.tags.join(", "));
        }
    }
    println!(
        "\n[subrosa] {} session(s). Open a session: {}/<project>/<session_id>.jsonl",
        rows.len(),
        paths::projects_dir().display()
    );
    ExitCode::SUCCESS
}

/// Batched per-page tag fetch, grouped by session and sorted into display order
/// (tool, ext, topic; by rank within each) — the same order `session --tags` uses.
fn fetch_tags(
    conn: &rusqlite::Connection,
    sids: &[String],
) -> rusqlite::Result<HashMap<String, Vec<String>>> {
    let placeholders = sids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT session_id, ns, tag, rank FROM session_tags WHERE session_id IN ({placeholders})"
    );
    let mut grouped: HashMap<String, Vec<(String, String, i64)>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let mut q = stmt.query(params_from_iter(sids.iter()))?;
        while let Some(row) = q.next()? {
            let sid: String = row.get(0)?;
            grouped
                .entry(sid)
                .or_default()
                .push((row.get(1)?, row.get(2)?, row.get(3)?));
        }
    }
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (sid, mut v) in grouped {
        v.sort_by(|a, b| {
            tags::ns_order(&a.0)
                .cmp(&tags::ns_order(&b.0))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.1.cmp(&b.1))
        });
        out.insert(sid, v.into_iter().map(|(_, tag, _)| tag).collect());
    }
    Ok(out)
}

/// Render a session's span as `YYYY-MM-DD HH:MM .. HH:MM`, compacting the end to
/// `HH:MM` when it falls on the same calendar day as the start (else full stamp).
fn fmt_span(first: &str, last: &str) -> String {
    let start = timeutil::fmt_ts(first);
    let end = if !first.is_empty() && first.get(..10) == last.get(..10) {
        last.get(11..16)
            .map(str::to_string)
            .unwrap_or_else(|| timeutil::fmt_ts(last))
    } else {
        timeutil::fmt_ts(last)
    };
    format!("{start} .. {end}")
}

fn query_error(e: rusqlite::Error) -> ExitCode {
    eprintln!("[subrosa] query error: {e}");
    ExitCode::FAILURE
}
