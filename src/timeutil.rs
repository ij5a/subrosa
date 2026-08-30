//! Shared ISO-8601 ↔ Unix-epoch helpers. No chrono — hand-rolled to keep the
//! dependency tree small: parse a stored timestamp (Z or ±HH:MM offset) to a
//! Unix epoch i64, read the wall clock, and humanize an age. Used by the
//! dashboard (stats) and by recall (recency tie-break + age hint), so it lives
//! apart from either module. `now_unix` honors a `SUBROSA_NOW` epoch override —
//! a global wall-clock seam for deterministic tests that also shifts the
//! dashboard's "today" at runtime.

use std::time::{SystemTime, UNIX_EPOCH};

/// Parse an ISO-8601 string (Z or ±HH:MM offset) to a Unix timestamp (seconds).
pub(crate) fn parse_ts(ts: &str) -> Option<i64> {
    let ts = ts.trim();
    if ts.is_empty() {
        return None;
    }
    let (dt_part, offset_secs) = split_iso_offset(ts)?;
    let (date_str, time_str) = if let Some(p) = dt_part.find('T') {
        (&dt_part[..p], &dt_part[p + 1..])
    } else {
        (dt_part, "")
    };
    let (y_s, rest) = date_str.split_once('-')?;
    let (mo_s, d_s) = rest.split_once('-')?;
    let y: i64 = y_s.parse().ok()?;
    let mo: u32 = mo_s.parse().ok()?;
    let d: u32 = d_s.parse().ok()?;
    let (h, mi, s) = if !time_str.is_empty() {
        let tp: Vec<&str> = time_str.splitn(3, ':').collect();
        let h: i64 = tp.first().and_then(|p| p.parse().ok()).unwrap_or(0);
        let mi: i64 = tp.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
        let s: i64 = tp.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
        (h, mi, s)
    } else {
        (0i64, 0i64, 0i64)
    };
    let days = civil_to_days(y, mo, d)?;
    Some(days * 86_400 + h * 3600 + mi * 60 + s - offset_secs)
}

// Split "YYYY-MM-DDTHH:MM:SS±HH:MM" or "…Z" into (datetime_part, offset_seconds).
fn split_iso_offset(ts: &str) -> Option<(&str, i64)> {
    if let Some(stripped) = ts.strip_suffix('Z') {
        return Some((stripped, 0));
    }
    // After the 'T', look for the first '+' or '-' that marks the timezone offset.
    let t_pos = ts.find('T').unwrap_or(0);
    let after_t = &ts[t_pos..];
    // Skip the 'T' character itself when scanning for the offset sign.
    if let Some(rel) = after_t[1..].find('+') {
        let split = t_pos + 1 + rel;
        return Some((&ts[..split], parse_hhmm_offset(&ts[split + 1..], 1)?));
    }
    // The date portion contains '-', so only look for '-' after position t_pos+1.
    if let Some(rel) = after_t[1..].find('-') {
        let split = t_pos + 1 + rel;
        return Some((&ts[..split], parse_hhmm_offset(&ts[split + 1..], -1)?));
    }
    Some((ts, 0)) // no offset — treat as UTC
}

fn parse_hhmm_offset(s: &str, sign: i64) -> Option<i64> {
    // Accepts "HH:MM" or "HHMM".
    let s = s.trim_end_matches(|c: char| !c.is_ascii_digit());
    let (h, m) = if s.contains(':') {
        let mut it = s.splitn(2, ':');
        let h: i64 = it.next()?.parse().ok()?;
        let m: i64 = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (h, m)
    } else if s.len() >= 4 {
        let h: i64 = s[..2].parse().ok()?;
        let m: i64 = s[2..4].parse().ok()?;
        (h, m)
    } else {
        let h: i64 = s.parse().ok()?;
        (h, 0)
    };
    Some(sign * (h * 3600 + m * 60))
}

// Proleptic Gregorian date → days since Unix epoch (1970-01-01).
pub(crate) fn civil_to_days(y: i64, mo: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (y, m) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * m as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Parse a strict `YYYY-MM-DD` UTC date into civil parts, range-checking the
/// month and day. The input form for the `--after`/`--before` archive filters.
pub(crate) fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let (y_s, rest) = s.split_once('-')?;
    let (mo_s, d_s) = rest.split_once('-')?;
    let y: i64 = y_s.parse().ok()?;
    let mo: u32 = mo_s.parse().ok()?;
    let d: u32 = d_s.parse().ok()?;
    civil_to_days(y, mo, d)?; // rejects out-of-range month/day
    Some((y, mo, d))
}

/// The calendar day after `(y, mo, d)`, formatted `YYYY-MM-DD`. The exclusive
/// upper bound for an inclusive `--before D` — comparing `ts < next_day` keeps the
/// whole of day D and is robust to sub-second timestamps.
pub(crate) fn next_day(y: i64, mo: u32, d: u32) -> Option<String> {
    let (ny, nmo, nd) = civil_from_days(civil_to_days(y, mo, d)? + 1);
    Some(format!("{ny:04}-{nmo:02}-{nd:02}"))
}

