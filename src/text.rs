//! Shared text primitives: tokenizing turns/prompts and judging term quality.
//! Lifted out of recall.rs so `related` reuses the same tokenizer without
//! depending on the recall hot path. Behavior is pinned by the recall golden
//! test (recall was the only original caller) — keep it byte-identical.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

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

/// A token worth indexing/ranking: an identifier (has a digit, `_`, or `-`, or
/// is ALL-CAPS of 2+ chars) or a word of 4+ chars — and never a stopword.
/// Identifier shape is judged on the original casing; the stopword test is
/// case-insensitive.
pub(crate) fn is_distinctive(tok: &str) -> bool {
    let identifierish = tok
        .chars()
        .any(|c| c.is_ascii_digit() || c == '_' || c == '-')
        || (tok.len() >= 2
            && tok.chars().any(|c| c.is_ascii_uppercase())
            && !tok.chars().any(|c| c.is_ascii_lowercase()));
    let low = tok.to_lowercase();
    (identifierish || tok.len() >= 4) && !STOPWORDS.contains(&low.as_str())
}

/// Distinctive terms in first-seen order, deduped case-insensitively, original
/// casing kept. The token shape keeps identifiers whole (`cache-prod`,
/// `auth.ts`, `TICKET-123` — digit/underscore/hyphen/dot stay attached).
pub(crate) fn extract_terms(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[A-Za-z0-9][A-Za-z0-9._-]*").unwrap());
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    for m in re.find_iter(text) {
        let tok = m.as_str();
        let low = tok.to_lowercase();
        if seen.contains(&low) {
            continue;
        }
        if is_distinctive(tok) {
            terms.push(tok.to_string());
            seen.insert(low);
        }
    }
    terms
}

/// Anchor-grade terms justify an injection: identifier-like (digit, `_`, `-`,
/// `.`, or ALL-CAPS) or 6+ chars. Two short generic words never fire alone.
pub(crate) fn is_anchor(tok: &str) -> bool {
    tok.chars()
        .any(|c| c.is_ascii_digit() || c == '_' || c == '-' || c == '.')
        || (tok.len() >= 2
            && tok.chars().any(|c| c.is_ascii_uppercase())
            && !tok.chars().any(|c| c.is_ascii_lowercase()))
        || tok.chars().count() >= 6
}

/// Split a turn into lowercase tokens, keeping `_` and `-` so identifiers like
/// `cache-prod` and `TICKET-123` stay whole (caller lowercases the text first).
pub(crate) fn turn_tokens(text_low: &str) -> Vec<&str> {
    text_low
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| !t.is_empty())
        .collect()
}

/// Stem/prefix-aware token match (both args already lowercased): exact, or one
/// is a prefix of the other. Approximates porter's two-way stemming
/// (`deploy`↔`deployed`, `service`↔`services`) while rejecting the substring
/// false positive the old `contains` had (`spec` is not a prefix of `respect`).
pub(crate) fn token_matches(tok: &str, term: &str) -> bool {
    tok == term || tok.starts_with(term) || term.starts_with(tok)
}

/// Separator-insensitive `token_matches`: folds `-`, `_`, `.` together so the
/// recall post-filter agrees with FTS5 `unicode61` (which splits all three) — a
/// stored `cache_prod` matches a prompt's `cache-prod`. Recall-only; it re-checks
/// rows FTS already returned, so it can re-admit a hit but never widen the set.
pub(crate) fn token_matches_loose(tok: &str, term: &str) -> bool {
    fn norm(s: &str) -> std::borrow::Cow<'_, str> {
        if s.bytes().any(|b| b == b'_' || b == b'.') {
            std::borrow::Cow::Owned(s.replace(['_', '.'], "-"))
        } else {
            std::borrow::Cow::Borrowed(s)
        }
    }
    let (a, b) = (norm(tok), norm(term));
    token_matches(a.as_ref(), b.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_matches_is_stem_prefix_not_substring() {
        // Porter-style two-way prefix — the point of the stem-aware gate.
        assert!(token_matches("deployed", "deploy"));
        assert!(token_matches("deploy", "deployed"));
        assert!(token_matches("services", "service"));
        assert!(token_matches("cache-prod", "cache-prod"));
        // The substring false positive the old `contains` allowed is now rejected.
        assert!(!token_matches("respect", "spec"));
        assert!(!token_matches("spec", "respect"));
    }

    #[test]
    fn token_matches_loose_folds_separator_variants() {
        // FTS5 unicode61 splits -, _, . alike; the post-filter must agree so the
        // same identifier written with a different separator still matches.
        assert!(token_matches_loose("cache_prod", "cache-prod"));
        assert!(token_matches_loose("cache-prod", "cache_prod"));
        assert!(token_matches_loose("rewst.prod", "rewst-prod"));
        // Stem/prefix still works; the substring false positive stays rejected.
        assert!(token_matches_loose("deployed", "deploy"));
        assert!(!token_matches_loose("respect", "spec"));
    }

    #[test]
    fn distinctive_keeps_identifiers_and_long_words_drops_stopwords() {
        assert!(is_distinctive("cache-prod")); // hyphenated identifier
        assert!(is_distinctive("TICKET-123")); // digits
        assert!(is_distinctive("OAuth")); // mixed-case 4+
        assert!(is_distinctive("DR")); // ALL-CAPS 2+
        assert!(is_distinctive("rollout")); // 4+ word
        assert!(!is_distinctive("the")); // stopword
        assert!(!is_distinctive("use")); // stopword (would pass the 4+ rule otherwise — it's 3)
        assert!(!is_distinctive("go")); // short + stopword
        assert!(!is_distinctive("did")); // stopword
    }

    #[test]
    fn extract_terms_dedups_case_insensitively_keeps_first_casing() {
        let terms = extract_terms("Deploy the cache-prod, then redeploy CACHE-PROD again");
        // `the`/`then`/`again` dropped (stopword / `again` kept? 5 chars, not a stopword)
        assert_eq!(terms, vec!["Deploy", "cache-prod", "redeploy", "again"]);
    }
}
