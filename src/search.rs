//! Keyword search over the archived transcripts (FTS5, bm25-ranked).

use std::process::ExitCode;

use rusqlite::{params, params_from_iter};

use crate::{db, embed, paths, redact, timeutil};

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
    semantic: bool,
) -> ExitCode {
    // Negative is meaningless (clap accepts it); treat it as "no context".
    let context = context.max(0);
    if terms.is_empty() {
        eprintln!("[subrosa] give search terms");
        return ExitCode::from(2);
    }
    // These three shape an FTS5 query, which --semantic doesn't build at all.
    if semantic && (fuzzy || raw || any) {
        eprintln!(
            "[subrosa] --semantic ranks by meaning — it can't combine with --fuzzy/--raw/--any"
        );
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
    if semantic {
        return run_semantic(
            terms,
            limit,
            project,
            session,
            after_bound.as_deref(),
            before_bound.as_deref(),
            tags,
            exclude,
            context,
        );
    }
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
    let (where_extra, extra_binds) = turn_filters(
        project,
        session,
        after_bound.as_deref(),
        before_bound.as_deref(),
        tags,
    );

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

/// The turn-level filters (`--project/--session/--after/--before/--tag`) as a
/// WHERE tail plus its binds. `t` is the turns alias on both search paths.
fn turn_filters(
    project: Option<&str>,
    session: Option<&str>,
    after_bound: Option<&str>,
    before_bound: Option<&str>,
    tags: &[String],
) -> (String, Vec<String>) {
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
    if let Some(a) = after_bound {
        where_extra.push_str(" AND t.ts >= ?");
        extra_binds.push(a.to_string());
    }
    if let Some(b) = before_bound {
        where_extra.push_str(" AND t.ts < ?");
        extra_binds.push(b.to_string());
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
    (where_extra, extra_binds)
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

//--- semantic search (opt-in, runs the bundled model locally) ----------------

/// How much of a turn is embedded. Enough for the gist, short enough that a big
/// archive backfills in one sitting; the tokenizer cuts whatever is still over
/// the model's 512-token window.
const EMBED_CHARS: usize = 2000;
/// Turns per forward pass. One, because the parallelism is already one turn per
/// core and a wide batch only makes each core's working set too big for cache.
/// Measured over 8192 real turns on an M3 Max, 14 workers — 1: 186/s at 1.2 GB,
/// 4: 173/s at 3.1 GB, 8: 159/s at 5.3 GB, 16: 139/s at 9.0 GB, 32: 125/s at
/// 13.2 GB. Held at one worker too (1: 22/s, 8: 20/s), so it isn't a threading
/// artefact. Re-measure this before trusting it on a machine with few cores.
const EMBED_BATCH: usize = 1;
/// Turns pulled from the archive per round, newest first. Big enough that the
/// dedupe has something to work with, small enough that stopping halfway still
/// leaves the most recent months searchable.
const EMBED_SLAB: usize = 4096;

/// The one test for "this `turn_embeddings` row counts for `model`", shared by
/// the ranked scan and the pending count so the two can never disagree about
/// which rows exist. `dim` is a usize read from our own column, never input.
fn same_width(dim: Option<usize>) -> String {
    dim.map(|d| format!(" AND e.dim = {d}")).unwrap_or_default()
}

/// Turns still needing a vector for `model`: none stored, or one stored at a
/// different width — a stale or corrupt row, which re-embedding replaces. The
/// backfill's work queue, and naturally resumable: an interrupted run just
/// finds fewer rows next time.
fn pending_sql(dim: Option<usize>) -> String {
    format!(
        "FROM turns t WHERE t.text IS NOT NULL AND t.text <> '' \
         AND NOT EXISTS (SELECT 1 FROM turn_embeddings e \
         WHERE e.turn_id = t.id AND e.model = ?1{})",
        same_width(dim)
    )
}

/// The rows `--semantic` ranks, under the same filters and the same width test
/// the pending count uses — a row the scan ranks must be one the count doesn't
/// call pending, or "N of M" double-counts it.
fn scan_sql(dim: usize, tail: &str) -> String {
    format!(
        "SELECT e.turn_id, e.vec FROM turn_embeddings e JOIN turns t ON t.id = e.turn_id \
         WHERE e.model = ?{}{tail} ORDER BY e.turn_id",
        same_width(Some(dim))
    )
}

/// The backfill's work queue is built against one stored width. If that width
/// moved while it ran (a concurrent `--rebuild`), rows written now would never
/// satisfy the old queue and the same batch would re-embed forever.
fn width_moved(queued: Option<usize>, seen: Option<usize>) -> bool {
    matches!((queued, seen), (Some(a), Some(b)) if a != b)
}

/// Exactly what the model is shown for a query. Stored turns were redacted at
/// ingest; the query is live user input, so it gets masked here — and bge's
/// query-side instruction goes on the front, which stored passages never carry.
fn query_text(terms: &[String]) -> String {
    format!(
        "{}{}",
        embed::QUERY_PREFIX,
        redact::redact(&terms.join(" "))
    )
}

/// One stored row's score against the query, or `None` when the row can't be
/// trusted: a blob that isn't `dim` wide, or a cosine outside the [-1, 1] two
/// unit vectors can reach (NaN, infinity, huge values). Any of those would sort
/// above every real hit, so they're dropped and counted rather than ranked.
fn score_row(qvec: &[f32], blob: &[u8], dim: usize) -> Option<f64> {
    if blob.len() != dim * 4 {
        return None;
    }
    let score = embed::cosine(qvec, &decode_vec(blob));
    (-1.001..=1.001).contains(&score).then_some(score)
}

/// Vectors are stored little-endian f32, L2-normalized at write time.
fn encode_vec(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn decode_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// The one dimension every vector for `model` must have, set by the first one
/// ever stored. `None` means nothing is stored for it yet.
fn model_dim(conn: &rusqlite::Connection, model: &str) -> Option<usize> {
    conn.query_row(
        "SELECT dim FROM turn_embeddings WHERE model = ?1 LIMIT 1",
        [model],
        |r| r.get::<_, i64>(0),
    )
    .ok()
    .filter(|d| *d > 0)
    .map(|d| d as usize)
}

/// One slab, ready to embed: every distinct truncated text once, carrying the
/// turn ids that share it. A fifth of a real archive is repeats — "Updated task
/// #N status" alone runs into the hundreds — and each is now embedded once.
/// If EMBED_BATCH is ever raised above 1, sort these by length before batching:
/// a batch pads out to its longest member, and database order wasted 2.5x.
fn group_slab(slab: Vec<(i64, String)>) -> Vec<(String, Vec<i64>)> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut groups: Vec<(String, Vec<i64>)> = Vec::new();
    for (id, text) in slab {
        // Stored passages go in bare — only the query side carries bge's prefix.
        let text: String = text.chars().take(EMBED_CHARS).collect();
        match seen.get(&text) {
            Some(&at) => groups[at].1.push(id),
            None => {
                seen.insert(text.clone(), groups.len());
                groups.push((text, vec![id]));
            }
        }
    }
    groups
}

/// Every worker shares one loaded model — `embed` takes `&self` and the weights
/// are a read-only mmap. A candle release that stops guaranteeing that fails
/// here, at compile time, instead of needing N copies of the weights.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<embed::Embedder>();
};

/// Every distinct text in the slab, embedded in order across `workers` threads.
/// Batches are claimed by index and results come back over a channel, so the
/// caller stays the only thing that touches the database.
fn embed_texts(
    embedder: &embed::Embedder,
    texts: &[String],
    workers: usize,
) -> Result<Vec<Vec<f32>>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let batches: Vec<&[String]> = texts.chunks(EMBED_BATCH).collect();
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, Vec<Vec<f32>>)> = Vec::with_capacity(batches.len());
    let sent = std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..workers.min(batches.len()) {
            let (tx, next, batches) = (tx.clone(), &next, &batches);
            scope.spawn(move || {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(batch) = batches.get(at) else { break };
                    // A batch that comes back short would slide every later
                    // vector onto the wrong turn, so it's caught per batch.
                    let out = embedder.embed(batch).and_then(|v| {
                        (v.len() == batch.len()).then_some(v).ok_or_else(|| {
                            format!(
                                "the model returned a short batch for {} input(s)",
                                batch.len()
                            )
                        })
                    });
                    let failed = out.is_err();
                    if tx.send((at, out)).is_err() || failed {
                        break;
                    }
                }
            });
        }
        drop(tx);
        rx.iter().collect::<Vec<_>>()
    });
    for (at, batch) in sent {
        results.push((at, batch?));
    }
    results.sort_by_key(|(at, _)| *at);
    let vecs: Vec<Vec<f32>> = results.into_iter().flat_map(|(_, v)| v).collect();
    if vecs.len() != texts.len() {
        return Err(format!(
            "the model returned {} vector(s) for {} input(s)",
            vecs.len(),
            texts.len()
        ));
    }
    Ok(vecs)
}

