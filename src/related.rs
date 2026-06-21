//! `subrosa related <identifier>` — co-occurrence over the archive. Read-only.
//! FTS-match the anchor, collect the sessions it appears in, tokenize those
//! sessions in-process (the SQL `fts5vocab` instance self-join was 29.8s/50k —
//! dead), and rank the distinctive terms that show up alongside it, then the
//! sessions those terms came from. Pure archaeology: "what clustered around X".

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;

use rusqlite::params_from_iter;

use crate::{db, paths, search, text, timeutil};

// Bound the co-occurrence so a very common anchor stays millisecond-scale: only
// the most recent CAP anchor-sessions are tokenized. The header says when it bit.
const SESSION_SCAN_CAP: usize = 300;
// A term must co-occur with the anchor in at least this many sessions to rank,
// which drops the long tail of words that share a single session by chance.
const MIN_SESSIONS: u32 = 2;
// Cap on how many candidate terms get a doc-frequency probe. A 100+-session
// anchor yields tens of thousands of co-occurring terms; scoring only the
// best-supported keeps the verb sub-second. Calibrated against the ~53k-turn
// archive — the dropped tail is all count-2 noise (the footer reports the total).
const MAX_SCORED_TERMS: usize = 1500;

/// One anchor-matching session: the first matching turn's metadata + snippet,
/// used for the sessions list and the recency tie-break.
struct SessionHit {
    sid: String,
    ts: Option<String>,
    role: String,
    project: Option<String>,
    snippet: Option<String>,
}

