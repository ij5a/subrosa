//! UserPromptSubmit recall: find archived sessions relevant to the current
//! prompt and return lines to inject into context. Stays quiet unless there's
//! a strong match. DB read-only, mechanical; never blocks the prompt.
//!
//! Relevance gate (to avoid noise on every prompt): the prompt must contain
//! at least 2 distinctive terms (identifiers, or words of 4+ chars that
//! aren't stopwords); a past turn counts only if at least `min_required` of
//! those terms match a turn token — stem/prefix-aware, not substring — one of
//! them anchor-grade (identifier-like or 6+ chars). `min_required` scales
//! mildly with prompt length. Survivors are kept by a relative bm25 floor,
//! re-ranked with a mild recency tie-break, deduped one-per-session, top 3.
//! The snippet is FTS5 match-centered so the injected line shows why it hit.
//! Scoped to the current project; the live session is excluded. Source
//! sessions already injected into this live session are skipped
//! (recall-seen.log) so a same-topic conversation doesn't re-inject them.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use serde_json::Value;

use crate::{db, paths, text, timeutil};

const MAX_INJECT: usize = 3;
// Hard cap on the rendered snippet: holds recall at the documented ~180 tokens/prompt
// even though the snippet is now match-centered, not the turn's first 160 chars.
const SNIPPET_CHARS: usize = 160;
const MIN_TERMS: usize = 2;
// bm25 floor: keep candidates within this factor of the best hit (scores are negative,
// lower = better) — worst_kept = best / FACTOR. Calibrated against a real archive.
const BM25_FLOOR_FACTOR: f64 = 1.6;
// Recency tie-break (half-life 30 days), scaled by the best score's magnitude (itself
// capped) so it only reorders genuine bm25 near-ties, never overrides a real gap.
const RECENCY_WEIGHT: f64 = 0.15;
const RECENCY_ABS_CAP: f64 = 10.0;
// FTS candidate query: cap the OR-union so a pasted wall of text can't explode
// into a hundred-branch posting-list merge. The post-filter still sees every term.
const MAX_FTS_TERMS: usize = 12;
// Trim the dedup log once it's clearly past any live-session working set.
const SEEN_TRIM_AT: usize = 4000;
const SEEN_KEEP: usize = 2000;

/// Source sessions already injected into this live session — never repeat them.
/// Takes the log text (read once per prompt) rather than re-reading the file.
fn already_injected(seen_text: &str, session: &str) -> HashSet<String> {
    if session.is_empty() {
        return HashSet::new();
    }
    seen_text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter(|(s, _)| *s == session)
        .map(|(_, sid)| sid.to_string())
        .collect()
}

/// Forget a live session's dedup entries. PreCompact uses this: the injected
/// blocks die with the compacted context, so re-injection is useful again.
pub fn forget_session(log: &Path, session: &str) {
    if session.is_empty() {
        return;
    }
    let Ok(text) = std::fs::read_to_string(log) else {
        return;
    };
    let keep: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.split_once('\t')
                .map(|(s, _)| s != session)
                .unwrap_or(true)
        })
        .collect();
    if keep.len() < text.lines().count() {
        let body = if keep.is_empty() {
            String::new()
        } else {
            keep.join("\n") + "\n"
        };
        let _ = std::fs::write(log, body);
    }
}

/// Best-effort append (+ occasional trim) of injected source sessions. No
/// locking: a lost line costs one repeated injection at worst. `seen_text` is
/// the log content this prompt already read — no second read on the hot path.
fn remember_injected<'a>(
    log: &Path,
    seen_text: &str,
    session: &str,
    sids: impl Iterator<Item = &'a str>,
) {
    if session.is_empty() {
        return;
    }
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lines: Vec<&str> = seen_text.lines().collect();
    if lines.len() > SEEN_TRIM_AT {
        let _ = std::fs::write(log, lines[lines.len() - SEEN_KEEP..].join("\n") + "\n");
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log)
    {
        for sid in sids {
            let _ = writeln!(f, "{session}\t{sid}");
        }
    }
}

/// Date part of the stored ISO timestamp (stored zone, ~UTC — same display
/// convention as `subrosa search`).
fn fmt_ts(ts: Option<&str>) -> String {
    match ts {
        Some(t) if !t.is_empty() => t.chars().take(10).collect(),
        _ => "?".to_string(),
    }
}

