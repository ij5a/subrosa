//! BERT WordPiece tokenizer for the bundled embedder. Hand-rolled: the
//! `tokenizers` crate builds C oniguruma, which the pure-Rust dep tree says no
//! to, and an uncased English vocab needs about a hundred lines.

use std::collections::HashMap;

/// BERT drops a word this long rather than piece it up.
const MAX_WORD_CHARS: usize = 100;

pub struct Vocab {
    ids: HashMap<String, u32>,
    cls: u32,
    sep: u32,
    unk: u32,
    pub pad: u32,
}

impl Vocab {
    /// vocab.txt is one token per line and the line number IS the token id, so
    /// the special ids are looked up rather than assumed.
    pub fn parse(text: &str) -> Result<Vocab, String> {
        let ids: HashMap<String, u32> = text
            .lines()
            .enumerate()
            .map(|(i, line)| (line.trim_end_matches('\r').to_string(), i as u32))
            .collect();
        let id = |t: &str| {
            ids.get(t)
                .copied()
                .ok_or_else(|| format!("vocab.txt has no {t} token"))
        };
        Ok(Vocab {
            cls: id("[CLS]")?,
            sep: id("[SEP]")?,
            unk: id("[UNK]")?,
            pad: id("[PAD]")?,
            ids,
        })
    }

    /// `[CLS]` + the word pieces + `[SEP]`, truncated so the whole sequence
    /// fits `max` — the model has no positions past its training length.
    pub fn encode(&self, text: &str, max: usize) -> Vec<u32> {
        let body = max.saturating_sub(2);
        let mut out = vec![self.cls];
        'words: for word in basic_tokens(text) {
            for id in self.pieces(&word) {
                if out.len() > body {
                    break 'words;
                }
                out.push(id);
            }
        }
        out.push(self.sep);
        out
    }

    /// Greedy longest-match WordPiece: the longest prefix in the vocab wins,
    /// every piece after the first carries the `##` continuation marker, and a
    /// word with any unmatched piece is `[UNK]` whole.
    fn pieces(&self, word: &str) -> Vec<u32> {
        let cs: Vec<char> = word.chars().collect();
        if cs.len() > MAX_WORD_CHARS {
            return vec![self.unk];
        }
        let mut out = Vec::new();
        let mut start = 0;
        while start < cs.len() {
            let mut end = cs.len();
            let hit = loop {
                if end == start {
                    break None;
                }
                let body: String = cs[start..end].iter().collect();
                let piece = if start == 0 {
                    body
                } else {
                    format!("##{body}")
                };
                if let Some(&id) = self.ids.get(&piece) {
                    break Some((id, end));
                }
                end -= 1;
            };
            match hit {
                Some((id, next)) => {
                    out.push(id);
                    start = next;
                }
                None => return vec![self.unk],
            }
        }
        out
    }
}