// Zero-padding the bounds keeps the lexical timestamp comparison correct.
pub(crate) fn date_bounds<'a>(
    after: Option<&'a str>,
    before: Option<&'a str>,
) -> Result<(Option<String>, Option<String>), &'static str> {
    let after = after
        .map(|s| {
            parse_ymd(s)
                .map(|(y, mo, d)| format!("{y:04}-{mo:02}-{d:02}"))
                .ok_or("--after")
        })
        .transpose()?;
    let before = before
        .map(|s| {
            parse_ymd(s)
                .and_then(|(y, mo, d)| next_day(y, mo, d))
                .ok_or("--before")
        })
        .transpose()?;
    Ok((after, before))
}

// Howard Hinnant's days-to-civil (public domain) — the inverse of civil_to_days.
// Shared by now_iso (db), the dashboard (stats), and search/sessions date bounds.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The wall clock as a Unix timestamp (seconds). Honors a `SUBROSA_NOW` epoch
/// override when it is set and parses as an i64 — a global test seam (it also
/// shifts the dashboard's "today"/age at runtime). Empty or unparseable values
/// fall back to the real clock, so a typo can't silently zero the clock.
pub(crate) fn now_unix() -> i64 {
    if let Some(epoch) = std::env::var("SUBROSA_NOW")
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
    {
        return epoch;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Humanize an age in seconds to a compact relative string for recall lines:
/// `today` (same day) then `Nd` / `Nw` / `Nmo` / `Ny`, on integer days. Bucket
/// edges are chosen so no unit ever renders zero. Pure integer math (no chrono).
pub(crate) fn fmt_age(secs: i64) -> String {
    let days = secs.max(0) / 86_400;
    if days == 0 {
        "today".to_string()
    } else if days < 7 {
        format!("{days}d")
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

/// The parenthesized age suffix shared by recall and search: " (today)" the same
/// day, " ({age} old)" otherwise. Leading space so it drops in right after the
/// timestamp. `secs` is `now - record_epoch`.
pub(crate) fn age_suffix(secs: i64) -> String {
    let a = fmt_age(secs);
    if a == "today" {
        " (today)".to_string()
    } else {
        format!(" ({a} old)")
    }
}

/// Display a stored ISO timestamp as `YYYY-MM-DD HH:MM` (stored zone, ~UTC) — the
/// shared convention for `search`, `related`, `sessions`, and the dashboard.
/// Empty in, `?` out; a string shorter than 16 chars passes through unchanged.
pub(crate) fn fmt_ts(ts: &str) -> String {
    if ts.is_empty() {
        return "?".to_string();
    }
    ts.get(..16)
        .map(|s| s.replace('T', " "))
        .unwrap_or_else(|| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_z_and_offset_forms() {
        // Z and +00:00 resolve to the same epoch.
        let z = parse_ts("2026-06-12T06:20:02Z").unwrap();
        let off = parse_ts("2026-06-12T06:20:02+00:00").unwrap();
        assert_eq!(z, off);
        // A +02:00 zone is two hours earlier in UTC than the same wall clock at Z.
        let plus2 = parse_ts("2026-06-12T06:20:02+02:00").unwrap();
        assert_eq!(plus2, z - 2 * 3600);
        // Midnight UTC lands on a day boundary. (Stored timestamps always carry a
        // 'T'; a bare date is not a supported input, matching the pre-extraction code.)
        assert_eq!(parse_ts("2026-06-12T00:00:00Z").unwrap() % 86_400, 0);
        // Junk is rejected, not guessed.
        assert!(parse_ts("not-a-date").is_none());
        assert!(parse_ts("").is_none());
    }

    #[test]
    fn civil_to_days_anchors_at_epoch() {
        assert_eq!(civil_to_days(1970, 1, 1), Some(0));
        assert_eq!(civil_to_days(1970, 1, 2), Some(1));
        assert_eq!(civil_to_days(1969, 12, 31), Some(-1));
    }

    #[test]
    fn fmt_age_buckets_have_no_zero_unit() {
        let day = 86_400;
        assert_eq!(fmt_age(0), "today");
        assert_eq!(fmt_age(day - 1), "today"); // under a day rounds to same-day
        assert_eq!(fmt_age(day), "1d");
        assert_eq!(fmt_age(6 * day), "6d");
        assert_eq!(fmt_age(7 * day), "1w"); // weeks start at d=7, never 0w
        assert_eq!(fmt_age(29 * day), "4w");
        assert_eq!(fmt_age(30 * day), "1mo"); // months start at d=30, never 0mo
        assert_eq!(fmt_age(59 * day), "1mo");
        assert_eq!(fmt_age(60 * day), "2mo");
        assert_eq!(fmt_age(364 * day), "12mo");
        assert_eq!(fmt_age(365 * day), "1y"); // years start at d=365, never 0y
        assert_eq!(fmt_age(730 * day), "2y");
        assert_eq!(fmt_age(-5 * day), "today"); // future-dated clamps to today
    }

    #[test]
    fn now_unix_honors_parseable_subrosa_now_else_real_clock() {
        let key = "SUBROSA_NOW";
        let saved = std::env::var(key).ok();

        std::env::set_var(key, "1799999999");
        assert_eq!(now_unix(), 1_799_999_999, "set + parseable is honored");

        std::env::set_var(key, "");
        assert!(now_unix() > 1_700_000_000, "empty falls back to real clock");

        std::env::set_var(key, "not-a-number");
        assert!(
            now_unix() > 1_700_000_000,
            "garbage falls back to real clock"
        );

        match saved {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
