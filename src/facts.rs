//! Curated facts: the rows MEMORY.md is generated from. Used by the checkpoint
//! skills and by hand. Corrections soft-delete (status/superseded_at), never rm.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::ValueEnum;
use rusqlite::{params, Connection, OptionalExtension};

use crate::{db, paths};

#[derive(ValueEnum, Clone, Copy)]
pub enum FactAction {
    Upsert,
    Archive,
    Pin,
    Unpin,
    List,
}

#[derive(ValueEnum, Clone, Copy, PartialEq)]
pub enum StatusFilter {
    Active,
    Archived,
    All,
}

/// Ranking weight by fact type — guardrails/identity outrank look-ups when the
/// index is over budget.
pub fn type_weight(t: Option<&str>) -> i64 {
    match t.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "feedback" | "user" => 40,
        "project" => 20,
        "reference" => 10,
        _ => 15,
    }
}

/// Drop one matching pair of surrounding quotes — YAML quoting that shouldn't
/// leak into the stored value.
fn unquote(v: &str) -> String {
    let v = v.trim();
    let b = v.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// YAML-ish frontmatter reader for leaf files: only the keys we store, first
/// occurrence wins (so a nested `metadata: type:` can't override a flat one).
pub fn parse_frontmatter(text: &str) -> HashMap<String, String> {
    let mut fm = HashMap::new();
    if !text.starts_with("---") {
        return fm;
    }
    let body = &text[3..];
    let block = match body.find("\n---") {
        Some(end) => &body[..end],
        None => body,
    };
    for line in block.lines() {
        let trimmed = line.trim_start();
        for key in [
            "name",
            "type",
            "description",
            "originSessionId",
            "pinned",
            "tags",
        ] {
            if let Some(rest) = trimmed.strip_prefix(key) {
                if let Some(value) = rest.strip_prefix(':') {
                    fm.entry(key.to_string()).or_insert_with(|| unquote(value));
                    break;
                }
            }
        }
    }
    fm
}

/// Infer a fact type from the leaf filename prefix.
pub fn guess_type(leaf_name: &str) -> &'static str {
    ["feedback", "project", "reference", "user"]
        .into_iter()
        .find(|t| leaf_name.starts_with(t))
        .unwrap_or("reference")
}

