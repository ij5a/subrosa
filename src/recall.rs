//! UserPromptSubmit recall: find archived sessions relevant to the current
//! prompt and return lines to inject into context. Stays quiet unless there's
//! a strong match. DB read-only, mechanical; never blocks the prompt.
//!
//! Relevance gate (to avoid noise on every prompt): the prompt must contain
//! at least 2 distinctive terms (identifiers, or words of 4+ chars that
//! aren't stopwords); a past turn counts only if at least `min_required` of
//! those terms match a turn token — stem/prefix-aware and separator-insensitive,
//! not substring — one of them anchor-grade (identifier-like or 6+ chars).
//! `min_required` scales mildly with prompt length (capped). Survivors are kept
//! by a relative bm25 floor,
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
// Cap the adaptive gate: a long pasted prompt shouldn't demand more than this many
// matched terms — the anchor rule already blocks generic-word noise.
const MAX_MIN_REQUIRED: usize = 3;
// Trim the dedup log once it's clearly past any live-session working set.
const SEEN_TRIM_AT: usize = 4000;
const SEEN_KEEP: usize = 2000;
/// The other trim trigger. Comfortably under `CONTROL_FILE_MAX`, so the file
/// is cut back long before it grows past what the reader will accept.
const SEEN_TRIM_BYTES: u64 = 512 * 1024;
/// Longest line the dedup log can hold. Ours are two ids and a tab; anything
/// longer came from somewhere else and is dropped on the next trim.
const SEEN_LINE_MAX: usize = 512;

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
    let Ok(Some(text)) = paths::read_control_file(log, paths::CONTROL_FILE_MAX) else {
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
        let _ = paths::write_control_file(log, &body);
    }
}

