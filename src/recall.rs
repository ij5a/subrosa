//! UserPromptSubmit recall: find archived sessions relevant to the current
//! prompt and return lines to inject into context. Stays quiet unless there's
//! a strong match. DB read-only, mechanical; never blocks the prompt.
//!
//! Relevance gate (to avoid noise on every prompt): the prompt must contain
//! at least 2 distinctive terms (identifiers, or words of 4+ chars that
//! aren't stopwords); a past turn only counts if it contains at least 2 of
//! those terms, one of them anchor-grade (identifier-like or 6+ chars);
//! scoped to the current project; the live session is excluded. Source
//! sessions already injected into this live session are skipped
//! (recall-seen.log) so a same-topic conversation doesn't re-inject them
//! on every prompt.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::{db, paths};

const MAX_INJECT: usize = 3;
const SNIPPET_LEN: usize = 160;
const MIN_TERMS: usize = 2;
// Trim the dedup log once it's clearly past any live-session working set.
const SEEN_TRIM_AT: usize = 4000;
const SEEN_KEEP: usize = 2000;

const STOPWORDS: &[&str] = &[
    "the",
    "a",
    "an",
    "and",
    "or",
    "but",
    "if",
    "then",
    "else",
    "of",
    "to",
    "in",
    "on",
    "at",
    "for",
    "with",
    "from",
    "by",
    "as",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "this",
    "that",
    "these",
    "those",
    "it",
    "its",
    "do",
    "does",
    "did",
    "can",
    "could",
    "should",
    "would",
    "will",
    "shall",
    "may",
    "might",
    "must",
    "not",
    "no",
    "yes",
    "ok",
    "okay",
    "i",
    "you",
    "we",
    "they",
    "he",
    "she",
    "him",
    "her",
    "his",
    "our",
    "your",
    "their",
    "what",
    "which",
    "who",
    "whom",
    "how",
    "why",
    "when",
    "where",
    "here",
    "there",
    "please",
    "let",
    "lets",
    "just",
    "only",
    "also",
    "more",
    "most",
    "some",
    "any",
    "all",
    "every",
    "each",
    "other",
    "than",
    "too",
    "very",
    "much",
    "many",
    "few",
    "need",
    "want",
    "make",
    "made",
    "sure",
    "able",
    "part",
    "flow",
    "doing",
    "something",
    "thing",
    "things",
    "get",
    "got",
    "use",
    "used",
    "using",
    "update",
    "updated",
    "add",
    "added",
    "fix",
    "fixed",
    "change",
    "changed",
    "check",
    "checked",
    "look",
    "looking",
    "go",
    "going",
    "know",
    "think",
    "see",
    "say",
];

/// Terms worth searching for: identifiers (digits/underscore/hyphen or
/// all-caps), or words >= 4 chars that aren't stopwords. Deduped, order kept.
fn distinctive_terms(prompt: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[A-Za-z0-9][A-Za-z0-9._-]*").unwrap());
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    for m in re.find_iter(prompt) {
        let tok = m.as_str();
        let low = tok.to_lowercase();
        if seen.contains(&low) {
            continue;
        }
        let identifierish = tok
            .chars()
            .any(|c| c.is_ascii_digit() || c == '_' || c == '-')
            || (tok.len() >= 2
                && tok.chars().any(|c| c.is_ascii_uppercase())
                && !tok.chars().any(|c| c.is_ascii_lowercase()));
        if (identifierish || tok.len() >= 4) && !STOPWORDS.contains(&low.as_str()) {
            terms.push(tok.to_string());
            seen.insert(low);
        }
    }
    terms
}

/// Anchor-grade terms justify an injection: identifier-like (digit, `_`, `-`,
/// `.`, or ALL-CAPS) or 6+ chars. Two short generic words never fire alone.
fn is_anchor(tok: &str) -> bool {
    tok.chars()
        .any(|c| c.is_ascii_digit() || c == '_' || c == '-' || c == '.')
        || (tok.len() >= 2
            && tok.chars().any(|c| c.is_ascii_uppercase())
            && !tok.chars().any(|c| c.is_ascii_lowercase()))
        || tok.chars().count() >= 6
}

/// Source sessions already injected into this live session — never repeat them.
fn already_injected(log: &Path, session: &str) -> HashSet<String> {
    if session.is_empty() {
        return HashSet::new();
    }
    let Ok(text) = std::fs::read_to_string(log) else {
        return HashSet::new();
    };
    text.lines()
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
/// locking: a lost line costs one repeated injection at worst.
fn remember_injected<'a>(log: &Path, session: &str, sids: impl Iterator<Item = &'a str>) {
    if session.is_empty() {
        return;
    }
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = std::fs::read_to_string(log) {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > SEEN_TRIM_AT {
            let _ = std::fs::write(log, lines[lines.len() - SEEN_KEEP..].join("\n") + "\n");
        }
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
    let terms = distinctive_terms(prompt);
    if terms.len() < MIN_TERMS {
        return None;
    }
    let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or("");
    let cur_session = input
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let fts_match = terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");

    // No archive yet, or locked — stay quiet.
    let conn = db::connect_readonly().ok()?;

    let mut sql = String::from(
        "SELECT t.session_id, t.ts, t.text \
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
    let rows: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .ok()?
        .flatten()
        .collect();

    // (lowercased term, anchor-grade) — anchor judged on the original casing.
    let term_meta: Vec<(String, bool)> = terms
        .iter()
        .map(|t| (t.to_lowercase(), is_anchor(t)))
        .collect();
    let seen_log = paths::recall_seen_log();
    let already = already_injected(&seen_log, cur_session);
    let mut picked = Vec::new();
    let mut seen_sessions = HashSet::new();
    for (sid, ts, text) in rows {
        let text_low = text.as_deref().unwrap_or("").to_lowercase();
        let matched: Vec<&(String, bool)> = term_meta
            .iter()
            .filter(|(low, _)| text_low.contains(low.as_str()))
            .collect();
        if matched.len() < MIN_TERMS || !matched.iter().any(|(_, anchor)| *anchor) {
            continue;
        }
        if already.contains(&sid) || !seen_sessions.insert(sid.clone()) {
            continue;
        }
        picked.push((sid, ts, text));
        if picked.len() >= MAX_INJECT {
            break;
        }
    }
    if picked.is_empty() {
        return None;
    }

    let mut lines = vec![String::from(
        "[subrosa recall] Possibly relevant past sessions from the local archive — verify before \
         relying on them; run `subrosa search` for the full text:",
    )];
    for (sid, ts, text) in &picked {
        let snip: String = text
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(SNIPPET_LEN)
            .collect();
        let sid8: String = sid.chars().take(8).collect();
        lines.push(format!("- {} · {}: {}", fmt_ts(ts.as_deref()), sid8, snip));
    }
    remember_injected(
        &seen_log,
        cur_session,
        picked.iter().map(|(sid, _, _)| sid.as_str()),
    );
    Some(lines.join("\n"))
}
