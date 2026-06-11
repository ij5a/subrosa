//! Mask the highest-value secret shapes before they land in the archive.
//! Conservative on purpose: these patterns almost never match prose, and only
//! the secret VALUE is replaced — surrounding words stay searchable. The source
//! transcripts are still cleartext; disk encryption is the real at-rest control.

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
        (
            Regex::new(
                r"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|mysql_pwd|access[_-]?key)\b(\s*[=:]\s*)(\S+)",
            )
            .unwrap(),
            "${1}${2}‹redacted›",
        ),
    ]
});

pub fn redact(text: &str) -> String {
    REDACTIONS
        .iter()
        .fold(text.to_string(), |acc, (pat, repl)| {
            pat.replace_all(&acc, *repl).into_owned()
        })
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

    #[test]
    fn leaves_prose_alone() {
        let s = "the token bucket algorithm rate-limits requests";
        assert_eq!(redact(s), s); // "token" without =/: value attached stays
    }
}
