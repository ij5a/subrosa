//! Keyword search over the archived transcripts (FTS5, bm25-ranked).

use std::process::ExitCode;
use std::time::Duration;

use rusqlite::{params, params_from_iter};

use crate::{db, ollama, paths, redact, timeutil};

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

//--- semantic search (opt-in, needs a local Ollama) --------------------------

/// How much of a turn is sent to the embedding model. Enough for the gist,
/// short enough that a big archive backfills in one sitting.
const EMBED_CHARS: usize = 2000;
/// Turns per `/api/embed` request.
const EMBED_BATCH: usize = 64;
/// Localhost refuses instantly when nothing listens, so the connect leash is
/// short; the first request after a cold start waits on the model loading.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

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

/// The backfill's work queue is built against one stored width. If that width
/// moved while it ran (a concurrent `--rebuild`), rows written now would never
/// satisfy the old queue and the same batch would re-embed forever.
fn width_moved(queued: Option<usize>, seen: Option<usize>) -> bool {
    matches!((queued, seen), (Some(a), Some(b)) if a != b)
}

/// nomic-embed-text is trained asymmetrically — stored text and queries carry
/// different task prefixes, and dropping them costs real ranking quality.
/// ponytail: nomic is the default and the only special case here; another model
/// with its own scheme needs its own arm.
fn prefixed(model: &str, text: &str, query: bool) -> String {
    if model.starts_with("nomic") {
        let tag = if query {
            "search_query: "
        } else {
            "search_document: "
        };
        format!("{tag}{text}")
    } else {
        text.to_string()
    }
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

/// `subrosa embed`: precompute a vector per archived turn. Never runs from a
/// hook — this is the one command that needs Ollama up.
pub fn embed_backfill(rebuild: bool) -> ExitCode {
    let (host, model) = (paths::ollama_host(), paths::embed_model());
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
    // --rebuild is the repair for vectors that went bad on disk: a row of the
    // right width holding garbage still looks complete to the work queue.
    if rebuild {
        match conn.execute("DELETE FROM turn_embeddings WHERE model = ?1", [&model]) {
            Ok(n) => println!("[subrosa] embed: cleared {n} stored vector(s) for {model}"),
            Err(e) => {
                eprintln!("[subrosa] cannot clear the stored vectors: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    // One width per model, fixed by the first vector ever stored for it. Rows at
    // any other width are stale and get re-embedded rather than skipped.
    let dim = model_dim(&conn, &model);
    let pending = pending_sql(dim);
    let total: i64 = match conn.query_row(&format!("SELECT count(*) {pending}"), [&model], |r| {
        r.get(0)
    }) {
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
    let mut done = 0i64;
    loop {
        let batch: Vec<(i64, String)> = match conn
            .prepare(&format!(
                "SELECT t.id, t.text {pending} ORDER BY t.id LIMIT {EMBED_BATCH}"
            ))
            .and_then(|mut s| {
                s.query_map([&model], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
                    .collect()
            }) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[subrosa] query error: {e}");
                return ExitCode::FAILURE;
            }
        };
        if batch.is_empty() {
            break;
        }
        let inputs: Vec<String> = batch
            .iter()
            .map(|(_, text)| {
                prefixed(
                    &model,
                    &text.chars().take(EMBED_CHARS).collect::<String>(),
                    false,
                )
            })
            .collect();
        let vecs = match ollama::embed(&host, &model, &inputs, CONNECT_TIMEOUT, IO_TIMEOUT) {
            Ok(v) if v.len() == inputs.len() => v,
            Ok(v) => {
                eprintln!(
                    "[subrosa] Ollama returned {} vector(s) for {} input(s)",
                    v.len(),
                    inputs.len()
                );
                return ExitCode::FAILURE;
            }
            // A hard stop, never a silent fall back to keyword. What was stored
            // so far persists, so re-running picks up where this left off.
            Err(e) => {
                eprintln!("[subrosa] {e}");
                return ExitCode::FAILURE;
            }
        };
        // ollama::embed already rejected empty and non-finite vectors, so width
        // is all that's left — and it's checked against the stored value INSIDE
        // the write transaction, where a value read earlier could be stale.
        let stored = (|| -> Result<(), String> {
            let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
            let seen = model_dim(&tx, &model);
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
                     refusing to mix dimensions. Use a different SUBROSA_EMBED_MODEL name for a \
                     different model, or `subrosa embed --rebuild` to replace what's stored.",
                    bad.len()
                ));
            }
            for ((id, _), mut v) in batch.iter().zip(vecs) {
                ollama::normalize(&mut v);
                tx.execute(
                    "INSERT OR REPLACE INTO turn_embeddings(turn_id, model, dim, vec) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![id, &model, v.len() as i64, encode_vec(&v)],
                )
                .map_err(|e| e.to_string())?;
            }
            tx.commit().map_err(|e| e.to_string())
        })();
        if let Err(e) = stored {
            eprintln!("[subrosa] {e}");
            return ExitCode::FAILURE;
        }
        done += batch.len() as i64;
        // The total is a snapshot; a live session ingesting mid-run can push the
        // real work past it, so the denominator grows rather than reading 70/64.
        eprintln!("[subrosa] embedded {done}/{}", total.max(done));
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
    let model = paths::embed_model();
    let conn = match db::connect_readonly() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    // A missing table reads the same as an empty one — both mean "not backfilled".
    let Some(dim) = model_dim(&conn, &model) else {
        println!("[subrosa] no embeddings yet — run: subrosa embed");
        return ExitCode::SUCCESS;
    };
    // Stored turns were redacted at ingest; the query is live user input, so it
    // gets masked before it reaches the model.
    let query = redact::redact(&terms.join(" ")).into_owned();
    let host = paths::ollama_host();
    let qvec = match ollama::embed(
        &host,
        &model,
        &[prefixed(&model, &query, true)],
        CONNECT_TIMEOUT,
        IO_TIMEOUT,
    ) {
        Ok(mut v) if !v.is_empty() => {
            let mut q = v.swap_remove(0);
            ollama::normalize(&mut q);
            q
        }
        Ok(_) => {
            eprintln!("[subrosa] Ollama returned no vector for the query");
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
    let mut binds: Vec<String> = vec![model.clone()];
    binds.extend(filter_binds);

    // ponytail: brute-force scan of every candidate vector, no index. Linear in
    // archive size and fine at tens of thousands of turns; an ANN index is the
    // upgrade if that stops holding.
    let mut scored: Vec<(f64, i64)> = Vec::new();
    let mut corrupt = 0usize;
    // Same width test as the pending count below: a row the scan ranks must be
    // one the count doesn't call pending, or "N of M" double-counts it.
    let sql = format!(
        "SELECT e.turn_id, e.vec FROM turn_embeddings e JOIN turns t ON t.id = e.turn_id \
         WHERE e.model = ?{}{tail} ORDER BY e.turn_id",
        same_width(Some(dim))
    );
    // One read transaction over the scan AND the pending count, so a backfill
    // running alongside can't leave the "N of M" warning quoting two snapshots.
    let scan = conn.unchecked_transaction().and_then(|tx| {
        {
            let mut s = tx.prepare(&sql)?;
            let mut rows = s.query(params_from_iter(binds.iter()))?;
            while let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                let blob: Vec<u8> = r.get(1)?;
                // The row says it's `dim` wide; a blob that isn't is corrupt.
                if blob.len() != dim * 4 {
                    corrupt += 1;
                    continue;
                }
                // Stored and query vectors are unit-normalized, so a real cosine
                // lands in [-1, 1] give or take float slop. NaN, infinity or a
                // huge score all mean a corrupt row — and any of them would sort
                // above every real hit, so drop it rather than rank it.
                let score = ollama::cosine(&qvec, &decode_vec(&blob));
                if !(-1.001..=1.001).contains(&score) {
                    corrupt += 1;
                    continue;
                }
                scored.push((score, id));
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

    #[test]
    fn vec_blob_roundtrips_little_endian_f32() {
        let v = vec![0.5f32, -0.25, 1.0];
        assert_eq!(decode_vec(&encode_vec(&v)), v);
        assert_eq!(encode_vec(&v).len(), 12);
    }

    #[test]
    fn nomic_gets_asymmetric_prefixes_and_other_models_dont() {
        assert_eq!(
            prefixed("nomic-embed-text", "hi", false),
            "search_document: hi"
        );
        assert_eq!(prefixed("nomic-embed-text", "hi", true), "search_query: hi");
        assert_eq!(prefixed("mxbai-embed-large", "hi", true), "hi");
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
