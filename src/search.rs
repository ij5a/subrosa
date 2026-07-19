//! Keyword search over the archived transcripts (FTS5, bm25-ranked).

use std::process::ExitCode;

use rusqlite::{params, params_from_iter};

use crate::{db, paths, timeutil};

/// How many characters of a neighbouring turn `--context` prints before it's cut
/// with `…`. Long enough to orient, short enough to keep results scannable.
const CONTEXT_PREVIEW_CHARS: usize = 160;

/// One-line preview of a context turn: whitespace collapsed, then truncated.
fn ctx_preview(text: &str) -> String {
    let collapsed = crate::text::collapse_ws(text);
    let mut out: String = collapsed.chars().take(CONTEXT_PREVIEW_CHARS).collect();
    if collapsed.chars().count() > CONTEXT_PREVIEW_CHARS {
        out.push('…');
    }
    out
}

/// Phrase-quote each whitespace token so identifiers like `my-app-prod` /
/// `TICKET-123` match instead of tripping FTS5's column/NOT operators on the hyphen.
/// Shared by the positive match and the `--exclude` NOT clauses.
fn quote_terms(terms: &[String], fuzzy: bool) -> Vec<String> {
    terms
        .iter()
        .flat_map(|t| t.split_whitespace())
        // The trigram tokenizer (--fuzzy) can't index a token shorter than 3 chars; drop those.
        .filter(|tok| !fuzzy || tok.chars().count() >= 3)
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect()
}