/// One FTS candidate: the full text gates term matching (#2); the snippet is
/// the match-centered display string; bm25 drives the floor (#3) and re-rank (#4).
struct Candidate {
    session_id: String,
    ts: Option<String>,
    text: Option<String>,
    snippet: Option<String>,
    bm25: f64,
}

/// The bm25 cutoff for the floor (#3): drop candidates worse than `best / FACTOR`.
/// `best` is the most-negative (best) score. Returns None when scores are
/// degenerate (best >= 0 or NaN) — meaning "no floor, keep all".
fn floor_threshold(best: f64) -> Option<f64> {
    (best < 0.0).then_some(best / BM25_FLOOR_FACTOR)
}

/// Recency-blended rank from a raw score and age in days (half-life 30 days),
/// scaled by `scale` (the capped best-score magnitude). Lower sorts first.
/// Split out from `rank_value` so the blend is unit-testable without timestamps.
fn blended_rank(bm25: f64, age_days: f64, scale: f64) -> f64 {
    bm25 - RECENCY_WEIGHT * scale * 0.5f64.powf(age_days / 30.0)
}

/// `blended_rank` for a candidate: an unparseable/missing timestamp gets no
/// bonus (treated as infinitely old, so `0.5^∞ = 0`).
fn rank_value(c: &Candidate, now: i64, scale: f64) -> f64 {
    let age_days =
        c.ts.as_deref()
            .and_then(timeutil::parse_ts)
            .map(|epoch| ((now - epoch).max(0) as f64) / 86_400.0)
            .unwrap_or(f64::INFINITY);
    blended_rank(c.bm25, age_days, scale)
}

