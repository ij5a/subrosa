//! Generate a project's MEMORY.md from the facts table, byte-budgeted so the
//! always-loaded index can never overflow. Facts are ranked by importance
//! (pinned > type weight > recency > hits) to decide what fits the budget,
//! then emitted in the curated index order. Facts below the budget stay in
//! the DB (searchable, archive-only) — nothing is deleted.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::facts::{project_of, type_weight};
use crate::{db, paths};

const HEADER: &str = "# Memory Index\n\n";
pub const DEFAULT_BUDGET: i64 = 23000;

struct Fact {
    type_: Option<String>,
    title: Option<String>,
    hook: Option<String>,
    leaf_path: Option<String>,
    index_seq: Option<i64>,
    pinned: i64,
    hits: i64,
    updated_at: Option<String>,
}

fn render_line(f: &Fact) -> String {
    format!(
        "- [{}]({}) — {}\n",
        f.title.as_deref().unwrap_or(""),
        f.leaf_path.as_deref().unwrap_or(""),
        f.hook.as_deref().unwrap_or("")
    )
}

// All components "higher = better"; compared descending. -index_seq makes the
// lower (earlier, more deliberately placed) index position win the final
// tiebreak; orphans/new sort last.
fn rank_key(f: &Fact) -> (i64, i64, String, i64, i64) {
    (
        f.pinned,
        type_weight(f.type_.as_deref()),
        f.updated_at.clone().unwrap_or_default(),
        f.hits,
        -(f.index_seq.unwrap_or(1_000_000_000)),
    )
}

// Curated index order; facts without an index position go after, keeping rank order.
fn display_key(f: &Fact) -> (bool, i64) {
    (f.index_seq.is_none(), f.index_seq.unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    project: Option<String>,
    memdir: Option<PathBuf>,
    budget: i64,
    out: Option<PathBuf>,
    include_orphans: bool,
    dry_run: bool,
) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Don't canonicalize: keep the canonical projects-dir name as the project key
    // even when the memory/ dir is reached through a symlink.
    let memdir = memdir
        .map(|p| paths::expanduser(&p))
        .unwrap_or_else(|| db::current_memdir(Some(&conn)));
    let project = project.unwrap_or_else(|| project_of(&memdir));
    let out = out.unwrap_or_else(|| memdir.join("MEMORY.md"));

    let rows: Result<Vec<Fact>, _> = conn
        .prepare(
            "SELECT type, title, hook, leaf_path, index_seq, pinned, hits, updated_at \
             FROM facts WHERE project=? AND status='active' AND superseded_at IS NULL",
        )
        .and_then(|mut s| {
            s.query_map([&project], |r| {
                Ok(Fact {
                    type_: r.get(0)?,
                    title: r.get(1)?,
                    hook: r.get(2)?,
                    leaf_path: r.get(3)?,
                    index_seq: r.get(4)?,
                    pinned: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    hits: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    updated_at: r.get(7)?,
                })
            })?
            .collect()
        });
    let facts = match rows {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[subrosa] query failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if facts.is_empty() {
        eprintln!("[subrosa] no active facts for project '{project}'");
        return ExitCode::FAILURE;
    }

    // Default: only facts already in the index (or pinned) compete for the budget,
    // so generation preserves the current index instead of surfacing previously
    // removed orphans. Orphans stay archive-only (still in the DB, still searchable).
    let (mut candidates, excluded): (Vec<Fact>, Vec<Fact>) = if include_orphans {
        (facts, Vec::new())
    } else {
        facts
            .into_iter()
            .partition(|f| f.index_seq.is_some() || f.pinned != 0)
    };

    candidates.sort_by_key(|f| std::cmp::Reverse(rank_key(f)));

    let budget_body = budget - HEADER.len() as i64;
    let (mut included, mut dropped, mut used) = (Vec::new(), Vec::new(), 0i64);
    for f in candidates {
        let n = render_line(&f).len() as i64;
        if used + n <= budget_body {
            used += n;
            included.push(f);
        } else {
            dropped.push(f);
        }
    }

    included.sort_by_key(display_key);
    let mut body = String::from(HEADER);
    for f in &included {
        body.push_str(&render_line(f));
    }
    let total = body.len();

    if dry_run {
        print!("{body}");
    } else if let Err(e) = std::fs::write(&out, &body) {
        eprintln!("[subrosa] cannot write {}: {e}", out.display());
        return ExitCode::FAILURE;
    }

    // Summary + dropped log to stderr (keeps --dry-run stdout clean for diffing).
    eprintln!(
        "[subrosa] project={project} kept={} dropped={} orphans_excluded={} bytes={total} \
         budget={budget} out={}",
        included.len(),
        dropped.len(),
        excluded.len(),
        out.display()
    );
    let risky: Vec<&Fact> = dropped
        .iter()
        .filter(|f| {
            f.pinned != 0
                || matches!(
                    f.type_
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .as_str(),
                    "feedback" | "user"
                )
        })
        .collect();
    if !risky.is_empty() {
        eprintln!(
            "[subrosa] WARNING: {} dropped fact(s) are pinned/feedback/user:",
            risky.len()
        );
        for f in &risky {
            eprintln!(
                "         !! {}: {} ({})",
                f.type_.as_deref().unwrap_or(""),
                f.title.as_deref().unwrap_or(""),
                f.leaf_path.as_deref().unwrap_or("")
            );
        }
    }
    if !dropped.is_empty() {
        eprintln!("[subrosa] dropped below budget (archive-only, still searchable):");
        for f in &dropped {
            eprintln!(
                "         - {}: {} ({})",
                f.type_.as_deref().unwrap_or(""),
                f.title.as_deref().unwrap_or(""),
                f.leaf_path.as_deref().unwrap_or("")
            );
        }
    }
    ExitCode::SUCCESS
}