/// The encoded project name a memdir belongs to: its parent directory's name.
pub fn project_of(memdir: &Path) -> String {
    memdir
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

struct Existing {
    id: i64,
    type_: Option<String>,
    title: Option<String>,
    hook: Option<String>,
    description: Option<String>,
}

/// Insert or update one fact. Precedence: an explicit flag always wins;
/// otherwise an update keeps the stored value and only falls back to
/// frontmatter/slug for a new fact (or when the stored value is empty) — so a
/// no-flag update can't rewrite a curated title into the `name` slug.
#[allow(clippy::too_many_arguments)]
fn upsert(
    conn: &Connection,
    project: &str,
    memdir: &Path,
    leaf: &str,
    type_: Option<&str>,
    title: Option<&str>,
    hook: Option<&str>,
    pin: bool,
    origin_session: Option<&str>,
) -> rusqlite::Result<()> {
    let origin_session: Option<String> = origin_session.map(str::to_string).or_else(|| {
        std::env::var("SUBROSA_ORIGIN_SESSION")
            .ok()
            .filter(|s| !s.is_empty())
    });
    let fm = std::fs::read_to_string(memdir.join(leaf))
        .map(|t| parse_frontmatter(&t))
        .unwrap_or_default();
    let now = db::now_iso();
    let row: Option<Existing> = conn
        .query_row(
            "SELECT id, type, title, hook, description FROM facts WHERE project=? AND leaf_path=?",
            params![project, leaf],
            |r| {
                Ok(Existing {
                    id: r.get(0)?,
                    type_: r.get(1)?,
                    title: r.get(2)?,
                    hook: r.get(3)?,
                    description: r.get(4)?,
                })
            },
        )
        .optional()?;

    let type_ = type_
        .map(str::to_string)
        .or_else(|| fm.get("type").cloned().filter(|s| !s.is_empty()))
        .or_else(|| {
            row.as_ref()
                .and_then(|r| r.type_.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| guess_type(leaf).to_string());
    let title = title
        .map(str::to_string)
        .or_else(|| {
            row.as_ref()
                .and_then(|r| r.title.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| {
            fm.get("name")
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    Path::new(leaf)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .replace('_', " ")
        });
    let hook = hook
        .map(str::to_string)
        .or_else(|| {
            row.as_ref()
                .and_then(|r| r.hook.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| fm.get("description").cloned().unwrap_or_default());
    let description: Option<String> = fm
        .get("description")
        .cloned()
        .or_else(|| row.as_ref().and_then(|r| r.description.clone()));

    let action = if let Some(r) = &row {
        conn.execute(
            "UPDATE facts SET type=?, title=?, hook=?, description=?, status='active', \
             superseded_at=NULL, pinned=MAX(pinned,?), updated_at=? WHERE id=?",
            params![type_, title, hook, description, pin as i64, now, r.id],
        )?;
        "updated"
    } else {
        // New facts append to the end of the curated order; updates keep position.
        let nxt: i64 = conn.query_row(
            "SELECT COALESCE(MAX(index_seq), -1) + 1 FROM facts WHERE project=?",
            [project],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO facts (project,name,type,title,hook,leaf_path,description,index_seq,\
             pinned,status,origin_session,created_at,updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?,'active',?,?,?)",
            params![
                project,
                fm.get("name"),
                type_,
                title,
                hook,
                leaf,
                description,
                nxt,
                pin as i64,
                origin_session,
                now,
                now
            ],
        )?;
        "inserted"
    };
    println!("[subrosa] {action}: {type_}: {title} ({leaf})");
    Ok(())
}

// (type, title, leaf_path, pinned, status)
type FactListRow = (Option<String>, Option<String>, Option<String>, i64, String);

fn list(conn: &Connection, project: &str, status: StatusFilter) -> rusqlite::Result<()> {
    let (where_sql, binds): (&str, Vec<String>) = match status {
        StatusFilter::All => ("project=?", vec![project.to_string()]),
        StatusFilter::Active => (
            "project=? AND status=?",
            vec![project.to_string(), "active".into()],
        ),
        StatusFilter::Archived => (
            "project=? AND status=?",
            vec![project.to_string(), "archived".into()],
        ),
    };
    let sql = format!(
        "SELECT type,title,leaf_path,pinned,status FROM facts WHERE {where_sql} \
         ORDER BY index_seq IS NULL, index_seq"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<FactListRow> = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<Result<_, _>>()?;
    for (type_, title, leaf, pinned, st) in rows {
        let flags = format!(
            "{}{}",
            if pinned != 0 { "📌" } else { "  " },
            if st == "active" { " " } else { "🗄 " }
        );
        println!(
            "{flags} [{}] {} ({})",
            type_.as_deref().unwrap_or(""),
            title.as_deref().unwrap_or(""),
            leaf.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

/// `subrosa fact <action> ...` entry point.
#[allow(clippy::too_many_arguments)]
pub fn run(
    action: FactAction,
    leaf: Option<String>,
    type_: Option<String>,
    title: Option<String>,
    hook: Option<String>,
    pin: bool,
    origin_session: Option<String>,
    project: Option<String>,
    memdir: Option<PathBuf>,
    status: StatusFilter,
) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    let memdir = memdir
        .map(|p| paths::expanduser(&p))
        .unwrap_or_else(|| db::current_memdir(Some(&conn)));
    let project = project.unwrap_or_else(|| project_of(&memdir));

    if let FactAction::List = action {
        return match list(&conn, &project, status) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[subrosa] list failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let Some(leaf) = leaf else {
        eprintln!("[subrosa] this action needs --leaf");
        return ExitCode::from(2);
    };
    if leaf.contains('/') || leaf.contains('\\') || leaf.contains("..") {
        eprintln!("[subrosa] refusing suspicious leaf name (must be a bare filename): {leaf:?}");
        return ExitCode::from(2);
    }

    let now = db::now_iso();
    let result = match action {
        FactAction::Upsert => upsert(
            &conn,
            &project,
            &memdir,
            &leaf,
            type_.as_deref(),
            title.as_deref(),
            hook.as_deref(),
            pin,
            origin_session.as_deref(),
        ),
        FactAction::Archive => conn
            .execute(
                "UPDATE facts SET status='archived', superseded_at=?, updated_at=? \
                 WHERE project=? AND leaf_path=?",
                params![now, now, project, leaf],
            )
            .map(|n| {
                if n > 0 {
                    println!("[subrosa] archived {n} fact(s): {leaf}");
                } else {
                    println!("[subrosa] no fact for {leaf}");
                }
            }),
        FactAction::Pin => conn
            .execute(
                "UPDATE facts SET pinned=1, updated_at=? WHERE project=? AND leaf_path=?",
                params![now, project, leaf],
            )
            .map(|n| println!("[subrosa] pinned {n} fact(s): {leaf}")),
        FactAction::Unpin => conn
            .execute(
                "UPDATE facts SET pinned=0, updated_at=? WHERE project=? AND leaf_path=?",
                params![now, project, leaf],
            )
            .map(|n| println!("[subrosa] unpinned {n} fact(s): {leaf}")),
        FactAction::List => unreachable!(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[subrosa] fact {e}");
            ExitCode::FAILURE
        }
    }
}
