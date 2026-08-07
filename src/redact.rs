//! Mask the highest-value secret shapes before they land in the archive.
//! Conservative on purpose: these patterns almost never match prose, and only
//! the secret VALUE is replaced — surrounding words stay searchable. The source
//! transcripts are still cleartext; disk encryption is the real at-rest control.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

static REDACTIONS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (
            Regex::new(
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            )
            .unwrap(),
            "‹redacted-private-key›",
        ),
        (
            Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").unwrap(),
            "‹redacted-aws-key›",
        ),
        (
            Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._\-]{20,}").unwrap(),
            "Bearer ‹redacted›",
        ),
        // A passphrase may contain spaces, so it has no end this side of the
        // newline — take the whole rest of the line. Quotes get masked along
        // with everything else, which is the point: no pattern here tries to
        // find where a quoted value ends. One that did would close on the next
        // opening quote whenever the first is unterminated, and archive the
        // secret in between — and unterminated quotes are routine, because
        // ingest caps tool blocks mid-value before this runs.
        //
        // Neither side of the separator crosses a newline: "the passphrase:"
        // ending a sentence must not wipe the next line, and a "passphrase"
        // heading over a "=====" underline must not either.
        //
        // The boundaries are ASCII-only ((?-u:\b)) while \w stays Unicode.
        // Every turn carries ⚙/↪/…, and a Unicode \b puts the whole match on
        // the regex crate's slow path — measurably so at ingest. ASCII \b can
        // only match in more places here, never fewer, so nothing narrows.
        //
        // Runs before the generic rule, so it wins.
        (
            Regex::new(r"(?i)(?-u:\b)(\w*pass_?phrase)(?-u:\b)([^\S\n]*[=:][^\S\n]*)(.+)").unwrap(),
            "${1}${2}‹redacted›",
        ),
        // The leading \w* is load-bearing: `_` is a word character, so a bare
        // \b never matches inside MYSQL_PASSWORD= or api_token=. The whole key
        // name stays in group 1, so it's still readable.
        //
        // ponytail: one whitespace-delimited value, so `password="two words"`
        // masks only `"two`. Widening this to understand quoting is what
        // opened the leak described above; a real fix needs a shell-aware
        // tokenizer, not a bigger regex.
        (
            Regex::new(
                r"(?i)(?-u:\b)(\w*(?:password|passwd|pwd|secret|token|api[_-]?key|mysql_pwd|access[_-]?key))(?-u:\b)(\s*[=:]\s*)(\S+)",
            )
            .unwrap(),
            "${1}${2}‹redacted›",
        ),
    ]
});

