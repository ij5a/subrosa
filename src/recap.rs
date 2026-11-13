//! `subrosa recap`: a session, or a date range of sessions, as its prompt trail.
//! It prints stored text only, which ingest already redacted.

use std::process::ExitCode;

use rusqlite::{params_from_iter, OptionalExtension};

use crate::session::{self, Resolved};
use crate::{db, ingest, text, timeutil};

// NOTE: same predicates as last_session_card in hook.rs. Share 1 const if a third caller appears.
const TURNS_SQL: &str = "SELECT seq, ts, role, text FROM turns
     WHERE session_id = ?1 AND is_sidechain = 0 AND text IS NOT NULL
       AND ((role = 'user' AND text NOT LIKE '↪%' AND text NOT LIKE '<%'
             AND text NOT LIKE '[Request interrupted%'
             AND text NOT LIKE 'This session is being continued%')
         OR (role = 'assistant' AND text NOT LIKE '⚙%'))
     ORDER BY seq";

/// One typed prompt and the first line of the final reply to it.
struct Exchange {
    seq: i64,
    ts: String,
    prompt: String,
    reply: Option<String>,
}

pub fn run(
    id: Option<&str>,
    project: Option<&str>,
    after: Option<&str>,
    before: Option<&str>,
    limit: usize,
) -> ExitCode {
    let (after_bound, before_bound) = match timeutil::date_bounds(after, before) {
        Ok(bounds) => bounds,
        Err(flag) => {
            let s = if flag == "--after" { after } else { before };
            eprintln!(
                "[subrosa] bad {flag} date (want YYYY-MM-DD): {}",
                s.unwrap_or("")
            );
            return ExitCode::from(2);
        }
    };
    let conn = match db::connect_readonly() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };

    let sessions = match id {
        Some(arg) => match session::resolve_session(&conn, arg) {
            Resolved::One(sid) => one_session(&conn, sid),
            Resolved::None => {
                eprintln!("[subrosa] no archived turns for session {arg} (not ingested?)");
                return ExitCode::FAILURE;
            }
            Resolved::Ambiguous(ids) => {
                session::print_ambiguous(arg, &ids);
                return ExitCode::from(2);
            }
        },
        None => list_sessions(&conn, project, &after_bound, &before_bound),
    };
    let sessions = match sessions {
        Ok(s) => s,
        Err(e) => return query_error(e),
    };

    let (mut printed, mut prompts, mut more) = (0, 0, false);
    for (sid, project) in sessions {
        let kept = match exchanges(&conn, &sid, &after_bound, &before_bound) {
            Ok(k) => k,
            Err(e) => return query_error(e),
        };
        if kept.is_empty() {
            continue;
        }
        if printed == limit {
            more = true;
            break;
        }
        println!(
            "{} · {} · {} prompt(s)",
            if project.is_empty() { "?" } else { &project },
            text::sid8(&sid),
            kept.len()
        );
        for e in &kept {
            println!("  #{} {}  {}", e.seq, timeutil::fmt_ts(&e.ts), e.prompt);
            if let Some(reply) = &e.reply {
                println!("     → {reply}");
            }
        }
        println!();
        printed += 1;
        prompts += kept.len();
    }
    if printed == 0 && !more {
        println!("[subrosa] no typed prompts match.");
        return ExitCode::SUCCESS;
    }
    println!(
        "[subrosa] {printed} session(s), {prompts} prompt(s).{}",
        if more {
            " More sessions match: raise -n."
        } else {
            ""
        }
    );
    ExitCode::SUCCESS
}

/// The resolved session with its project. A missing `sessions` row prints `?`.
fn one_session(
    conn: &rusqlite::Connection,
    sid: String,
) -> rusqlite::Result<Vec<(String, String)>> {
    let project: Option<Option<String>> = conn
        .query_row(
            "SELECT project FROM sessions WHERE session_id = ?",
            [sid.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(vec![(sid, project.flatten().unwrap_or_default())])
}

/// Sessions newest-first, selected the way `subrosa sessions` selects them.
fn list_sessions(
    conn: &rusqlite::Connection,
    project: Option<&str>,
    after: &Option<String>,
    before: &Option<String>,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut sql = String::from("SELECT session_id, project FROM sessions WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();
    if let Some(p) = project {
        sql.push_str(" AND project LIKE ?");
        binds.push(format!("%{p}%"));
    }
    if let Some(a) = after {
        sql.push_str(" AND last_ts >= ?");
        binds.push(a.clone());
    }
    if let Some(b) = before {
        sql.push_str(" AND first_ts < ?");
        binds.push(b.clone());
    }
    sql.push_str(" ORDER BY last_ts DESC");
    conn.prepare(&sql)?
        .query_map(params_from_iter(binds.iter()), |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?
        .collect()
}

/// A `user` row starts an exchange and the last `assistant` row before the next
/// one is its reply. The date bounds test the prompt, so a reply may fall outside.
fn exchanges(
    conn: &rusqlite::Connection,
    sid: &str,
    after: &Option<String>,
    before: &Option<String>,
) -> rusqlite::Result<Vec<Exchange>> {
    let mut stmt = conn.prepare_cached(TURNS_SQL)?;
    let mut rows = stmt.query([sid])?;
    let mut out: Vec<Exchange> = Vec::new();
    while let Some(r) = rows.next()? {
        let line = first_line(&r.get::<_, String>(3)?);
        if r.get::<_, String>(2)? == "user" {
            out.push(Exchange {
                seq: r.get(0)?,
                ts: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                prompt: line,
                reply: None,
            });
        } else if let Some(last) = out.last_mut() {
            last.reply = Some(line);
        }
    }
    out.retain(|e| {
        after.as_ref().is_none_or(|a| e.ts >= *a) && before.as_ref().is_none_or(|b| e.ts < *b)
    });
    Ok(out)
}

// NOTE: stored text does not mark thinking blocks, so one can show as the reply line. Fix at ingest if it matters.
// A reply often opens with `---` or a bare code fence, so a line with a letter or digit wins.
fn first_line(stored: &str) -> String {
    let line = stored
        .lines()
        .find(|l| l.chars().any(char::is_alphanumeric))
        .or_else(|| stored.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("");
    ingest::cap(&text::collapse_ws(line), 160)
}

fn query_error(e: rusqlite::Error) -> ExitCode {
    eprintln!("[subrosa] query error: {e}");
    ExitCode::FAILURE
}
