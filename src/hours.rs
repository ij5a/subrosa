//! `subrosa hours`: active time for 1 local-time month, from typed-prompt timestamps.

use std::process::ExitCode;

use rusqlite::params;

use crate::{db, text, timeutil};

const IDLE_MS: i64 = 30 * 60 * 1000;
const TAIL_MS: i64 = 2 * 60 * 1000;

// The UTC bounds are 1 day wider than the month, because a local month can
// start or end up to 1 day away from the UTC month.
const ROWS_SQL: &str = "SELECT project, session_id, CAST(round(unixepoch(ts, 'subsec') * 1000) AS INTEGER) AS ms, strftime('%Y-%m-%d %H:%M', ts, 'localtime') FROM turns
     WHERE role = 'user' AND is_sidechain = 0 AND ts >= date(?1 || '-01', '-1 day') AND ts < date(?1 || '-01', '+1 month', '+1 day')
       AND strftime('%Y-%m', ts, 'localtime') = ?1 AND (?2 IS NULL OR project LIKE ?2)
       AND text NOT GLOB '↪*' AND text NOT GLOB '<system-reminder>*' AND text NOT GLOB '<local-command*' AND text NOT GLOB 'Caveat:*' AND text NOT GLOB '<task-notification>*'
     ORDER BY ms, session_id";

struct Group {
    key: String,
    project: String,
    sid: String,
    started: String,
    stamps: Vec<i64>,
}

pub fn run(month: &str, project: Option<&str>, by_session: bool) -> ExitCode {
    let Some((y, mo, _)) = timeutil::parse_ymd(&format!("{month}-01")) else {
        eprintln!("[subrosa] bad month (want YYYY-MM): {month}");
        return ExitCode::from(2);
    };
    let month = format!("{y:04}-{mo:02}");
    let conn = match db::connect_readonly() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (groups, all) = match rows(&conn, &month, project, by_session) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[subrosa] query error: {e}");
            return ExitCode::FAILURE;
        }
    };
    if all.is_empty() {
        println!("[subrosa] no typed prompts in {month}.");
        return ExitCode::SUCCESS;
    }
    for g in &groups {
        let (n, h) = (g.stamps.len(), hours(&g.stamps));
        if by_session {
            let sid = text::sid8(&g.sid);
            println!(
                "{} · {} · {sid} · {n} prompt(s) · {h:.1} h",
                g.started, g.project
            );
        } else {
            println!("{} · {n} prompt(s) · {h:.1} h", g.project);
        }
    }
    println!();
    println!(
        "[subrosa] {month} · {} prompt(s) · {:.1} h total (one merged timeline)",
        all.len(),
        hours(&all)
    );
    ExitCode::SUCCESS
}

/// The groups in first-seen order, and every stamp in time order.
fn rows(
    conn: &rusqlite::Connection,
    month: &str,
    project: Option<&str>,
    by_session: bool,
) -> rusqlite::Result<(Vec<Group>, Vec<i64>)> {
    let mut stmt = conn.prepare(ROWS_SQL)?;
    let mut rows = stmt.query(params![month, project.map(|p| format!("%{p}%"))])?;
    let (mut groups, mut all): (Vec<Group>, Vec<i64>) = (Vec::new(), Vec::new());
    while let Some(r) = rows.next()? {
        let project = r.get::<_, Option<String>>(0)?.unwrap_or_default();
        let project = if project.is_empty() {
            "?".to_string()
        } else {
            project
        };
        let sid: String = r.get(1)?;
        let ms: i64 = r.get(2)?;
        let key = if by_session { &sid } else { &project };
        match groups.iter_mut().find(|g| g.key == *key) {
            Some(g) => g.stamps.push(ms),
            None => groups.push(Group {
                key: key.clone(),
                started: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                stamps: vec![ms],
                project,
                sid,
            }),
        }
        all.push(ms);
    }
    Ok((groups, all))
}

fn hours(stamps: &[i64]) -> f64 {
    credited(stamps) as f64 / 3_600_000.0
}

/// The first stamp, and a stamp after a gap over `IDLE_MS`, add `TAIL_MS`.
/// Every other stamp adds its gap.
fn credited(stamps: &[i64]) -> i64 {
    let mut total = if stamps.is_empty() { 0 } else { TAIL_MS };
    for w in stamps.windows(2) {
        let gap = w[1] - w[0];
        total += if gap <= IDLE_MS { gap } else { TAIL_MS };
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credited_caps_idle_gaps() {
        assert_eq!(credited(&[]), 0);
        assert_eq!(credited(&[5]), 120_000);
        assert_eq!(credited(&[0, 1_800_000]), 1_920_000);
        assert_eq!(credited(&[0, 1_800_001]), 240_000);
    }
}