pub fn run(identifier: &str, limit: i64, project: Option<&str>, sessions: i64) -> ExitCode {
    let id = identifier.trim();
    if id.is_empty() {
        eprintln!("[subrosa] give an identifier to relate");
        return ExitCode::from(2);
    }
    // No archive yet, or locked — same quiet posture as a no-match.
    let Ok(conn) = db::connect_readonly() else {
        println!("[subrosa] no sessions mention \"{id}\"");
        return ExitCode::SUCCESS;
    };

    // Q1: anchor-matching turns → one representative row per session (first by
    // id), in id order. turns_fts is a fixed literal; MATCH/project are bound.
    let mut sql = String::from(
        "SELECT t.session_id, t.ts, t.role, t.project, snippet(turns_fts, 0, '«', '»', '…', 12) \
         FROM turns_fts JOIN turns t ON t.id = turns_fts.rowid \
         WHERE turns_fts MATCH ?",
    );
    let mut binds: Vec<String> = vec![search::build_match(&[id.to_string()], false, false)];
    if let Some(p) = project {
        sql.push_str(" AND t.project LIKE ?");
        binds.push(format!("%{p}%"));
    }
    sql.push_str(" ORDER BY t.id");

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => return query_error(e),
    };
    type Row = (
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    );
    let rows: Result<Vec<Row>, _> = stmt
        .query_map(params_from_iter(binds.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .and_then(|it| it.collect());
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return query_error(e),
    };

    // Collapse to one hit per session (first matching turn), first-seen order.
    let mut hits: Vec<SessionHit> = Vec::new();
    let mut seen = HashSet::new();
    for (sid, ts, role, project, snippet) in rows {
        if seen.insert(sid.clone()) {
            hits.push(SessionHit {
                sid,
                ts,
                role,
                project,
                snippet,
            });
        }
    }
    if hits.is_empty() {
        println!("[subrosa] no sessions mention \"{id}\"");
        return ExitCode::SUCCESS;
    }
    let total = hits.len();

    // Cap to the most recent SESSION_SCAN_CAP before tokenizing (ts desc; a
    // missing ts sorts last). No silent truncation — the header reports it.
    if hits.len() > SESSION_SCAN_CAP {
        hits.sort_by(|a, b| b.ts.cmp(&a.ts));
        hits.truncate(SESSION_SCAN_CAP);
    }
    let scanned = hits.len();
    let capped = total > scanned;
    let sids: Vec<String> = hits.iter().map(|h| h.sid.clone()).collect();

    // The anchor itself (and its sub-tokens) is excluded from co-occurrence.
    let anchor_terms: Vec<String> = text::extract_terms(id)
        .iter()
        .map(|t| t.to_lowercase())
        .collect();

    // Q2: every non-noise turn for the scanned sessions → per-session term sets
    // + a lowercased-key → first-seen-casing display map.
    let placeholders = sids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let q2 = format!(
        "SELECT session_id, text FROM turns \
         WHERE session_id IN ({placeholders}) AND is_meta = 0 AND is_sidechain = 0 \
         ORDER BY id"
    );
    let mut session_terms: HashMap<String, HashSet<String>> = HashMap::new();
    let mut display: HashMap<String, String> = HashMap::new();
    {
        let mut stmt2 = match conn.prepare(&q2) {
            Ok(s) => s,
            Err(e) => return query_error(e),
        };
        let mut q = match stmt2.query(params_from_iter(sids.iter())) {
            Ok(q) => q,
            Err(e) => return query_error(e),
        };
        loop {
            match q.next() {
                Ok(Some(row)) => {
                    let Ok(sid) = row.get::<_, String>(0) else {
                        continue;
                    };
                    let text_s: String = row
                        .get::<_, Option<String>>(1)
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    let set = session_terms.entry(sid).or_default();
                    for (low, term) in cooccurring_terms(&text_s, &anchor_terms) {
                        display.entry(low.clone()).or_insert(term);
                        set.insert(low);
                    }
                }
                Ok(None) => break,
                Err(e) => return query_error(e),
            }
        }
    }

    // Tally distinct-session counts, drop singletons, keep the full pool size
    // for the footer, then bound the per-term doc-frequency probes to the
    // best-supported candidates (count desc, term asc). The dropped tail is all
    // count-MIN_SESSIONS noise — a few sessions sharing a one-off word.
    let mut candidates: Vec<(String, u32)> =
        cooccurrence_candidates(session_terms.values(), MIN_SESSIONS)
            .into_iter()
            .collect();
    let term_pool = candidates.len();
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    candidates.truncate(MAX_SCORED_TERMS);

    // Down-weight terms that are common across the whole archive. Document
    // frequency comes from an FTS phrase-count, so the term is tokenized by the
    // same porter stemmer as the index — no raw-vs-stemmed mismatch (a `fts5vocab`
    // lookup of the raw key misses every inflected word and wrongly boosts it).
    // score = damped term-frequency × idf — see `term_score`.
    let total_turns = conn
        .query_row("SELECT count(*) FROM turns", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
        .max(1);
    let mut scored: Vec<(String, u32, f64)> = Vec::with_capacity(candidates.len());
    {
        let mut df_stmt = conn
            .prepare("SELECT count(*) FROM turns_fts WHERE turns_fts MATCH ?")
            .ok();
        for (term, count) in candidates {
            // df ≥ count always; on a query failure fall back to count (rare).
            let df = df_stmt
                .as_mut()
                .and_then(|s| doc_frequency(s, &term))
                .unwrap_or(count as i64);
            scored.push((term, count, term_score(count, df, total_turns)));
        }
    }
    // Score desc, then raw count desc, then term asc — deterministic.
    scored.sort_by(|a, b| b.2.total_cmp(&a.2).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0)));
    let top_n = limit.max(0) as usize;
    let top_terms: Vec<(String, u32)> = scored
        .into_iter()
        .take(top_n)
        .map(|(t, c, _)| (t, c))
        .collect();
    let top_keys: HashSet<&str> = top_terms.iter().map(|(t, _)| t.as_str()).collect();

    // Rank sessions by how many top terms they contain (desc), recency (desc),
    // then sid (asc). Reuses the per-session term sets already built.
    let mut ranked: Vec<(usize, &SessionHit)> = hits
        .iter()
        .map(|h| {
            let shared = session_terms
                .get(&h.sid)
                .map(|s| s.iter().filter(|t| top_keys.contains(t.as_str())).count())
                .unwrap_or(0);
            (shared, h)
        })
        .collect();
    ranked.sort_by(|(ash, a), (bsh, b)| bsh.cmp(ash).then(b.ts.cmp(&a.ts)).then(a.sid.cmp(&b.sid)));
    let shown = (sessions.max(0) as usize).min(ranked.len());

    render(
        &Report {
            id,
            scanned,
            total,
            capped,
            term_pool,
        },
        &top_terms,
        &display,
        &session_terms,
        &ranked[..shown],
    );
    ExitCode::SUCCESS
}