/// Build the injection block for this prompt, or None to stay silent.
pub fn run(input: &Value) -> Option<String> {
    let prompt = input
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if prompt.chars().count() < 8 {
        return None;
    }
    let terms = text::extract_terms(prompt);
    if terms.len() < MIN_TERMS {
        return None;
    }
    let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or("");
    let cur_session = input
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    // When over the cap, keep the terms most likely to be selective: anchors
    // first, then longer words. Under the cap the query is byte-identical.
    let mut fts_terms: Vec<&String> = terms.iter().collect();
    if fts_terms.len() > MAX_FTS_TERMS {
        fts_terms.sort_by_key(|t| (!text::is_anchor(t), std::cmp::Reverse(t.chars().count())));
        fts_terms.truncate(MAX_FTS_TERMS);
    }
    let fts_match = fts_terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");

    // No archive yet, or locked — stay quiet.
    let conn = db::connect_readonly().ok()?;

    // turns_fts is a fixed literal; MATCH/project/session stay bound params. The snippet
    // token budget (20) is a literal (not user input), sized to roughly fill the 160-char
    // cap; bm25 is selected so the floor and recency re-rank can read the score.
    let mut sql = String::from(
        "SELECT t.session_id, t.ts, t.text, bm25(turns_fts), \
                snippet(turns_fts, 0, '«', '»', '…', 20) \
         FROM turns_fts JOIN turns t ON t.id = turns_fts.rowid \
         WHERE turns_fts MATCH ?",
    );
    let mut binds: Vec<String> = vec![fts_match];
    if !cwd.is_empty() {
        sql.push_str(" AND t.project = ?");
        binds.push(db::encode_cwd(cwd));
    }
    if !cur_session.is_empty() {
        sql.push_str(" AND t.session_id <> ?");
        binds.push(cur_session.to_string());
    }
    sql.push_str(" ORDER BY bm25(turns_fts) LIMIT 30");

    let mut stmt = conn.prepare(&sql).ok()?;
    let rows: Vec<Candidate> = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            Ok(Candidate {
                session_id: r.get(0)?,
                ts: r.get(1)?,
                text: r.get(2)?,
                bm25: r.get(3)?,
                snippet: r.get(4)?,
            })
        })
        .ok()?
        .flatten()
        .collect();

    // (lowercased term, anchor-grade) — anchor judged on the original casing.
    let term_meta: Vec<(String, bool)> = terms
        .iter()
        .map(|t| (t.to_lowercase(), text::is_anchor(t)))
        .collect();
    // #5 adaptive gate: ask for more matched terms on longer prompts, never fewer than 2.
    // The anchor requirement is left untouched — scaling it would drop valid hits.
    let min_required = MIN_TERMS.max(terms.len() / 5);

    // #2 stem/prefix term match on the full turn text (not the snippet), plus the anchor rule.
    let mut qualified: Vec<Candidate> = Vec::new();
    for c in rows {
        let text_low = c.text.as_deref().unwrap_or("").to_lowercase();
        let toks = text::turn_tokens(&text_low);
        let matched: Vec<&(String, bool)> = term_meta
            .iter()
            .filter(|(low, _)| toks.iter().any(|tok| text::token_matches(tok, low)))
            .collect();
        if matched.len() < min_required || !matched.iter().any(|(_, anchor)| *anchor) {
            continue;
        }
        qualified.push(c);
    }
    if qualified.is_empty() {
        return None;
    }

    // #3 relative bm25 floor on raw scores: keep candidates within FACTOR of the best
    // (scores negative, lower = better). best >= 0 or NaN is degenerate → keep all.
    let best = qualified
        .iter()
        .map(|c| c.bm25)
        .fold(f64::INFINITY, f64::min);
    let floor = floor_threshold(best);

    // #4 recency tie-break: compute each rank once (one parse_ts per survivor), then sort.
    // Scale is the capped best magnitude, so a very strong match can't override a real gap.
    let now = timeutil::now_unix();
    let scale = best.abs().min(RECENCY_ABS_CAP);
    let mut ranked: Vec<(f64, Candidate)> = qualified
        .into_iter()
        .filter(|c| floor.is_none_or(|t| c.bm25 <= t))
        .map(|c| (rank_value(&c, now, scale), c))
        .collect();
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0));

    // One hit per session; skip sources already injected into this live session; top 3.
    let seen_log = paths::recall_seen_log();
    let seen_text = std::fs::read_to_string(&seen_log).unwrap_or_default();
    let already = already_injected(&seen_text, cur_session);
    let mut picked: Vec<Candidate> = Vec::new();
    let mut seen_sessions = HashSet::new();
    for (_, c) in ranked {
        if already.contains(&c.session_id) || !seen_sessions.insert(c.session_id.clone()) {
            continue;
        }
        picked.push(c);
        if picked.len() >= MAX_INJECT {
            break;
        }
    }
    if picked.is_empty() {
        return None;
    }

    // #1 render from the FTS match-centered snippet, whitespace-collapsed, hard-capped.
    let mut lines = vec![String::from(
        "[subrosa recall] Possibly relevant past sessions from the local archive — verify before \
         relying on them; run `subrosa search` for the full text:",
    )];
    for c in &picked {
        let snip: String = c
            .snippet
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(SNIPPET_CHARS)
            .collect();
        let sid8: String = c.session_id.chars().take(8).collect();
        lines.push(format!(
            "- {} · {}: {}",
            fmt_ts(c.ts.as_deref()),
            sid8,
            snip
        ));
    }
    remember_injected(
        &seen_log,
        &seen_text,
        cur_session,
        picked.iter().map(|c| c.session_id.as_str()),
    );
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bm25_floor_drops_weak_tail() {
        // best = -8, FACTOR 1.6 → threshold -5: keep -8 and -6, drop -2.
        let t = floor_threshold(-8.0).expect("a negative best yields a floor");
        assert!((t + 5.0).abs() < 1e-9, "threshold should be -5.0, got {t}");
        assert!(-8.0 <= t, "the best score stays");
        assert!(-6.0 <= t, "a within-factor score stays");
        assert!(-2.0 > t, "the weak tail is dropped");
    }

    #[test]
    fn bm25_floor_degenerate_keeps_all() {
        // best >= 0 or NaN is degenerate — no floor, keep everything.
        assert!(floor_threshold(0.0).is_none());
        assert!(floor_threshold(3.5).is_none());
        assert!(floor_threshold(f64::NAN).is_none());
    }

    #[test]
    fn recency_breaks_only_near_ties() {
        let scale = 8.0; // best.abs() for a -8 best, under RECENCY_ABS_CAP
                         // Near-tie: a fresh -7.9 edges out a slightly-better but 120-day-old -8.0.
        assert!(
            blended_rank(-7.9, 0.0, scale) < blended_rank(-8.0, 120.0, scale),
            "recency should flip a genuine near-tie"
        );
        // Clear gap: a much-stronger but stale -8.0 still beats a fresh -4.0.
        assert!(
            blended_rank(-8.0, 120.0, scale) < blended_rank(-4.0, 0.0, scale),
            "a clearly better bm25 must hold its lead"
        );
    }
}