/// Roughly how much longer, for the progress line only. Clamped because an
/// infinite estimate casts to `u64::MAX` and prints as a nonsense age.
fn eta(secs: f64) -> String {
    match secs.clamp(0.0, 359_999.0) as u64 {
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", (s + 30) / 60),
        s => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

/// `subrosa embed`: precompute a vector per archived turn. Never runs from a
/// hook — this is the one command that loads the model and downloads it if
/// it isn't there yet.
pub fn embed_backfill(rebuild: bool) -> ExitCode {
    // Rows are keyed by model AND revision; the plain name is for reading.
    let (key, model) = (embed::MODEL_KEY, embed::MODEL_NAME);
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = db::ensure_embeddings_table(&conn) {
        eprintln!("[subrosa] cannot create the embeddings store: {e}");
        return ExitCode::FAILURE;
    }
    // Vectors from a model we no longer ship are a different space: nothing can
    // rank them, nothing can resume from them, and they're the biggest thing in
    // the archive. Their pages go back to the free list for the new ones.
    match conn.execute("DELETE FROM turn_embeddings WHERE model <> ?1", [key]) {
        Ok(n) if n > 0 => {
            eprintln!("[subrosa] embed: deleted {n} vector(s) left by an older model")
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("[subrosa] cannot clear the older model's vectors: {e}");
            return ExitCode::FAILURE;
        }
    }
    // --rebuild is the repair for vectors that went bad on disk: a row of the
    // right width holding garbage still looks complete to the work queue.
    if rebuild {
        match conn.execute("DELETE FROM turn_embeddings WHERE model = ?1", [key]) {
            Ok(n) => println!("[subrosa] embed: cleared {n} stored vector(s) for {model}"),
            Err(e) => {
                eprintln!("[subrosa] cannot clear the stored vectors: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    // One width per model, fixed by the first vector ever stored for it. Rows at
    // any other width are stale and get re-embedded rather than skipped.
    let dim = model_dim(&conn, key);
    let pending = pending_sql(dim);
    let total: i64 =
        match conn.query_row(&format!("SELECT count(*) {pending}"), [key], |r| r.get(0)) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("[subrosa] query error: {e}");
                return ExitCode::FAILURE;
            }
        };
    if total == 0 {
        println!("[subrosa] embed: every turn is already embedded with {model}");
        return ExitCode::SUCCESS;
    }
    // Loaded only once there's work: this is where the one-time download
    // happens, and a no-op backfill shouldn't trigger it.
    let embedder = match embed::Embedder::load() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[subrosa] {e}");
            return ExitCode::FAILURE;
        }
    };
    // One thread per core, all sharing that model. Inference is the whole cost
    // of this command, and one forward pass at a time leaves every other core
    // idle — this is where nearly all of the wall clock went.
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    let started = std::time::Instant::now();
    let mut done = 0i64;
    loop {
        // Newest first: an archive this size takes minutes, and recent work is
        // what a search asked in those minutes is looking for.
        let slab: Vec<(i64, String)> = match conn
            .prepare(&format!(
                "SELECT t.id, t.text {pending} ORDER BY t.id DESC LIMIT {EMBED_SLAB}"
            ))
            .and_then(|mut s| {
                s.query_map([key], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
                    .collect()
            }) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[subrosa] query error: {e}");
                return ExitCode::FAILURE;
            }
        };
        if slab.is_empty() {
            break;
        }
        let (inputs, turns): (Vec<String>, Vec<Vec<i64>>) = group_slab(slab).into_iter().unzip();
        // A hard stop, never a silent fall back to keyword. What was stored so
        // far persists, so re-running picks up where this left off.
        let vecs = match embed_texts(&embedder, &inputs, workers) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[subrosa] {e}");
                return ExitCode::FAILURE;
            }
        };
        // Embedder::embed already rejected non-finite vectors, so width is all
        // that's left — and it's checked against the stored value INSIDE the
        // write transaction, where a value read earlier could be stale.
        let stored = (|| -> Result<(), String> {
            let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
            let seen = model_dim(&tx, key);
            // Something else re-embedded this model at a different width while we
            // ran. Rows written now would never satisfy the queue we built, so the
            // same batch would come back round forever — stop instead.
            if width_moved(dim, seen) {
                return Err(format!(
                    "'{model}' width changed from {} to {} under this backfill — \
                     re-run: subrosa embed",
                    dim.unwrap_or(0),
                    seen.unwrap_or(0)
                ));
            }
            let want = seen.unwrap_or(vecs[0].len());
            if let Some(bad) = vecs.iter().find(|v| v.len() != want) {
                return Err(format!(
                    "'{model}' returned a {}-value vector but this archive stores {want} — \
                     refusing to mix dimensions. Run `subrosa embed --rebuild` to replace \
                     what's stored.",
                    bad.len()
                ));
            }
            // One vector, then a row per turn that shares its text.
            for (ids, mut v) in turns.iter().zip(vecs) {
                embed::normalize(&mut v);
                let blob = encode_vec(&v);
                for id in ids {
                    tx.execute(
                        "INSERT OR REPLACE INTO turn_embeddings(turn_id, model, dim, vec) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![id, key, v.len() as i64, &blob],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            tx.commit().map_err(|e| e.to_string())
        })();
        if let Err(e) = stored {
            eprintln!("[subrosa] {e}");
            return ExitCode::FAILURE;
        }
        done += turns.iter().map(|ids| ids.len() as i64).sum::<i64>();
        // The total is a snapshot; a live session ingesting mid-run can push the
        // real work past it, so the denominator grows rather than reading 70/64.
        let total = total.max(done);
        let rate = done as f64 / started.elapsed().as_secs_f64();
        eprintln!(
            "[subrosa] embedded {done}/{total} · {rate:.0}/s · ~{} left",
            eta((total - done) as f64 / rate)
        );
    }
    println!("[subrosa] embed: {done} turn(s) embedded with {model}");
    ExitCode::SUCCESS
}

/// `search --semantic`: rank stored vectors against the query's, so a turn can
/// surface on meaning alone without sharing a word with the query.
#[allow(clippy::too_many_arguments)]
fn run_semantic(
    terms: &[String],
    limit: i64,
    project: Option<&str>,
    session: Option<&str>,
    after_bound: Option<&str>,
    before_bound: Option<&str>,
    tags: &[String],
    exclude: &[String],
    context: i64,
) -> ExitCode {
    // Rows are keyed by model AND revision; the plain name is for reading.
    let (key, model) = (embed::MODEL_KEY, embed::MODEL_NAME);
    let conn = match db::connect_readonly() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    // A missing table reads the same as an empty one — both mean "not backfilled".
    let Some(dim) = model_dim(&conn, key) else {
        println!("[subrosa] no embeddings yet — run: subrosa embed");
        return ExitCode::SUCCESS;
    };
    let qvec = match embed::Embedder::load().and_then(|e| e.embed(&[query_text(terms)])) {
        Ok(mut v) if !v.is_empty() => {
            let mut q = v.swap_remove(0);
            embed::normalize(&mut q);
            q
        }
        Ok(_) => {
            eprintln!("[subrosa] the model returned no vector for the query");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("[subrosa] {e}");
            return ExitCode::FAILURE;
        }
    };

    if qvec.len() != dim {
        eprintln!(
            "[subrosa] '{model}' now returns {}-value vectors but this archive stores {dim} — \
             the stored index is stale. Re-run: subrosa embed",
            qvec.len()
        );
        return ExitCode::FAILURE;
    }

    // One filter tail for both the ranked scan and the pending-turn count, so
    // "N not yet embedded" is always counted over the same set being searched.
    let (mut tail, mut filter_binds) =
        turn_filters(project, session, after_bound, before_bound, tags);
    // There's no MATCH here to hang a NOT on, so --exclude drops the candidates
    // the keyword index says hold one of the terms.
    let nots = quote_terms(exclude, false).join(" OR ");
    if !nots.is_empty() {
        tail.push_str(" AND t.id NOT IN (SELECT rowid FROM turns_fts WHERE turns_fts MATCH ?)");
        filter_binds.push(nots);
    }
    let mut binds: Vec<String> = vec![key.to_string()];
    binds.extend(filter_binds);

    // ponytail: brute-force scan of every candidate vector, no index. Linear in
    // archive size and fine at tens of thousands of turns; an ANN index is the
    // upgrade if that stops holding.
    let mut scored: Vec<(f64, i64)> = Vec::new();
    let mut corrupt = 0usize;
    let sql = scan_sql(dim, &tail);
    // One read transaction over the scan AND the pending count, so a backfill
    // running alongside can't leave the "N of M" warning quoting two snapshots.
    let scan = conn.unchecked_transaction().and_then(|tx| {
        {
            let mut s = tx.prepare(&sql)?;
            let mut rows = s.query(params_from_iter(binds.iter()))?;
            while let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                let blob: Vec<u8> = r.get(1)?;
                match score_row(&qvec, &blob, dim) {
                    Some(score) => scored.push((score, id)),
                    None => corrupt += 1,
                }
            }
        }
        tx.query_row(
            &format!("SELECT count(*) {}{tail}", pending_sql(Some(dim))),
            params_from_iter(binds.iter()),
            |r| r.get::<_, i64>(0),
        )
    });
    let pending = match scan {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[subrosa] query error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if corrupt > 0 {
        eprintln!(
            "[subrosa] skipped {corrupt} unreadable embedding(s) — repair with: subrosa embed --rebuild"
        );
    }
    let ranked = scored.len();
    if scored.is_empty() {
        println!("[subrosa] no matches for: {}", terms.join(" "));
        // Still warn: filters that select only un-embedded turns give an empty
        // result that looks like "nothing here" instead of "nothing indexed".
        warn_incomplete(pending, ranked + corrupt);
        return ExitCode::SUCCESS;
    }
    // Stable sort: equal scores keep turn-id order, so results are reproducible.
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    // SQLite reads a negative LIMIT as "no limit" — the keyword path relies on
    // that, so -n -1 has to mean the same thing here.
    if limit >= 0 {
        scored.truncate(limit as usize);
    }

    // The keyword path shows an FTS snippet; with no match terms, a plain
    // preview of the turn takes that slot.
    let rows: Vec<Hit> = match conn
        .prepare("SELECT session_id, ts, role, project, text, seq FROM turns WHERE id = ?")
    {
        Ok(mut stmt) => scored
            .iter()
            .filter_map(|(_, id)| {
                stmt.query_row([id], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get::<_, Option<String>>(4)?.as_deref().map(ctx_preview),
                        r.get(5)?,
                    ))
                })
                .ok()
            })
            .collect(),
        Err(e) => {
            eprintln!("[subrosa] query error: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Stdout stays the shared result format; the how-it-ranked note is stderr.
    eprintln!("[subrosa] semantic: ranked {ranked} turns via {model}");
    print_results(&conn, &rows, context);
    warn_incomplete(pending, ranked + corrupt);
    ExitCode::SUCCESS
}

/// Only embedded turns can be ranked, so a backfill that stopped early — or
/// turns archived since it ran — drop out of what reads as a whole-archive
/// search. Warn loudly and still show what we have: refusing outright over one
/// new turn would be worse. `searched` is the same filtered set the scan saw.
fn warn_incomplete(pending: i64, searched: usize) {
    if pending <= 0 {
        return;
    }
    let total = searched as i64 + pending;
    eprintln!(
        "[subrosa] ACTION REQUIRED — INCOMPLETE INDEX: {pending} of {total} matching turns \
         have no embedding and were NOT searched. The best match may be missing."
    );
    eprintln!("[subrosa] Run `subrosa embed` to finish the index, then search again.");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every turn keeps its own id and repeats collapse onto one text, so the
    /// vector a turn gets is still the vector for the text it holds. Losing an
    /// id here would leave that turn silently unsearchable; putting it in the
    /// wrong group would give it another turn's meaning.
    #[test]
    fn grouping_keeps_every_turn_and_embeds_a_repeat_once() {
        let slab = vec![
            (1, "a longer line about a failover".to_string()),
            (2, "short".to_string()),
            (3, "a longer line about a failover".to_string()),
            (4, "mid length line".to_string()),
        ];
        let groups = group_slab(slab);
        assert_eq!(
            groups,
            vec![
                ("a longer line about a failover".to_string(), vec![1, 3]),
                ("short".to_string(), vec![2]),
                ("mid length line".to_string(), vec![4]),
            ],
            "the repeat carries both turn ids"
        );
        // Only what's over EMBED_CHARS is cut, and two turns that differ only
        // past the cut are one text — they'd embed identically anyway.
        let long = "x".repeat(EMBED_CHARS + 50);
        let groups = group_slab(vec![(1, format!("{long}aaa")), (2, format!("{long}bbb"))]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0.chars().count(), EMBED_CHARS);
        assert_eq!(groups[0].1, vec![1, 2]);
    }

    /// The progress line's only job is to be readable at a glance.
    #[test]
    fn eta_reads_in_the_unit_that_fits() {
        assert_eq!(eta(45.0), "45s");
        assert_eq!(eta(600.0), "10m");
        assert_eq!(eta(7200.0), "2h0m");
        // A rate of zero divides to infinity, which casts to u64::MAX.
        assert_eq!(eta(f64::INFINITY), "99h59m");
        assert_eq!(eta(f64::NAN), "0s");
    }

    #[test]
    fn vec_blob_roundtrips_little_endian_f32() {
        let v = vec![0.5f32, -0.25, 1.0];
        assert_eq!(decode_vec(&encode_vec(&v)), v);
        assert_eq!(encode_vec(&v).len(), 12);
    }

    /// The query is the one piece of live user input that reaches the model, so
    /// a secret in it must be masked before it goes — and the query prefix must
    /// be there, or ranking quality drops without anything looking wrong.
    #[test]
    fn the_query_is_masked_and_prefixed_before_the_model_sees_it() {
        let out = query_text(&["password=hunter2".into(), "failover".into()]);
        assert_eq!(
            out,
            "Represent this sentence for searching relevant passages: \
             password=‹redacted› failover"
        );
        assert!(!out.contains("hunter2"));
        assert!(out.starts_with(embed::QUERY_PREFIX));
    }

    /// A row we can't trust must be dropped, never scored — every one of these
    /// would otherwise sort above the real hits.
    #[test]
    fn score_row_drops_rows_it_cannot_trust() {
        let q = [1.0f32, 0.0, 0.0];
        assert_eq!(score_row(&q, &encode_vec(&[1.0, 0.0, 0.0]), 3), Some(1.0));
        assert_eq!(score_row(&q, &encode_vec(&[-1.0, 0.0, 0.0]), 3), Some(-1.0));
        // A blob that isn't the width its row claims.
        assert_eq!(score_row(&q, &encode_vec(&[1.0, 0.0]), 3), None);
        // Values that went bad on disk: NaN and a magnitude no unit vector has.
        assert_eq!(score_row(&q, &encode_vec(&[f32::NAN, 0.0, 0.0]), 3), None);
        assert_eq!(score_row(&q, &encode_vec(&[f32::MAX, 0.0, 0.0]), 3), None);
    }

    /// A three-turn archive: one embedded at the model's width, one embedded at
    /// a stale width, one never embedded at all.
    fn seeded() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE turns (id INTEGER PRIMARY KEY, project TEXT, session_id TEXT, \
               ts TEXT, text TEXT, seq INTEGER);\
             CREATE TABLE turn_embeddings (turn_id INTEGER, model TEXT, dim INTEGER, \
               vec BLOB, PRIMARY KEY (turn_id, model));\
             INSERT INTO turns VALUES (1,'proj-a','s',NULL,'failover',0),\
                                      (2,'proj-a','s',NULL,'pod drained',1),\
                                      (3,'proj-b','s',NULL,'invoice totals',2);\
             INSERT INTO turn_embeddings VALUES (1,'m',2,x'0000803f00000000'),\
                                               (2,'m',1,x'0000803f');",
        )
        .unwrap();
        conn
    }

    /// What `--semantic` ranks and what it calls "not yet embedded" have to add
    /// up to the whole filtered archive: a stale-width row belongs to the
    /// pending side only, and a turn with no vector at all belongs there too.
    /// Between them that's the "N of M matching turns" the warning quotes.
    #[test]
    fn every_turn_is_either_ranked_or_pending_never_both() {
        let conn = seeded();
        let count = |sql: &str, binds: &[String]| -> i64 {
            conn.query_row(sql, params_from_iter(binds.iter()), |r| r.get(0))
                .unwrap()
        };
        let (tail, filter_binds) = turn_filters(None, None, None, None, &[]);
        let binds: Vec<String> = std::iter::once("m".to_string())
            .chain(filter_binds)
            .collect();
        let ranked = count(
            &format!("SELECT count(*) FROM ({})", scan_sql(2, &tail)),
            &binds,
        );
        let pending = count(
            &format!("SELECT count(*) {}{tail}", pending_sql(Some(2))),
            &binds,
        );
        // Turn 1 ranks; turns 2 (stale width) and 3 (no vector) are pending.
        assert_eq!((ranked, pending), (1, 2));
        assert_eq!(ranked + pending, 3, "a turn was counted twice or lost");
    }

    /// Filters narrow both sides the same way. A filter that selects only
    /// un-embedded turns gives an empty result, which reads as "nothing here"
    /// unless the pending count still says what wasn't searched.
    #[test]
    fn filters_narrow_the_scan_and_the_pending_count_together() {
        let conn = seeded();
        let (tail, filter_binds) = turn_filters(Some("proj-b"), None, None, None, &[]);
        let binds: Vec<String> = std::iter::once("m".to_string())
            .chain(filter_binds)
            .collect();
        let count = |sql: String| -> i64 {
            conn.query_row(&sql, params_from_iter(binds.iter()), |r| r.get(0))
                .unwrap()
        };
        let ranked = count(format!("SELECT count(*) FROM ({})", scan_sql(2, &tail)));
        let pending = count(format!("SELECT count(*) {}{tail}", pending_sql(Some(2))));
        assert_eq!((ranked, pending), (0, 1), "only turn 3 is in proj-b");
    }

    /// The scan and the pending count have to test width the same way, or a
    /// stale row lands in both and "N of M" counts it twice.
    #[test]
    fn the_width_test_is_one_fragment_shared_by_scan_and_count() {
        assert_eq!(same_width(Some(768)), " AND e.dim = 768");
        assert_eq!(same_width(None), "");
        assert!(pending_sql(Some(768)).ends_with(" AND e.dim = 768)"));
        assert!(!pending_sql(None).contains("e.dim"));
    }

    /// Only a width that was known before AND moved is a stale queue; adopting
    /// a width the run started without is normal.
    #[test]
    fn width_moved_needs_two_known_and_different_widths() {
        assert!(width_moved(Some(3), Some(2)));
        assert!(!width_moved(Some(3), Some(3)));
        assert!(!width_moved(None, Some(3)));
        assert!(!width_moved(Some(3), None));
        assert!(!width_moved(None, None));
    }

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