/// Borrows straight through when nothing matches — the overwhelmingly common
/// case — so clean turns cost zero copies here.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut acc = Cow::Borrowed(text);
    for (pat, repl) in REDACTIONS.iter() {
        if pat.is_match(&acc) {
            acc = Cow::Owned(pat.replace_all(&acc, *repl).into_owned());
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn masks_common_secret_shapes() {
        assert_eq!(
            redact("key AKIAIOSFODNN7EXAMPLE ok"),
            "key ‹redacted-aws-key› ok"
        );
        assert_eq!(redact("password=hunter2 rest"), "password=‹redacted› rest");
        assert_eq!(
            redact("Authorization: Bearer abcdefghijklmnopqrstu.vwxyz"),
            "Authorization: Bearer ‹redacted›"
        );
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nzzz\n-----END RSA PRIVATE KEY-----";
        assert_eq!(redact(pem), "‹redacted-private-key›");
    }

    /// subrosa's own mirror passphrase, in every shape a session can show it.
    /// Two things make these match: `_` is a word character, so the pattern
    /// has to allow a prefix before the keyword; and a passphrase may contain
    /// spaces, so a value pattern that stops at the first one leaks the rest.
    /// Quoted shapes come out fully masked, quotes included — nothing here
    /// tries to work out where a quoted value ends.
    #[test]
    fn masks_its_own_passphrase() {
        let secret = "correct horse battery staple";
        for line in [
            "SUBROSA_MIRROR_PASSPHRASE=\"correct horse battery staple\" subrosa backup --force",
            "SUBROSA_MIRROR_PASSPHRASE=correct horse battery staple subrosa backup --force",
            "mirror_passphrase=correct horse battery staple",
            "export SUBROSA_MIRROR_PASSPHRASE='correct horse battery staple'",
            "passphrase: correct horse battery staple",
            r#"SUBROSA_MIRROR_PASSPHRASE="correct \"horse battery staple\"" subrosa backup"#,
            r#"export SUBROSA_MIRROR_PASSPHRASE='correct \'horse battery staple\''"#,
        ] {
            let got = redact(line);
            for word in secret.split(' ') {
                assert!(!got.contains(word), "leaked {word:?} from {line:?}: {got}");
            }
            assert!(
                got.ends_with("‹redacted›") && !got.contains('"') && !got.contains('\''),
                "the value should be masked to end of line, quotes included: {got}"
            );
        }
        assert!(!redact("export MYSQL_PASSWORD=hunter2").contains("hunter2"));
    }

    /// An unterminated quote used to close on the NEXT value's opening quote,
    /// leaving that secret in the clear. No quoted branch exists now, so the
    /// second key matches the generic rule on its own.
    #[test]
    fn an_unterminated_quote_cannot_expose_a_later_value_on_the_same_line() {
        let got = redact(r#"API_KEY="abc PASSWORD="s3cr3t-live" x"#);
        assert!(!got.contains("s3cr3t-live"), "leaked: {got}");
        assert!(got.contains("PASSWORD"), "ate the next key name: {got}");
    }

    /// A sentence ending in "the passphrase:" must not wipe the line after it,
    /// and neither must a "passphrase" heading over a "=====" underline —
    /// so nothing on either side of the separator crosses a newline.
    #[test]
    fn the_passphrase_separator_does_not_cross_a_newline() {
        for s in [
            "set the mirror passphrase:\nsubrosa backup --force",
            "passphrase\n=====\nbody text here",
        ] {
            assert_eq!(redact(s), s, "masked across a newline: {s:?}");
        }
    }

    /// A turn is every block joined with "\n", and ingest caps tool blocks
    /// before this runs — so an unterminated quote is something subrosa
    /// manufactures. A quoted value that could cross the newline would run on
    /// to the next line's opening quote and archive the secret in between.
    #[test]
    fn an_unterminated_quote_cannot_swallow_the_next_line() {
        let turn = "⚙ Bash API_KEY=\"abcdefgh…\n↪ ok\nPASSWORD=\"s3cr3t-live-value\" done";
        let got = redact(turn);
        assert!(!got.contains("s3cr3t-live-value"), "leaked: {got}");
        assert!(got.contains("PASSWORD"), "ate the next key name: {got}");
        assert!(got.contains("↪ ok"), "swallowed a line: {got}");
        assert_eq!(got.lines().count(), 3, "lines went missing: {got}");
    }

    /// Only passphrase keys eat the rest of the line. A generic key takes one
    /// whitespace-delimited value and leaves the sentence after it searchable —
    /// which is also why a quoted value with a space in it only masks its
    /// first word. That limitation is the price of never running past the
    /// value; see the ponytail note on the pattern.
    #[test]
    fn generic_keys_take_one_value_and_leave_the_sentence() {
        assert_eq!(
            redact("token: abc123 and then we deployed the service"),
            "token: ‹redacted› and then we deployed the service"
        );
        assert_eq!(
            redact("password=\"hunter 2\" rest"),
            "password=‹redacted› 2\" rest"
        );
    }

    #[test]
    fn leaves_prose_alone() {
        let s = "the token bucket algorithm rate-limits requests";
        assert_eq!(redact(s), s); // "token" without =/: value attached stays
    }
}
