//! Auto-derived, read-only session tags. At ingest we read a session's stored
//! (already-redacted) turns and distill three namespaces — `tool:<name>`,
//! `ext:<ext>`, `topic:<term>` — into the `session_tags` table, so the archive
//! can be filtered by what a session was *about* without a keyword. Derivation is
//! fully deterministic (same turns → same `(ns, tag, rank)` rows); the golden
//! tests depend on it. Tags are never user-mutable: recompute is the only writer.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use regex::Regex;
use rusqlite::{params, Connection, Transaction, TransactionBehavior};

use crate::text;

// Per-namespace caps. Total stays ≤ ~30 tags/session.
const TOOL_CAP: usize = 12;
const EXT_CAP: usize = 8;
const TOPIC_CAP: usize = 10;

// File extensions worth a tag: code + config only. The allowlist is what keeps
// version numbers (`v0.10.0`), domains (`example.com`), and prose dots from
// minting junk `ext:` tags — only a match here survives.
const ALLOWED_EXTS: &[&str] = &[
    "rs", "go", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "sh", "bash", "zsh", "sql", "md",
    "mdx", "toml", "yaml", "yml", "json", "jsonc", "tf", "tfvars", "hcl", "c", "h", "cpp", "hpp",
    "cc", "hh", "cxx", "java", "rb", "php", "cs", "swift", "kt", "kts", "scala", "clj", "ex",
    "exs", "erl", "lua", "pl", "pm", "r", "jl", "dart", "vue", "svelte", "css", "scss", "sass",
    "less", "html", "htm", "xml", "proto", "graphql", "gql", "ini", "cfg", "conf", "env", "gradle",
    "groovy", "cmake", "ps1", "psm1", "zig", "nim", "hs", "ml",
];

// `⚙ <Name>` is the marker the ingester writes for every tool_use; the name is
// alphanumeric/underscore (`Bash`, `mcp__github__create_pull_request`).
fn tool_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\u{2699} ([A-Za-z0-9_]+)").unwrap())
}

// A file extension: a dot then 1–8 alphanumerics. The allowlist filters it.
fn ext_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\.([A-Za-z0-9]{1,8})").unwrap())
}

// Same token shape as text::extract_terms — used only to count term frequency.
fn term_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9][A-Za-z0-9._-]*").unwrap())
}

/// Display order across namespaces: tools first, then extensions, then topics.
/// Rank orders within a namespace; this orders between them.
pub(crate) fn ns_order(ns: &str) -> u8 {
    match ns {
        "tool" => 0,
        "ext" => 1,
        _ => 2,
    }
}

// The redaction sentinels (`‹redacted›`, `‹redacted-aws-key›`, …) tokenize to
// `redacted` / `redacted-*`; never let a masked secret mint a topic tag.
fn is_redaction_sentinel(low: &str) -> bool {
    low == "redacted" || low.starts_with("redacted-")
}

// Strip leading/trailing `.`/`-`/`_` so a sentence artifact like "now." doesn't
// become a tag; interior punctuation in real identifiers (auth.ts) is untouched.
fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c| c == '.' || c == '-' || c == '_')
}