/// BERT's basic tokenizer: drop control characters, split on whitespace,
/// lowercase (this vocab is uncased), fold accents, and cut punctuation and CJK
/// characters out as tokens of their own.
fn basic_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for raw in text.chars() {
        if raw == '\u{fffd}' || is_ignored(raw) || (raw.is_control() && !raw.is_whitespace()) {
            continue;
        }
        if raw.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        for lower in raw.to_lowercase() {
            let Some(c) = fold_accent(lower) else {
                continue;
            };
            if is_punct(c) || is_cjk(c) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            } else {
                cur.push(c);
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Invisible formatting characters, joiners and bidi marks — the kind that
/// arrive by copy-paste. They carry no meaning and would otherwise sit inside a
/// word and turn the whole word into `[UNK]`.
fn is_ignored(c: char) -> bool {
    matches!(c, '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

/// One ASCII base letter per Latin Extended-A code point, in block order.
/// Generated from the Unicode NFD decompositions; the few without one (`đ`,
/// `ħ`, `ł`, `ŋ`, `ŧ`, `ı`, `ſ`) map to the letter they're drawn from, and the
/// ligatures (`ĳ`, `œ`) to their first letter.
const LATIN_A: &[u8; 128] = b"AaAaAaCcCcCcCcDdDdEeEeEeEeEeGgGgGgGgHhHhIiIiIiIiIiIiJjKkkLlLlLlLlLlNnNnNnnNnOoOoOoOoRrRrRrSsSsSsSsTtTtTtUuUuUuUuUuUuWwYyYZzZzZzs";

/// `None` for a combining mark (dropped), otherwise the unaccented letter.
/// ponytail: Latin-1 plus Latin Extended-A plus the combining range, not full
/// NFD and no Unicode category tables. This vocab is English; anything outside
/// those blocks keeps its accent and at worst costs one odd split. A
/// unicode-normalization crate is the upgrade if ranking quality ever asks.
fn fold_accent(c: char) -> Option<char> {
    const ACCENTED: &str = "àáâãäåçèéêëìíîïñòóôõöùúûüýÿ";
    const PLAIN: &str = "aaaaaaceeeeiiiinooooouuuuyy";
    if ('\u{300}'..='\u{36f}').contains(&c) {
        return None;
    }
    if ('\u{100}'..='\u{17f}').contains(&c) {
        return Some(LATIN_A[c as usize - 0x100] as char);
    }
    match ACCENTED.chars().position(|a| a == c) {
        Some(i) => PLAIN.chars().nth(i),
        None => Some(c),
    }
}

/// ponytail: ASCII punctuation, the General Punctuation block (dashes, smart
/// quotes, ellipsis), the handful of Latin-1 marks, and the CJK/fullwidth
/// blocks. BERT splits on every Unicode `P*` category; the rest are rare in
/// transcripts and only shift where a word is cut.
fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(c, '\u{a1}' | '\u{b7}' | '\u{bf}' | '\u{2010}'..='\u{205e}'
            | '\u{3001}'..='\u{303f}'
            // Fullwidth punctuation only: the digits and letters interleaved
            // through this block are word characters and stay in the word.
            | '\u{ff01}'..='\u{ff0f}' | '\u{ff1a}'..='\u{ff20}'
            | '\u{ff3b}'..='\u{ff40}' | '\u{ff5b}'..='\u{ff65}')
}

/// CJK characters are one token each — they aren't whitespace-separated.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4e00..=0x9fff | 0x3400..=0x4dbf | 0xf900..=0xfaff
        | 0x20000..=0x2a6df | 0x2a700..=0x2b73f | 0x2b740..=0x2b81f
        | 0x2b820..=0x2ceaf | 0x2f800..=0x2fa1f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in vocab in the real layout: specials first, then whole words,
    /// then `##` continuations.
    fn vocab() -> Vocab {
        Vocab::parse(
            "[PAD]\n[UNK]\n[CLS]\n[SEP]\ndeploy\nfail\n.\n,\ncafe\n##ed\n##ing\n##s\nun\n##want\n",
        )
        .unwrap()
    }

    #[test]
    fn a_missing_special_token_is_an_error() {
        assert!(Vocab::parse("hello\nworld\n").is_err());
    }

    #[test]
    fn continuations_carry_the_hash_marker() {
        let v = vocab();
        // deploy + ##ed, wrapped in [CLS]/[SEP].
        assert_eq!(v.encode("deployed", 512), vec![2, 4, 9, 3]);
        assert_eq!(v.encode("unwanted", 512), vec![2, 12, 13, 9, 3]);
    }

    #[test]
    fn an_unknown_word_falls_back_to_unk_whole() {
        let v = vocab();
        assert_eq!(v.encode("kubernetes", 512), vec![2, 1, 3]);
        // A word longer than the cap is [UNK] without being pieced at all.
        assert_eq!(v.encode(&"deploy".repeat(30), 512), vec![2, 1, 3]);
    }

    #[test]
    fn text_is_lowercased_and_accents_folded() {
        let v = vocab();
        assert_eq!(v.encode("DEPLOY", 512), vec![2, 4, 3]);
        // Precomposed and decomposed é both fold to e, so both find "cafe".
        assert_eq!(v.encode("café", 512), vec![2, 8, 3]);
        assert_eq!(v.encode("cafe\u{301}", 512), vec![2, 8, 3]);
    }

    /// Latin Extended-A folds to the plain letters the vocab actually holds.
    #[test]
    fn latin_extended_a_folds_to_ascii() {
        assert_eq!(
            basic_tokens("Łódź café naïve"),
            vec!["lodz", "cafe", "naive"]
        );
        // The letters without an NFD decomposition still lose their stroke.
        assert_eq!(basic_tokens("Đđ Ħħ Ŋŋ ıſ"), vec!["dd", "hh", "nn", "is"]);
    }

    /// Invisible formatting characters are dropped outright — left in, they
    /// would glue into the word and make the whole thing [UNK].
    #[test]
    fn zero_width_and_format_characters_vanish() {
        let v = vocab();
        for glued in [
            "de\u{200b}ploy", // zero-width space
            "de\u{200d}ploy", // zero-width joiner
            "\u{feff}deploy", // BOM
            "de\u{ad}ploy",   // soft hyphen
            "\u{202a}deploy", // left-to-right embedding
            "de\u{2066}ploy", // left-to-right isolate
            "de\u{2062}ploy", // invisible times
        ] {
            assert_eq!(basic_tokens(glued), vec!["deploy"], "{glued:?}");
            assert_eq!(v.encode(glued, 512), vec![2, 4, 3], "{glued:?}");
        }
    }

    #[test]
    fn punctuation_splits_into_its_own_token() {
        let v = vocab();
        assert_eq!(v.encode("deploy, fail.", 512), vec![2, 4, 7, 5, 6, 3]);
        // Attached punctuation splits the same way it would with spaces.
        assert_eq!(basic_tokens("a.b"), vec!["a", ".", "b"]);
        assert_eq!(basic_tokens("hi\u{2014}there"), vec!["hi", "—", "there"]);
        // CJK and fullwidth punctuation cut a word the same way.
        assert_eq!(basic_tokens("deploy、fail"), vec!["deploy", "、", "fail"]);
        assert_eq!(basic_tokens("deploy！fail"), vec!["deploy", "！", "fail"]);
        assert_eq!(basic_tokens("¿deploy?"), vec!["¿", "deploy", "?"]);
        // Fullwidth digits and letters live in the same block but are word
        // characters — splitting them would make each one its own [UNK].
        assert_eq!(basic_tokens("ＡＢｃ１２３"), vec!["ａｂｃ１２３"]);
    }

    #[test]
    fn cjk_and_control_characters_are_handled_like_bert_does() {
        assert_eq!(basic_tokens("東京tower"), vec!["東", "京", "tower"]);
        assert_eq!(basic_tokens("a\u{0}\u{7}b\tc"), vec!["ab", "c"]);
        // The ideographic space separates words like any other whitespace.
        assert_eq!(basic_tokens("deploy\u{3000}fail"), vec!["deploy", "fail"]);
    }

    #[test]
    fn a_long_sequence_is_truncated_inside_the_limit() {
        let v = vocab();
        let ids = v.encode(&"deploy ".repeat(50), 8);
        assert_eq!(ids.len(), 8);
        assert_eq!(ids[0], 2, "[CLS] leads");
        assert_eq!(ids[7], 3, "[SEP] closes even after a cut");
        // The floor cases can't lose the specials either.
        assert_eq!(v.encode("deploy", 2), vec![2, 3]);
        assert_eq!(v.encode("deploy", 0), vec![2, 3]);
    }
}