/// Header/footer facts for the rendered report.
struct Report<'a> {
    id: &'a str,
    scanned: usize,
    total: usize,
    capped: bool,
    term_pool: usize,
}

/// Lowercased distinctive term keys in one turn, paired with their original
/// casing, with the anchor terms (and their stem/prefix variants) removed.
fn cooccurring_terms(text_s: &str, anchor_terms: &[String]) -> Vec<(String, String)> {
    text::extract_terms(text_s)
        .into_iter()
        .filter_map(|term| {
            let low = term.to_lowercase();
            // Drop pure number/date/version noise ("1.", "2026-06-12", "2.1") —
            // a real co-occurring term has at least one letter.
            if !low.chars().any(|c| c.is_ascii_alphabetic()) {
                return None;
            }
            if anchor_terms.iter().any(|a| text::token_matches(&low, a)) {
                None
            } else {
                Some((low, term))
            }
        })
        .collect()
}

/// Count, for each term, the number of distinct sessions it appears in, then
/// drop terms below `min_sessions`. The rankable candidate set.
fn cooccurrence_candidates<'a>(
    per_session: impl Iterator<Item = &'a HashSet<String>>,
    min_sessions: u32,
) -> HashMap<String, u32> {
    let mut tally: HashMap<String, u32> = HashMap::new();
    for set in per_session {
        for term in set {
            *tally.entry(term.clone()).or_insert(0) += 1;
        }
    }
    tally.retain(|_, c| *c >= min_sessions);
    tally
}

/// Rank score for a co-occurring term: damped term-frequency × inverse document
/// frequency. `count` is the number of anchor-sessions holding the term; `df` is
/// its global turn doc-frequency (always ≥ count). A globally-rare term that
/// co-occurs a few times beats ubiquitous boilerplate, while the log-damped
/// count stops a single rare fluke from outranking a well-supported term.
fn term_score(count: u32, df: i64, total_turns: i64) -> f64 {
    let df_eff = df.max(count as i64).max(1);
    let idf = (total_turns as f64 / df_eff as f64).ln().max(0.0);
    (1.0 + (count as f64).ln()) * idf
}

/// Global turn document-frequency of `term`, via an FTS phrase-count. The term
/// is phrase-quoted (so hyphens/dots don't trip FTS5) and tokenized by the same
/// porter stemmer as the index, so an inflected word counts all its forms.
fn doc_frequency(stmt: &mut rusqlite::Statement, term: &str) -> Option<i64> {
    let m = search::build_match(&[term.to_string()], false, false);
    stmt.query_row([m], |r| r.get::<_, i64>(0)).ok()
}

fn query_error(e: rusqlite::Error) -> ExitCode {
    eprintln!("[subrosa] query error: {e}");
    eprintln!("[subrosa] tip: wrap special characters in quotes");
    ExitCode::FAILURE
}

// At most this many shared terms are spelled out per session line; the rest
// collapse into a "+N more" so a wide top-N list doesn't blow out the line.
const SHARED_TERMS_PER_LINE: usize = 6;

