//! Shared ISO-8601 ↔ Unix-epoch helpers. No chrono — hand-rolled to keep the
//! dependency tree small: parse a stored timestamp (Z or ±HH:MM offset) to a
//! Unix epoch i64, and read the wall clock. Used by the dashboard (stats) and
//! by recall's recency tie-break, so it lives apart from either module.

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
    let dp: Vec<&str> = date_str.splitn(3, '-').collect();
    if dp.len() < 3 {
        return None;
    }
    let y: i64 = dp[0].parse().ok()?;
    let mo: u32 = dp[1].parse().ok()?;
    let d: u32 = dp[2].parse().ok()?;
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

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
}
