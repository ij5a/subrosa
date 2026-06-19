//! Keyword search over the archived transcripts (FTS5, bm25-ranked).

use std::process::ExitCode;

use rusqlite::{params, params_from_iter};

use crate::{db, paths, timeutil};

/// How many characters of a neighbouring turn `--context` prints before it's cut
/// with `…`. Long enough to orient, short enough to keep results scannable.
const CONTEXT_PREVIEW_CHARS: usize = 160;

/// One-line preview of a context turn: whitespace collapsed, then truncated.
fn ctx_preview(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(CONTEXT_PREVIEW_CHARS).collect();
    if collapsed.chars().count() > CONTEXT_PREVIEW_CHARS {
        out.push('…');
    }
    out
}

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
    context: i64,
) -> ExitCode {
    // Negative is meaningless (clap accepts it); treat it as "no context".
    let context = context.max(0);
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
                snippet({table}, 0, '«', '»', '…', 12) AS snip, t.seq \
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
    // (session_id, ts, role, project, snippet, seq)
    type Hit = (
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        i64,
    );
    let rows: Result<Vec<Hit>, _> = stmt
        .query_map(params_from_iter(binds.iter()), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
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
    // --context looks up the turns on each side of a hit by (session_id, seq);
    // idx_turns_session makes this a cheap indexed range read per hit. Prepared
    // once and reused; None (and so no extra work) on the default context=0 path.
    let mut ctx_stmt = if context > 0 {
        conn.prepare(
            "SELECT seq, role, text FROM turns \
             WHERE session_id = ?1 AND seq BETWEEN ?2 AND ?3 AND seq <> ?4 ORDER BY seq",
        )
        .ok()
    } else {
        None
    };

    let now = timeutil::now_unix();
    for (i, (sid, ts, role, project, snip, seq)) in rows.iter().enumerate() {
        let snip = snip
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let sid8: String = sid.chars().take(8).collect();
        // Relative age after the timestamp, shared with recall (`(7mo old)` etc.);
        // empty when the timestamp is missing or unparseable.
        let age = match ts.as_deref().and_then(timeutil::parse_ts) {
            Some(epoch) => timeutil::age_suffix(now - epoch),
            None => String::new(),
        };
        println!(
            "{:>2}. [{}]{} {} · {} · {}",
            i + 1,
            fmt_ts(ts.as_deref().unwrap_or("")),
            age,
            role,
            project.as_deref().unwrap_or("?"),
            sid8
        );
        // Neighbouring turns (empty unless --context). Split on the hit's own seq
        // so the matched snippet stays in the middle, everything in transcript order.
        let (lo, hi) = (seq.saturating_sub(context), seq.saturating_add(context));
        let neighbors: Vec<(i64, String, String)> = match ctx_stmt.as_mut() {
            Some(cs) => cs
                .query_map(params![sid, lo, hi, seq], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    ))
                })
                .map(|it| it.filter_map(Result::ok).collect())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        for n in neighbors.iter().filter(|n| n.0 < *seq) {
            println!("      {}  {}", n.1, ctx_preview(&n.2));
        }
        println!("    {snip}");
        for n in neighbors.iter().filter(|n| n.0 > *seq) {
            println!("      {}  {}", n.1, ctx_preview(&n.2));
        }
    }
    println!(
        "\n[subrosa] {} result(s). Open a session: {}/<project>/<session_id>.jsonl",
        rows.len(),
        paths::projects_dir().display()
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_preview_collapses_whitespace_and_leaves_short_text_whole() {
        // Newlines and runs of spaces fold to single spaces; nothing is cut.
        assert_eq!(ctx_preview("pod-1\nRunning   pod-2"), "pod-1 Running pod-2");
        assert_eq!(ctx_preview("  trimmed  "), "trimmed");
        assert!(!ctx_preview("short line").ends_with('…'));
    }

    #[test]
    fn ctx_preview_truncates_long_text_with_ellipsis() {
        let long = "x ".repeat(200); // 200 one-char tokens, collapses to 399 chars
        let out = ctx_preview(&long);
        // Capped at CONTEXT_PREVIEW_CHARS tokens/chars plus the ellipsis marker.
        assert_eq!(out.chars().count(), CONTEXT_PREVIEW_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn ctx_preview_counts_chars_not_bytes_for_multibyte_text() {
        // 200 «é» (2 bytes each) must cut on the char boundary, not panic mid-byte.
        let out = ctx_preview(&"é".repeat(200));
        assert_eq!(out.chars().count(), CONTEXT_PREVIEW_CHARS + 1);
        assert!(out.ends_with('…'));
    }
}