/// Derive the `(ns, "ns:value", rank)` rows for one session's combined text.
/// Pure and deterministic — every collection has a total-ordered sort.
fn derive_from_text(text: &str) -> Vec<(&'static str, String, i64)> {
    let mut out: Vec<(&'static str, String, i64)> = Vec::new();

    // tool: unique names from the ⚙ marker, alphabetical, cap 12.
    let mut tools: Vec<String> = tool_re()
        .captures_iter(text)
        .map(|c| c[1].to_lowercase())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    tools.truncate(TOOL_CAP);
    for (i, name) in tools.iter().enumerate() {
        out.push(("tool", format!("tool:{name}"), i as i64));
    }

    // ext: allowed file extensions, frequency desc then ext asc, cap 8.
    let mut ext_freq: HashMap<String, usize> = HashMap::new();
    for c in ext_re().captures_iter(text) {
        let e = c[1].to_lowercase();
        if ALLOWED_EXTS.contains(&e.as_str()) {
            *ext_freq.entry(e).or_insert(0) += 1;
        }
    }
    let mut exts: Vec<(String, usize)> = ext_freq.into_iter().collect();
    exts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    exts.truncate(EXT_CAP);
    for (i, (e, _)) in exts.iter().enumerate() {
        out.push(("ext", format!("ext:{e}"), i as i64));
    }

    // topic: anchor-grade distinctive terms (no stemming — identifiers stay
    // whole), frequency desc → first-seen asc → lexicographic, cap 10. Trailing
    // `.`/`-`/`_` is stripped first so a sentence artifact like "now." can't become
    // a tag; interior punctuation in real identifiers (auth.ts) is untouched.
    let mut freq: HashMap<String, usize> = HashMap::new();
    for m in term_re().find_iter(text) {
        let low = trim_punct(m.as_str()).to_lowercase();
        if !low.is_empty() {
            *freq.entry(low).or_insert(0) += 1;
        }
    }
    let terms = text::extract_terms(text); // original casing, first-seen, deduped
    let mut topics: Vec<(String, usize, usize)> = Vec::new(); // (value, freq, first-seen idx)
    for (idx, t) in terms.iter().enumerate() {
        let trimmed = trim_punct(t);
        // is_anchor on the trimmed original keeps ALL-CAPS detection (e.g. "DR").
        if trimmed.is_empty() || !text::is_anchor(trimmed) {
            continue;
        }
        let low = trimmed.to_lowercase();
        // Drop pure number/date/version tokens and the redaction sentinel.
        if !low.chars().any(|c| c.is_ascii_alphabetic()) || is_redaction_sentinel(&low) {
            continue;
        }
        let f = freq.get(&low).copied().unwrap_or(1);
        topics.push((low, f, idx));
    }
    topics.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    topics.truncate(TOPIC_CAP);
    for (i, (low, _, _)) in topics.iter().enumerate() {
        out.push(("topic", format!("topic:{low}"), i as i64));
    }

    out
}

/// Recompute one session's tags from its stored turns. Delete-and-recompute (not
/// INSERT OR IGNORE): `sweep()` re-ingests a growing live session and the correct
/// top-N topic set shifts as it grows, so stale rows must be cleared. Runs in its
/// own short IMMEDIATE transaction. Reads only already-redacted, non-noise turns.
pub fn derive_tags(conn: &Connection, session_id: &str) -> rusqlite::Result<()> {
    let text = {
        let mut stmt = conn.prepare(
            "SELECT text FROM turns \
             WHERE session_id=?1 AND is_meta=0 AND is_sidechain=0 ORDER BY id",
        )?;
        let mut joined = String::new();
        let rows = stmt.query_map([session_id], |r| r.get::<_, Option<String>>(0))?;
        for r in rows {
            if let Some(t) = r? {
                if !joined.is_empty() {
                    joined.push('\n');
                }
                joined.push_str(&t);
            }
        }
        joined
    };
    let tags = derive_from_text(&text);

    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM session_tags WHERE session_id=?1", [session_id])?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO session_tags(session_id, ns, tag, rank) VALUES (?1,?2,?3,?4)",
        )?;
        for (ns, tag, rank) in &tags {
            stmt.execute(params![session_id, ns, tag, rank])?;
        }
    }
    tx.commit()
}

/// One-time pass at schema-v3 upgrade: derive tags for every session that has
/// none yet. Each session commits independently (finer-grained than a batch), so
/// an interrupted run resumes the remainder on the next connect — the
/// `NOT EXISTS` filter skips what already landed. A per-session failure is logged,
/// not fatal, so one bad session can't strand the rest.
pub fn backfill(conn: &Connection) -> rusqlite::Result<()> {
    let sids: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT s.session_id FROM sessions s \
             WHERE NOT EXISTS (SELECT 1 FROM session_tags st WHERE st.session_id = s.session_id)",
        )?;
        let v = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<String>, _>>()?;
        v
    };
    for sid in &sids {
        if let Err(e) = derive_tags(conn, sid) {
            eprintln!("[subrosa] tag backfill {sid}: {e}");
        }
    }
    Ok(())
}

/// One session's tags in display order (tool, ext, topic; by rank within each).
/// The read path for `session --tags` and the `sessions` listing.
pub(crate) fn tags_for_session(conn: &Connection, sid: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT ns, tag, rank FROM session_tags WHERE session_id=?1")?;
    let mut rows: Vec<(String, String, i64)> = stmt
        .query_map([sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    rows.sort_by(|a, b| {
        ns_order(&a.0)
            .cmp(&ns_order(&b.0))
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.1.cmp(&b.1))
    });
    Ok(rows.into_iter().map(|(_, tag, _)| tag).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_from_text_is_deterministic_and_namespaced() {
        // Two tools (alphabetical), one allowed ext, and anchor-grade topics
        // ranked by frequency (cache-prod appears twice) then first-seen.
        let text = "\u{2699} Bash ran\n\
                    \u{2699} Read opened auth.ts\n\
                    cache-prod rollout cache-prod TICKET-123 password=\u{2039}redacted\u{203a}";
        let got = derive_from_text(text);
        assert_eq!(
            got,
            vec![
                ("tool", "tool:bash".to_string(), 0),
                ("tool", "tool:read".to_string(), 1),
                ("ext", "ext:ts".to_string(), 0),
                ("topic", "topic:cache-prod".to_string(), 0),
                ("topic", "topic:opened".to_string(), 1),
                ("topic", "topic:auth.ts".to_string(), 2),
                ("topic", "topic:rollout".to_string(), 3),
                ("topic", "topic:ticket-123".to_string(), 4),
                ("topic", "topic:password".to_string(), 5),
            ]
        );
        // The redaction sentinel never becomes a topic.
        assert!(!got.iter().any(|(_, t, _)| t == "topic:redacted"));
    }

    #[test]
    fn derive_from_text_empty_when_nothing_distinctive() {
        // No tool marker, no allowed ext, no anchor-grade term.
        assert!(derive_from_text("ok did the run go").is_empty());
    }
}