/// Best-effort append (+ occasional trim) of injected source sessions. No
/// locking: a lost line costs one repeated injection at worst. `seen_text` is
/// the log content this prompt already read — no second read on the hot path.
///
/// Opening is guarded the same way the read is: appending to a FIFO blocks
/// until someone reads it, and this runs on every prompt.
fn remember_injected<'a>(
    log: &Path,
    seen_text: &str,
    session: &str,
    sids: impl Iterator<Item = &'a str>,
) {
    if session.is_empty() || !paths::appendable(log) {
        return;
    }
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // What this prompt is about to add, measured, because the file has to be
    // under the threshold once those lines are in it too. Held to the same
    // length bound as anything already in there: an oversized new line must
    // not be the thing that pushes the file past the cap.
    let additions: Vec<String> = sids
        .map(|sid| format!("{session}\t{sid}\n"))
        .filter(|l| l.len() <= SEEN_LINE_MAX)
        .collect();
    let adding: u64 = additions.iter().map(|l| l.len() as u64).sum();

    let lines: Vec<&str> = seen_text.lines().collect();
    let on_disk = std::fs::metadata(log).map(|m| m.len()).unwrap_or(0);
    // Trimmed on line count OR on bytes. Counting lines alone is not enough:
    // one pathological line keeps the file over the read cap forever, and past
    // that cap the dedup goes dark for good.
    if lines.len() > SEEN_TRIM_AT || on_disk + adding > SEEN_TRIM_BYTES {
        let body = tail_within(&lines, SEEN_TRIM_BYTES.saturating_sub(adding));
        // A trim that fails and an append that succeeds is the worst pair:
        // the file grows past the read cap, and past it reading and healing
        // both fail, so recall goes quiet on every future prompt with no way
        // back. Leave the file exactly as it was instead.
        if paths::write_control_file(log, &body).is_err() {
            return;
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log)
    {
        for line in &additions {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// The newest lines that fit in `budget` bytes once written back, oldest
/// first. Lines past `SEEN_LINE_MAX` are dropped rather than kept: one of them
/// alone can hold the file over the cap, and nothing we write is ever that
/// long — two ids and a tab.
fn tail_within(lines: &[&str], budget: u64) -> String {
    let mut used = 0u64;
    let mut keep: Vec<&str> = Vec::new();
    for line in lines.iter().rev().take(SEEN_KEEP) {
        if line.len() > SEEN_LINE_MAX {
            continue;
        }
        let cost = line.len() as u64 + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        keep.push(line);
    }
    keep.reverse();
    match keep.is_empty() {
        true => String::new(),
        false => keep.join("\n") + "\n",
    }
}

/// A dedup log that has outgrown the read cap can never be read again, so the
/// dedup would stay dark forever and every prompt would re-inject the same
/// sessions. Emptying it costs one repeated injection and gets it back.
fn heal_seen_log(log: &Path) {
    // Only an oversized plain file of ours. Anything else — a FIFO, a symlink
    // pointing somewhere odd — is left exactly as it is.
    if std::fs::symlink_metadata(log).is_ok_and(|m| m.is_file() && m.len() > SEEN_TRIM_BYTES) {
        let _ = paths::write_control_file(log, "");
        return;
    }
    // Nothing we can fix, so recall will stay quiet from here on. This is the
    // UserPromptSubmit path — stdout belongs to the injected context — so the
    // only place to say so is the log. One line a prompt is noisy; a silent
    // hole with no trail anywhere is worse.
    crate::hook::log(&format!(
        "recall off: {} can't be read and isn't a plain file we can reset — \
         delete it to bring recall back",
        log.display()
    ));
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

/// Matched-term bar for a prompt of `n` distinctive terms: scales mildly with
/// length but clamped to `[MIN_TERMS, MAX_MIN_REQUIRED]` so a pasted wall of text
/// can't over-filter. Pure so the clamp is unit-testable.
fn min_required_terms(n: usize) -> usize {
    MIN_TERMS.max(n / 5).min(MAX_MIN_REQUIRED)
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
    // #5 adaptive gate: more matched terms on longer prompts, clamped to
    // [MIN_TERMS, MAX_MIN_REQUIRED] so a pasted wall can't over-filter. Anchor rule untouched.
    let min_required = min_required_terms(terms.len());

    // #2 stem/prefix term match on the full turn text (not the snippet), plus the anchor rule.
    let mut qualified: Vec<Candidate> = Vec::new();
    for c in rows {
        let text_low = c.text.as_deref().unwrap_or("").to_lowercase();
        let toks = text::turn_tokens(&text_low);
        let matched: Vec<&(String, bool)> = term_meta
            .iter()
            .filter(|(low, _)| toks.iter().any(|tok| text::token_matches_loose(tok, low)))
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
    let seen_text = match paths::read_control_file(&seen_log, paths::CONTROL_FILE_MAX) {
        Ok(text) => text.unwrap_or_default(),
        // Unusable dedup state is not the same as an empty one: injecting now
        // would be blind, since we could not record what went out, and the same
        // sessions would come back on every prompt. Stay quiet — and empty a
        // log that has simply outgrown the cap so the next prompt works.
        Err(_) => {
            heal_seen_log(&seen_log);
            return None;
        }
    };
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
        let snip: String = text::collapse_ws(c.snippet.as_deref().unwrap_or_default())
            .chars()
            .take(SNIPPET_CHARS)
            .collect();
        let sid8 = text::sid8(&c.session_id);
        // Age hint after the date, shared with `search`; empty (no parens) when the
        // timestamp is missing or unparseable.
        let age = match c.ts.as_deref().and_then(timeutil::parse_ts) {
            Some(epoch) => timeutil::age_suffix(now - epoch),
            None => String::new(),
        };
        lines.push(format!(
            "- {}{} · {}: {}",
            fmt_ts(c.ts.as_deref()),
            age,
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

    /// The trim has to leave the file provably under the threshold, in BYTES.
    /// Keeping a fixed number of lines doesn't: one oversized line survives
    /// every trim and holds the file over the read cap for good, after which
    /// the dedup goes dark and recall re-injects the same sessions forever.
    #[test]
    fn the_trim_bounds_bytes_not_just_lines() {
        let long = "x".repeat(SEEN_LINE_MAX + 1);
        let normal = "sess-aaaa\tsrc-bbbb";
        let lines: Vec<&str> = vec![&long, normal, &long, normal, normal];

        // Way past the bound: only the ordinary lines come back, newest last,
        // and the result really is under budget.
        let body = tail_within(&lines, 1024);
        assert!(!body.contains('x'), "an oversized line survived: {body:?}");
        assert_eq!(body, format!("{normal}\n{normal}\n{normal}\n"));
        assert!((body.len() as u64) <= 1024);

        // A budget that fits two lines keeps the two NEWEST, not the oldest.
        let numbered: Vec<String> = (0..5).map(|i| format!("s\tsid{i}")).collect();
        let refs: Vec<&str> = numbered.iter().map(String::as_str).collect();
        let two = tail_within(&refs, (numbered[0].len() as u64 + 1) * 2);
        assert_eq!(two, "s\tsid3\ns\tsid4\n");

        // No budget at all is an empty file, never a partial line.
        assert_eq!(tail_within(&refs, 0), "");
        // Nothing to keep is empty too, not a stray newline.
        assert_eq!(tail_within(&[&long], 1024), "");
    }

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

    #[test]
    fn min_required_terms_scales_then_clamps() {
        assert_eq!(min_required_terms(4), 2, "short prompt holds at MIN_TERMS");
        assert_eq!(min_required_terms(15), 3, "scales up with length");
        assert_eq!(
            min_required_terms(50),
            3,
            "clamped, not 10, on a pasted wall"
        );
    }
}