/// Build the FTS5 MATCH string for the positive search terms (each phrase-quoted),
/// or the user's verbatim query when `raw`.
pub fn build_match(terms: &[String], raw: bool, fuzzy: bool) -> String {
    if raw {
        return terms.join(" ").trim().to_string();
    }
    quote_terms(terms, fuzzy).join(" ")
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    terms: &[String],
    limit: i64,
    raw: bool,
    project: Option<&str>,
    session: Option<&str>,
    fuzzy: bool,
    any: bool,
    after: Option<&str>,
    before: Option<&str>,
    tags: &[String],
    exclude: &[String],
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
    // --any ORs the terms instead of the default AND; raw owns its own query.
    let m = if any && !raw {
        quote_terms(terms, fuzzy).join(" OR ")
    } else {
        build_match(terms, raw, fuzzy)
    };
    if fuzzy && m.trim().is_empty() {
        eprintln!("[subrosa] --fuzzy needs at least one term of 3+ characters");
        return ExitCode::from(2);
    }
    let m = wrap_excludes(m, exclude, raw, fuzzy);

    // WHERE extras shared by the ranked query and the fuzzy nearest-match fallback.
    let mut where_extra = String::new();
    let mut extra_binds: Vec<String> = Vec::new();
    if let Some(p) = project {
        where_extra.push_str(" AND t.project LIKE ?");
        extra_binds.push(format!("%{p}%"));
    }
    if let Some(s) = session {
        where_extra.push_str(" AND t.session_id LIKE ?");
        extra_binds.push(format!("{s}%"));
    }
    // Date bounds: ISO timestamps sort lexically, so a string compare is correct.
    if let Some(a) = &after_bound {
        where_extra.push_str(" AND t.ts >= ?");
        extra_binds.push(a.clone());
    }
    if let Some(b) = &before_bound {
        where_extra.push_str(" AND t.ts < ?");
        extra_binds.push(b.clone());
    }
    // EXISTS, not JOIN: a JOIN would multiply result rows per matching tag, which
    // corrupts bm25() ranking and LIMIT. Repeated --tag is ANDed.
    for tg in tags {
        where_extra.push_str(
            " AND EXISTS (SELECT 1 FROM session_tags st \
             WHERE st.session_id = t.session_id AND st.tag = ?)",
        );
        extra_binds.push(tg.clone());
    }

    // The table name is a fixed literal chosen by --fuzzy, never user input; the
    // tail is a fixed ORDER/LIMIT clause built from typed integers. Strings stay bound.
    let fetch = |match_str: &str, tail: &str| -> rusqlite::Result<Vec<Hit>> {
        let sql = format!(
            "SELECT t.session_id, t.ts, t.role, t.project, \
                    snippet({table}, 0, '«', '»', '…', 12) AS snip, t.seq \
             FROM {table} JOIN turns t ON t.id = {table}.rowid \
             WHERE {table} MATCH ?{where_extra} {tail}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut binds = vec![match_str.to_string()];
        binds.extend(extra_binds.iter().cloned());
        let it = stmt.query_map(params_from_iter(binds.iter()), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?;
        it.collect()
    };

    let rows = match fetch(&m, &format!("ORDER BY bm25({table}) LIMIT {limit}")) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[subrosa] query error: {e}");
            eprintln!("[subrosa] tip: drop --raw, or wrap special characters in quotes");
            return ExitCode::FAILURE;
        }
    };

    if rows.is_empty() {
        // Nearest-match fallback: a mid-word typo shares most of its trigrams with
        // the stored word even when no 3+-char substring survives. OR each term's
        // trigrams, then keep only rows holding a token within one edit of the term.
        if fuzzy && !raw {
            let toks: Vec<String> = terms
                .iter()
                .flat_map(|t| t.split_whitespace())
                .filter(|t| t.chars().count() >= 3)
                .map(str::to_lowercase)
                .collect();
            // Terms under 5 chars decompose into 1-2 trigrams the substring pass
            // already required, so they stay exact phrases; only ≥5 terms relax.
            let close_idxs: Vec<usize> = toks
                .iter()
                .enumerate()
                .filter(|(_, t)| t.chars().count() >= 5)
                .map(|(i, _)| i)
                .collect();
            if !close_idxs.is_empty() {
                let quote = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
                let tri_group = |t: &str| {
                    let tris: Vec<String> = term_trigrams(t).iter().map(|x| quote(x)).collect();
                    format!("({})", tris.join(" OR "))
                };
                // Does the row's snippet hold a token within one edit of `term`?
                // Strip the «» highlight markers first — they land mid-word
                // («late»ncy) and would split the token being distance-checked.
                let snip_close = |r: &Hit, term: &str| {
                    let s =
                        r.4.as_deref()
                            .unwrap_or("")
                            .replace(['«', '»'], "")
                            .to_lowercase();
                    crate::text::turn_tokens(&s).iter().any(|tok| {
                        crate::text::within_one_edit(tok, term)
                            || tok
                                .split(['-', '_'])
                                .any(|p| crate::text::within_one_edit(p, term))
                    })
                };
                let tail = format!("ORDER BY bm25({table}) LIMIT {FUZZY_CAND_LIMIT}");
                let mut near: Vec<Hit> = Vec::new();
                if any {
                    // --any: one close term suffices, so relax everything in one OR.
                    let groups: Vec<String> = toks
                        .iter()
                        .map(|t| {
                            if t.chars().count() >= 5 {
                                tri_group(t)
                            } else {
                                quote(t)
                            }
                        })
                        .collect();
                    let m2 = wrap_excludes(groups.join(" OR "), exclude, raw, fuzzy);
                    // Best-effort: a fallback query error means no nearest matches,
                    // but say so on stderr — a silent swallow hid a syntax bug once.
                    near = fetch(&m2, &tail)
                        .map_err(|e| eprintln!("[subrosa] fuzzy fallback query error: {e}"))
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|r| close_idxs.iter().any(|&i| snip_close(r, &toks[i])))
                        .collect();
                } else {
                    // AND: assume one typo'd term per query — relax one term at a
                    // time so the others stay hard phrases and keep the candidate
                    // pool tight. Explicit AND: FTS5 rejects implicit AND right
                    // after a parenthesized group.
                    // ponytail: two typo'd terms in one query stay unrescued.
                    for &ri in &close_idxs {
                        let groups: Vec<String> = toks
                            .iter()
                            .enumerate()
                            .map(|(i, t)| if i == ri { tri_group(t) } else { quote(t) })
                            .collect();
                        let m2 = wrap_excludes(groups.join(" AND "), exclude, raw, fuzzy);
                        for r in fetch(&m2, &tail)
                            .map_err(|e| eprintln!("[subrosa] fuzzy fallback query error: {e}"))
                            .unwrap_or_default()
                        {
                            if snip_close(&r, &toks[ri])
                                && !near.iter().any(|n| n.0 == r.0 && n.5 == r.5)
                            {
                                near.push(r);
                            }
                        }
                    }
                }
                near.truncate(limit.max(0) as usize);
                if !near.is_empty() {
                    println!("[subrosa] no substring match — nearest matches (within one edit):");
                    print_results(&conn, &near, context);
                    return ExitCode::SUCCESS;
                }
            }
        }
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
    print_results(&conn, &rows, context);
    ExitCode::SUCCESS
}