fn render(
    report: &Report,
    top_terms: &[(String, u32)],
    display: &HashMap<String, String>,
    session_terms: &HashMap<String, HashSet<String>>,
    shown: &[(usize, &SessionHit)],
) {
    let cap_note = if report.capped {
        format!(" (capped from {})", report.total)
    } else {
        String::new()
    };
    println!(
        "related to «{}» — {} session(s) scanned{cap_note}",
        report.id, report.scanned
    );

    println!("\nterms (co-occur in {MIN_SESSIONS}+ sessions):");
    if top_terms.is_empty() {
        println!("  (none — every co-occurring term appears in just one session)");
    } else {
        for (key, count) in top_terms {
            let term = display.get(key).map(String::as_str).unwrap_or(key.as_str());
            println!("  {count:>3}  {term}");
        }
        if report.term_pool > top_terms.len() {
            println!(
                "  … {} more co-occurring term(s)",
                report.term_pool - top_terms.len()
            );
        }
    }

    println!("\nsessions:");
    for (i, (_, h)) in shown.iter().enumerate() {
        let sid8 = text::sid8(&h.sid);
        let snip = text::collapse_ws(h.snippet.as_deref().unwrap_or(""));
        println!(
            "{:>2}. [{}] {} · {} · {}  ({})",
            i + 1,
            timeutil::fmt_ts(h.ts.as_deref().unwrap_or("")),
            h.role,
            h.project.as_deref().unwrap_or("?"),
            sid8,
            shared_terms(top_terms, display, session_terms.get(&h.sid))
        );
        println!("    {snip}");
    }
    println!(
        "\n[subrosa] {} of {} session(s). Open a session: {}/<project>/<session_id>.jsonl",
        shown.len(),
        report.scanned,
        paths::projects_dir().display()
    );
}

