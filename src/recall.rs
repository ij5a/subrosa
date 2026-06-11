//! UserPromptSubmit recall: find archived sessions relevant to the current
//! prompt and return lines to inject into context. Stays quiet unless there's
//! a strong match. Read-only, mechanical; never blocks the prompt.
//!
//! Relevance gate (to avoid noise on every prompt): the prompt must contain
//! at least 2 distinctive terms (identifiers, or words of 4+ chars that
//! aren't stopwords); a past turn only counts if it contains at least 2 of
//! those terms; scoped to the current project; the live session is excluded.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::db;

const MAX_INJECT: usize = 3;
const SNIPPET_LEN: usize = 160;
const MIN_TERMS: usize = 2;

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
        if identifierish || (tok.len() >= 4 && !STOPWORDS.contains(&low.as_str())) {
            terms.push(tok.to_string());
            seen.insert(low);
        }
    }
    terms
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

    let low_terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let mut picked = Vec::new();
    let mut seen_sessions = HashSet::new();
    for (sid, ts, text) in rows {
        let text_low = text.as_deref().unwrap_or("").to_lowercase();
        if low_terms
            .iter()
            .filter(|t| text_low.contains(t.as_str()))
            .count()
            < MIN_TERMS
        {
            continue;
        }
        if !seen_sessions.insert(sid.clone()) {
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
    for (sid, ts, text) in picked {
        let snip: String = text
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
    Some(lines.join("\n"))
}