/// Candidate pool for the fuzzy nearest-match fallback: bm25-top rows fetched
/// before the Rust-side one-edit filter cuts them down to real near-misses.
const FUZZY_CAND_LIMIT: usize = 50;

/// One search hit: (session_id, ts, role, project, snippet, seq).
type Hit = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    i64,
);

/// Wrap `NOT` clauses for --exclude around a positive match. Non-raw only (raw
/// owns the whole query); `(includes) NOT "x" NOT "y"` keeps grouping correct.
fn wrap_excludes(m: String, exclude: &[String], raw: bool, fuzzy: bool) -> String {
    if raw || exclude.is_empty() || m.trim().is_empty() {
        return m;
    }
    let nots = quote_terms(exclude, fuzzy)
        .iter()
        .map(|e| format!("NOT {e}"))
        .collect::<Vec<_>>()
        .join(" ");
    if nots.is_empty() {
        m
    } else {
        format!("({m}) {nots}")
    }
}

/// Distinct char-level trigrams of an already-lowercased term, capped so a very
/// long identifier can't balloon the fallback OR-query.
fn term_trigrams(term_low: &str) -> Vec<String> {
    const CAP: usize = 24;
    let cs: Vec<char> = term_low.chars().collect();
    let mut out: Vec<String> = Vec::new();
    for w in cs.windows(3) {
        let t: String = w.iter().collect();
        if !out.contains(&t) {
            out.push(t);
            if out.len() == CAP {
                break;
            }
        }
    }
    out
}

/// Print ranked hits with optional --context neighbours, then the footer line.
fn print_results(conn: &rusqlite::Connection, rows: &[Hit], context: i64) {
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
        let sid8 = crate::text::sid8(sid);
        // Relative age after the timestamp, shared with recall (`(7mo old)` etc.);
        // empty when the timestamp is missing or unparseable.
        let age = match ts.as_deref().and_then(timeutil::parse_ts) {
            Some(epoch) => timeutil::age_suffix(now - epoch),
            None => String::new(),
        };
        println!(
            "{:>2}. [{}]{} {} · {} · {}",
            i + 1,
            timeutil::fmt_ts(ts.as_deref().unwrap_or("")),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_trigrams_dedups_and_caps() {
        assert_eq!(
            term_trigrams("latecny"),
            vec!["lat", "ate", "tec", "ecn", "cny"]
        );
        // Repeated windows dedup; the cap bounds very long identifiers.
        assert_eq!(term_trigrams("aaaa"), vec!["aaa"]);
        let long: String = ('a'..='z').cycle().take(60).collect();
        assert!(term_trigrams(&long).len() <= 24);
    }

    #[test]
    fn wrap_excludes_wraps_only_non_raw_non_empty() {
        assert_eq!(wrap_excludes("\"a\"".into(), &[], false, false), "\"a\"");
        assert_eq!(wrap_excludes("q".into(), &["x".into()], true, false), "q");
        assert_eq!(
            wrap_excludes("\"a\"".into(), &["x".into()], false, false),
            "(\"a\") NOT \"x\""
        );
    }

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