/// The top terms this session shares, in rank order, capped with a "+N more".
fn shared_terms(
    top_terms: &[(String, u32)],
    display: &HashMap<String, String>,
    session: Option<&HashSet<String>>,
) -> String {
    let Some(set) = session else {
        return "—".to_string();
    };
    let hit: Vec<&str> = top_terms
        .iter()
        .filter(|(key, _)| set.contains(key.as_str()))
        .map(|(key, _)| display.get(key).map(String::as_str).unwrap_or(key.as_str()))
        .collect();
    if hit.is_empty() {
        "—".to_string()
    } else if hit.len() > SHARED_TERMS_PER_LINE {
        format!(
            "{}, +{} more",
            hit[..SHARED_TERMS_PER_LINE].join(", "),
            hit.len() - SHARED_TERMS_PER_LINE
        )
    } else {
        hit.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(terms: &[&str]) -> HashSet<String> {
        terms.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn anchor_and_its_prefixes_are_excluded() {
        let anchor = vec!["cache-prod".to_string()];
        let got: Vec<String> = cooccurring_terms(
            "deploying cache-prod and CACHE-PROD with cache plus TICKET-123",
            &anchor,
        )
        .into_iter()
        .map(|(low, _)| low)
        .collect();
        // The anchor (either casing) and its prefix `cache` are gone; the rest stays.
        assert!(!got.contains(&"cache-prod".to_string()));
        assert!(!got.contains(&"cache".to_string()));
        assert!(got.contains(&"deploying".to_string()));
        assert!(got.contains(&"ticket-123".to_string()));
    }

    #[test]
    fn cooccurring_terms_keeps_first_seen_casing() {
        let pairs = cooccurring_terms("OAuth then oauth again", &[]);
        // Deduped within the turn by `extract_terms`; the display casing is the first seen.
        let oauth: Vec<&(String, String)> = pairs.iter().filter(|(l, _)| l == "oauth").collect();
        assert_eq!(oauth.len(), 1);
        assert_eq!(oauth[0].1, "OAuth");
    }

    #[test]
    fn singletons_dropped_shared_terms_counted() {
        let sessions = [
            set(&["rollout", "ticket-123", "kubectl"]),
            set(&["rollout", "ticket-123", "latency"]),
            set(&["rollout", "pgbouncer"]),
        ];
        let cand = cooccurrence_candidates(sessions.iter(), MIN_SESSIONS);
        assert_eq!(cand.get("rollout"), Some(&3)); // all three sessions
        assert_eq!(cand.get("ticket-123"), Some(&2)); // two sessions
        assert_eq!(cand.get("kubectl"), None); // singleton dropped
        assert_eq!(cand.get("latency"), None); // singleton dropped
        assert_eq!(cand.get("pgbouncer"), None); // singleton dropped
    }

    #[test]
    fn term_score_demotes_common_boosts_rare_and_well_supported() {
        let n = 50_000;
        // Equal support, very different global frequency: the rarer term wins big.
        let common = term_score(100, 40_000, n); // boilerplate, in most turns
        let rare = term_score(100, 200, n); // a focused, archive-rare term
        assert!(
            rare > common * 5.0,
            "a globally-rare term must clearly outrank boilerplate, {rare} vs {common}"
        );
        // A single rare fluke (count 2) must not outrank a well-supported rare term.
        assert!(
            rare > term_score(2, 2, n),
            "log-damped count keeps a fluke from winning"
        );
        // df is clamped to at least `count`, so a bogus df can't divide by ~0.
        assert!(term_score(10, 0, n).is_finite());
    }

    #[test]
    fn number_and_date_noise_is_filtered_out() {
        let keys: Vec<String> =
            cooccurring_terms("see step 1. on 2026-06-12 the 2.1 release for auth.ts", &[])
                .into_iter()
                .map(|(low, _)| low)
                .collect();
        // Pure number/date/version tokens are gone; real terms stay.
        assert!(!keys
            .iter()
            .any(|k| k == "1." || k == "2026-06-12" || k == "2.1"));
        assert!(keys.contains(&"auth.ts".to_string()));
        assert!(keys.contains(&"release".to_string()));
    }

    // Guards the down-weight: document frequency via FTS phrase-count must work
    // on a READ-ONLY WAL connection (the flags `db::connect_readonly` uses) and
    // stem consistently — `description` must also count `descriptions`, so an
    // inflected common word reads as common. (A raw `fts5vocab` key lookup missed
    // inflections and wrongly boosted them — the bug calibration surfaced.)
    #[test]
    fn doc_frequency_via_fts_on_readonly_wal_is_stem_consistent() {
        use rusqlite::{Connection, OpenFlags};
        let p = std::env::temp_dir().join(format!("subrosa-reldf-{}.db", std::process::id()));
        let clean = |p: &std::path::Path| {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
            }
        };
        clean(&p);
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(
                "PRAGMA journal_mode=WAL;\n\
                 CREATE TABLE turns(id INTEGER PRIMARY KEY, text TEXT);\n\
                 CREATE VIRTUAL TABLE turns_fts USING fts5(\
                   text, content='turns', content_rowid='id', tokenize='porter unicode61');\n\
                 CREATE TRIGGER turns_ai AFTER INSERT ON turns BEGIN\n\
                   INSERT INTO turns_fts(rowid, text) VALUES (new.id, new.text);\n\
                 END;\n\
                 INSERT INTO turns(text) VALUES \
                   ('the description here'),\
                   ('many descriptions there'),\
                   ('an unrelated note');",
            )
            .unwrap();
        }
        let c = Connection::open_with_flags(
            &p,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let mut stmt = c
            .prepare("SELECT count(*) FROM turns_fts WHERE turns_fts MATCH ?")
            .unwrap();
        let df = doc_frequency(&mut stmt, "description");
        drop(stmt);
        drop(c);
        clean(&p);
        assert_eq!(
            df,
            Some(2),
            "porter stems `descriptions`→`description`, so df counts both forms"
        );
    }
}
