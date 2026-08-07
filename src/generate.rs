//! Generate a project's MEMORY.md from the facts table, byte-budgeted so the
//! always-loaded index can never overflow. Facts are ranked by importance
//! (pinned > type weight > recency > hits) to decide what fits the budget,
//! then emitted in the curated index order. Facts below the budget stay in
//! the DB (searchable, archive-only) — nothing is deleted.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::facts::{project_of, type_weight};
use crate::{db, paths};

const HEADER: &str = "# Memory Index\n\n";
pub const DEFAULT_BUDGET: i64 = 23000;

// Claude Code stops reading MEMORY.md somewhere around here. Warn-only: this
// byte figure is observed behaviour, not documented, so nothing enforces it.
pub const CC_LOAD_CAP: i64 = 25_000;

// The line limit is the confirmed one (anthropics/claude-code issue #25006),
// so selection does enforce it: a fact past line 200 is written but never read.
pub const CC_LOAD_LINES: usize = 200;

/// Per-project budget override: one number in `<memdir>/.budget`, at least
/// large enough to hold the header. `Ok` carries the budget plus whatever
/// complaint a bad value earned — the caller places that, because the hook
/// path must not write to stderr. Only a missing file means "no override": a
/// file that exists but won't read is an `Err`, so generate can refuse to
/// rewrite MEMORY.md against a number it had to guess.
pub fn resolve_budget(memdir: &Path) -> Result<(i64, Option<String>), String> {
    let path = memdir.join(".budget");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((DEFAULT_BUDGET, None)),
        Err(e) => return Err(format!("[subrosa] cannot read {}: {e}", path.display())),
    };
    match text.trim().parse::<i64>() {
        Ok(n) if n >= HEADER.len() as i64 => Ok((n, None)),
        _ => Ok((
            DEFAULT_BUDGET,
            Some(format!(
                "[subrosa] ignoring {}: want one byte count of {} or more — using {DEFAULT_BUDGET}",
                path.display(),
                HEADER.len()
            )),
        )),
    }
}

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

/// One fact is one line, always. A stored newline would split the entry in
/// two, breaking the index format and slipping past the line cap.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

fn render_line(f: &Fact) -> String {
    format!(
        "- [{}]({}) — {}\n",
        one_line(f.title.as_deref().unwrap_or("")),
        one_line(f.leaf_path.as_deref().unwrap_or("")),
        // Cap defensively too: rows may predate the upsert-side cap.
        one_line(&crate::facts::cap_hook(f.hook.as_deref().unwrap_or("")))
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
    budget: Option<i64>,
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
    let budget = match budget {
        Some(b) if b < HEADER.len() as i64 => {
            eprintln!(
                "[subrosa] --budget wants one byte count of {} or more",
                HEADER.len()
            );
            return ExitCode::FAILURE;
        }
        Some(b) => b,
        // Rewriting the index against a guessed budget is the harm here, so an
        // unreadable .budget stops the run instead of falling back.
        None => match resolve_budget(&memdir) {
            Ok((b, warn)) => {
                if let Some(w) = warn {
                    eprintln!("{w}");
                }
                b
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        },
    };
    if budget > CC_LOAD_CAP {
        eprintln!(
            "[subrosa] budget={budget} is above the ~{CC_LOAD_CAP}-byte load cap — bytes past \
             it (or past line 200) never reach Claude's context."
        );
    }

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
    // A fact has to fit both limits. Every fact renders as exactly one line, so
    // the line room is whatever the header leaves of CC_LOAD_LINES.
    let line_room = CC_LOAD_LINES.saturating_sub(HEADER.lines().count());
    let (mut included, mut dropped, mut used) = (Vec::new(), Vec::new(), 0i64);
    for f in candidates {
        let n = render_line(&f).len() as i64;
        if used + n <= budget_body && included.len() < line_room {
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
        eprintln!(
            "[subrosa] to keep more, raise the budget: echo <n> > {} (ceiling ~{CC_LOAD_CAP} \
             bytes / 200 lines — Claude stops reading past that)",
            memdir.join(".budget").display()
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// Unique throwaway dir per test so the parallel runner never races.
    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("subrosa-budget-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_budget_file_uses_the_default_and_says_nothing() {
        let d = tmpdir("missing");
        assert_eq!(resolve_budget(&d), Ok((DEFAULT_BUDGET, None)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn budget_file_wins_and_tolerates_whitespace() {
        let d = tmpdir("ok");
        std::fs::write(d.join(".budget"), "30000\n").unwrap();
        assert_eq!(resolve_budget(&d), Ok((30000, None)));
        std::fs::write(d.join(".budget"), " 24900 ").unwrap();
        assert_eq!(resolve_budget(&d), Ok((24900, None)));
        // The floor is the header itself: anything smaller can't hold a file.
        std::fs::write(d.join(".budget"), HEADER.len().to_string()).unwrap();
        assert_eq!(resolve_budget(&d), Ok((HEADER.len() as i64, None)));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Unparsable, non-positive, or below the header length falls back instead
    /// of emptying the index, and hands the caller a complaint to place.
    #[test]
    fn bad_budget_file_falls_back_and_reports() {
        let d = tmpdir("bad");
        for bad in ["junk", "0", "-5", "15"] {
            std::fs::write(d.join(".budget"), bad).unwrap();
            let (budget, warn) = resolve_budget(&d).expect("a readable file is never an error");
            assert_eq!(budget, DEFAULT_BUDGET, "input: {bad}");
            assert!(
                warn.is_some_and(|w| w.contains(".budget")),
                "input {bad} should have earned a complaint"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A .budget that exists but won't read is not "no budget" — guessing here
    /// would rewrite MEMORY.md against the wrong number. A directory is the
    /// portable way to make the read fail.
    #[test]
    fn unreadable_budget_file_is_an_error() {
        let d = tmpdir("unreadable");
        std::fs::create_dir(d.join(".budget")).unwrap();
        assert!(
            resolve_budget(&d).is_err_and(|e| e.contains(".budget")),
            "an unreadable .budget must not fall back silently"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
