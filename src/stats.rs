//! Activity dashboard: sparkline, store stats, MEMORY.md budget meter,
//! by-project share table. Also the default view for a bare `subrosa`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use rusqlite::Connection;

use crate::timeutil::{civil_from_days, civil_to_days, fmt_ts, now_unix, parse_ts, parse_ymd};
use crate::{db, embed, generate, ingest, paths};

// Path segments that name containers, not the project itself — dropped when shortening a label.
const CONTAINER_TOKENS: &[&str] = &[
    "users",
    "home",
    "desktop",
    "downloads",
    "documents",
    "library",
    "mobile documents",
    "com~apple~clouddocs",
    "clouddocs",
    "icloud",
    "git",
    "src",
    "code",
    "projects",
    "repos",
    "work",
    "dev",
    "claude",
    "github.com",
    "bitbucket.org",
    "gitlab.com",
];

// ---- CLI args ---------------------------------------------------------------

#[derive(clap::Args)]
pub struct Args {
    /// Show extra detail: archive span, by-role/type counts, paths
    #[arg(long)]
    pub detail: bool,

    /// Disable ANSI color
    #[arg(long)]
    pub no_color: bool,
}

// ---- color ------------------------------------------------------------------

// Thread-local flag set once at startup, read by every color helper.
use std::cell::Cell;

thread_local! {
    static USE_COLOR: Cell<bool> = const { Cell::new(false) };
}

fn color_on() {
    USE_COLOR.with(|c| c.set(true));
}

fn use_color() -> bool {
    USE_COLOR.with(|c| c.get())
}

// ANSI code lookup.
fn ansi(code: &str) -> &'static str {
    match code {
        "reset" => "0",
        "bold" => "1",
        "dim" => "2",
        "red" => "31",
        "green" => "32",
        "yellow" => "33",
        "blue" => "34",
        "magenta" => "35",
        "cyan" => "36",
        "white" => "37",
        "gray" => "90",
        "bgreen" => "92",
        "byellow" => "93",
        "bred" => "91",
        "bcyan" => "96",
        _ => "0",
    }
}

fn c1(text: &str, a: &str) -> String {
    if !use_color() {
        return text.to_string();
    }
    format!("\x1b[{}m{}\x1b[0m", ansi(a), text)
}

fn c2(text: &str, a: &str, b: &str) -> String {
    if !use_color() {
        return text.to_string();
    }
    format!("\x1b[{};{}m{}\x1b[0m", ansi(a), ansi(b), text)
}

// Visible width of a string: character count minus ANSI escape sequences.
fn vlen(s: &str) -> usize {
    let mut n = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if ch == '\x1b' {
            in_esc = true;
        } else if in_esc {
            if ch == 'm' {
                in_esc = false;
            }
        } else {
            n += 1;
        }
    }
    n
}

// ---- humanize ---------------------------------------------------------------

fn human(n: i64) -> String {
    let mut v = n as f64;
    for unit in &["", "K", "M", "B"] {
        if v.abs() < 1000.0 {
            return if unit.is_empty() {
                format!("{}", v as i64)
            } else {
                format!("{:.1}{}", v, unit)
            };
        }
        v /= 1000.0;
    }
    format!("{:.1}T", v)
}

/// Exact count, grouped in threes. The index line compares two numbers that
/// often differ by a handful of turns, and `human` rounds both to "6.0K" —
/// "6.0K of 6.0K done" reads as frozen exactly when someone is watching it.
fn commas(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = if n < 0 {
        "-".to_string()
    } else {
        String::new()
    };
    for (i, c) in digits.char_indices() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn human_bytes(n: u64, space: bool) -> String {
    let sep = if space { " " } else { "" };
    let mut v = n as f64;
    for unit in &["B", "KB", "MB", "GB"] {
        if v.abs() < 1000.0 {
            return if *unit == "B" {
                format!("{}{}{}", v as u64, sep, unit)
            } else {
                let s = strip_trailing_zeros(&format!("{:.1}", v));
                format!("{}{}{}", s, sep, unit)
            };
        }
        v /= 1000.0;
    }
    let s = strip_trailing_zeros(&format!("{:.1}", v));
    format!("{}{}TB", s, sep)
}

// Trim trailing zeros (then a bare dot) off a formatted float string.
fn strip_trailing_zeros(s: &str) -> String {
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

// ---- ISO-8601 timestamp helpers --------------------------------------------
// parse_ts / now_unix / civil_to_days / civil_from_days live in crate::timeutil
// (shared with recall, search, and sessions).

fn ago_secs(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return "just now".to_string();
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{}m ago", m);
    }
    let h = m / 60;
    if h < 24 {
        return format!("{}h ago", h);
    }
    format!("{}d ago", h / 24)
}

fn ago(ts: &str) -> String {
    parse_ts(ts)
        .map(|epoch| ago_secs(now_unix() - epoch))
        .unwrap_or_default()
}

// ---- progress meter ---------------------------------------------------------

fn meter(frac: f64, width: usize) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let color = if frac < 0.70 {
        "green"
    } else if frac < 0.90 {
        "yellow"
    } else {
        "red"
    };
    let bar: String = "█".repeat(filled);
    let rest: String = "░".repeat(width - filled);
    format!("{}{}", c1(&bar, color), c1(&rest, "gray"))
}

// ---- sparkline --------------------------------------------------------------

const SPARK: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn bucketize(vals: &[i64], nbins: usize) -> Vec<i64> {
    let n = vals.len();
    if n <= nbins {
        return vals.to_vec();
    }
    (0..nbins)
        .map(|i| vals[i * n / nbins..(i + 1) * n / nbins].iter().sum())
        .collect()
}

fn sparkline(counts: &[i64]) -> String {
    let hi = *counts.iter().max().unwrap_or(&0);
    if hi <= 0 {
        let ch: String = std::iter::repeat_n(SPARK[0], counts.len()).collect();
        return c2(&ch, "cyan", "dim");
    }
    counts
        .iter()
        .map(|&v| {
            let idx = if v <= 0 {
                0
            } else {
                (1 + ((v as f64 / hi as f64) * (SPARK.len() as f64 - 2.0)).round() as usize)
                    .min(SPARK.len() - 1)
            };
            let ch = SPARK[idx].to_string();
            // Density gradient: faint for quiet buckets, bright cyan for the busiest.
            if idx <= 1 {
                c2(&ch, "cyan", "dim")
            } else if idx <= 4 {
                c1(&ch, "cyan")
            } else {
                c1(&ch, "bcyan")
            }
        })
        .collect()
}

fn today_str() -> String {
    let days = now_unix().div_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}", y, mo, d)
}

fn date_axis(dates: &[String], width: usize) -> String {
    if width < 12 || dates.is_empty() {
        return String::new();
    }
    let today = today_str();

    let lbl = |dstr: &str, newest: bool| -> String {
        if newest && dstr == today {
            return "today".to_string();
        }
        let parts = dstr.split_once('-').and_then(|(_, md)| md.split_once('-'));
        if let Some((mo_s, d_s)) = parts {
            let mo: usize = mo_s.parse().unwrap_or(0);
            let d: u32 = d_s.parse().unwrap_or(0);
            const MON: [&str; 13] = [
                "???", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                "Dec",
            ];
            format!("{} {:02}", MON.get(mo).copied().unwrap_or("???"), d)
        } else {
            dstr.to_string()
        }
    };

    let mut slots: Vec<char> = vec![' '; width];

    let place = |slots: &mut Vec<char>, text: &str, start: usize| {
        let tchars: Vec<char> = text.chars().collect();
        let start = start.min(width.saturating_sub(tchars.len()));
        for (i, ch) in tchars.iter().enumerate() {
            if start + i < width {
                slots[start + i] = *ch;
            }
        }
    };

    let left = lbl(&dates[0], false);
    let right = lbl(dates.last().unwrap(), true);
    let mid = lbl(&dates[dates.len() / 2], false);

    place(&mut slots, &left, 0);
    place(&mut slots, &right, width.saturating_sub(right.len()));
    if width >= left.len() + right.len() + mid.len() + 6 {
        place(&mut slots, &mid, (width - mid.len()) / 2);
    }

    slots.iter().collect::<String>().trim_end().to_string()
}

// Fill calendar gaps so idle days show as zero buckets.
fn daily_series(daily: &[(String, i64)]) -> (Vec<i64>, Vec<String>) {
    let mut parsed: Vec<(i64, i64)> = Vec::new();
    for (dstr, n) in daily {
        if let Some(days) = parse_ymd(dstr).and_then(|(y, mo, d)| civil_to_days(y, mo, d)) {
            parsed.push((days, *n));
        }
    }
    if parsed.is_empty() {
        return (vec![], vec![]);
    }
    let by_day: HashMap<i64, i64> = parsed.iter().cloned().collect();
    let (mut start, end) = (parsed[0].0, parsed.last().unwrap().0);
    if end - start > 3650 {
        start = end - 3650; // guard: one ancient session can't explode the series
    }
    let mut counts = Vec::new();
    let mut dates = Vec::new();
    let mut cur = start;
    while cur <= end {
        counts.push(*by_day.get(&cur).unwrap_or(&0));
        let (y, mo, d) = civil_from_days(cur);
        dates.push(format!("{:04}-{:02}-{:02}", y, mo, d));
        cur += 1;
    }
    (counts, dates)
}

// ---- project label ----------------------------------------------------------

fn home_username() -> String {
    paths::home()
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn shorten_project(key: &str, cwd: Option<&str>) -> String {
    let home = home_username();
    if let Some(cwd) = cwd {
        let p = Path::new(cwd);
        let parts: Vec<&str> = p
            .components()
            .filter_map(|c| {
                use std::path::Component;
                match c {
                    Component::Normal(s) => s.to_str(),
                    _ => None,
                }
            })
            .collect();
        let meaningful: Vec<&str> = parts
            .iter()
            .copied()
            .filter(|s| {
                let lo = s.to_lowercase();
                lo != home && !CONTAINER_TOKENS.contains(&lo.as_str())
            })
            .collect();
        let tail: &[&str] = if meaningful.len() >= 2 {
            &meaningful[meaningful.len() - 2..]
        } else if !meaningful.is_empty() {
            &meaningful[meaningful.len() - 1..]
        } else if !parts.is_empty() {
            &parts[parts.len() - 1..]
        } else {
            &[]
        };
        if !tail.is_empty() {
            return tail.join("/");
        }
        return if key.is_empty() {
            "?".to_string()
        } else {
            key.to_string()
        };
    }
    // Fallback: decode the encoded key (dashes act as path separators).
    let toks: Vec<&str> = key
        .split('-')
        .filter(|t| {
            let lo = t.to_lowercase();
            !t.is_empty() && lo != home && !CONTAINER_TOKENS.contains(&lo.as_str())
        })
        .collect();
    if toks.is_empty() {
        return if key.is_empty() {
            "?".to_string()
        } else {
            key.to_string()
        };
    }
    if toks.len() >= 2 {
        toks[toks.len() - 2..].join("/")
    } else {
        toks[toks.len() - 1].to_string()
    }
}

fn tilde(path: &str) -> String {
    let home = paths::home().to_string_lossy().into_owned();
    if path == home {
        return "~".to_string();
    }
    if let Some(rest) = path.strip_prefix(&home) {
        if rest.starts_with('/') || rest.starts_with('\\') {
            return format!("~{}", rest);
        }
    }
    path.to_string()
}

// ---- git helpers ------------------------------------------------------------

// Runs a git subcommand against `cwd`. GIT_TERMINAL_PROMPT=0 prevents any
// interactive prompt; repo-local helper config (fsmonitor) is neutralized so
// reading an untrusted working dir can never spawn its programs.
fn git_run_in(cwd: &str, args: &[&str]) -> Option<String> {
    // Absolute path only. No git there means no repo label on the dashboard,
    // which is a cosmetic loss; a git found through PATH is an arbitrary
    // program run against the directory you happen to be sitting in.
    let git = paths::system_tool(&["/usr/bin/git", "/bin/git"])?;
    let out = Command::new(git)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "echo")
        .env("SSH_ASKPASS", "echo")
        .args(["-c", "core.fsmonitor="])
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            Some(s)
        } else {
            None
        }
    } else {
        None
    }
}

fn slug_from_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let path_part: &str = if let Some(pos) = url.find("://") {
        // scheme://[user@]host[:port]/owner/repo
        let after = &url[pos + 3..];
        after.find('/').map(|p| &after[p + 1..]).unwrap_or("")
    } else {
        // scp-like: git@host:owner/repo
        &url[url.find(':')? + 1..]
    };
    let segs: Vec<&str> = path_part.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() >= 2 {
        Some(format!("{}/{}", segs[segs.len() - 2], segs[segs.len() - 1]))
    } else {
        None
    }
}

fn git_slug(cwd: &str) -> Option<String> {
    let url = git_run_in(cwd, &["-C", cwd, "config", "--get", "remote.origin.url"])?;
    slug_from_url(&url)
}

fn git_root(cwd: &str) -> Option<String> {
    git_run_in(cwd, &["-C", cwd, "rev-parse", "--show-toplevel"])
}

// ---- data helpers -----------------------------------------------------------

fn last_backup_age_secs() -> Option<u64> {
    let bdir = paths::backups_dir();
    if !bdir.exists() {
        return None;
    }
    let newest = std::fs::read_dir(&bdir)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let n = name.to_string_lossy();
            n.starts_with("snapshot-") && n.ends_with(".db")
        })
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()?;
    newest.elapsed().ok().map(|d| d.as_secs())
}

/// Where the semantic index stands, in one plain line. Nothing here starts a
/// run or touches the network — the dashboard only reports. Counted over the
/// same set the backfill works from: turns that hold text.
fn semantic_line(conn: &Connection) -> String {
    match paths::semantic_mode() {
        Ok(mode) if mode == "off" => {
            return format!(
                "{} {}",
                c1("off", "gray"),
                c1("(semantic=off in config)", "gray")
            )
        }
        // An unreadable config counts as off everywhere else, so it reads off
        // here too — with the reason, since this is the diagnostic view.
        Err(e) => return format!("{} {}", c1("off", "gray"), c1(&format!("({e})"), "yellow")),
        Ok(_) => {}
    }
    // The recorded error, not a guess: "offline?" is wrong when the truth is a
    // full disk or a checksum that didn't match.
    if let Some(r) = embed::last_failure() {
        return c1(
            &format!(
                "waiting to retry — the last run didn't finish ({}) — retries on its own",
                r.last_error
            ),
            "yellow",
        );
    }
    let total: i64 = conn
        .query_row(
            "SELECT count(*) FROM turns WHERE text IS NOT NULL AND text <> ''",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // The table is created on first use, so a missing one just reads as zero.
    let done: i64 = conn
        .query_row(
            "SELECT count(*) FROM turn_embeddings e JOIN turns t ON t.id = e.turn_id \
             WHERE e.model = ?1 AND t.text IS NOT NULL AND t.text <> ''",
            [embed::MODEL_KEY],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if done >= total {
        return format!(
            "{} {}",
            c1("ready", "green"),
            c1(&format!("— all {} turns indexed", commas(total)), "gray")
        );
    }
    format!(
        "{} {}",
        c1("building the index", "yellow"),
        c1(
            &format!(
                "— {} of {} turns done (finishes on its own; searches already work, newest first)",
                commas(done),
                commas(total)
            ),
            "gray"
        )
    )
}

/// `None` when the queue can't be read — the dashboard says so rather than
/// drawing a reassuring zero over a file that is actually broken.
fn pending_count() -> Option<usize> {
    let text = paths::read_control_file(&paths::pending_log(), paths::CONTROL_FILE_MAX)
        .ok()?
        .unwrap_or_default();
    Some(
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(ingest::queue_sid)
            .collect::<HashSet<_>>()
            .len(),
    )
}

// ---- current-context resolution ---------------------------------------------

struct CurrentContext {
    cwd: String,
    key: String,
    memory_md: PathBuf,
    candidates: Vec<String>,
}

fn current_context(conn: &Connection) -> CurrentContext {
    let pwd = std::env::var("PWD").ok();
    let cwd_real = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());

    let mut raws: Vec<String> = Vec::new();
    for p in [pwd.as_deref(), cwd_real.as_deref()].into_iter().flatten() {
        if !raws.contains(&p.to_string()) {
            raws.push(p.to_string());
        }
    }

    // Collect raw + canonical (symlink-resolved) variants.
    let mut cwds: Vec<String> = Vec::new();
    for p in &raws {
        let canonical = std::fs::canonicalize(p)
            .ok()
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.clone());
        for q in [p.clone(), canonical] {
            if !cwds.contains(&q) {
                cwds.push(q);
            }
        }
    }

    // A subdir's sessions are usually keyed by the git repo root — add it too.
    if let Some(raw) = raws.first() {
        if let Some(root) = git_root(raw) {
            let canonical = std::fs::canonicalize(&root)
                .ok()
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.clone());
            for q in [root, canonical] {
                if !cwds.contains(&q) {
                    cwds.push(q);
                }
            }
        }
    }

    // A sessions-table hit is authoritative — it holds Claude Code's own encoding.
    let mut candidates: Vec<String> = Vec::new();
    for cw in &cwds {
        if let Ok(Some(proj)) = conn.query_row(
            "SELECT project FROM sessions WHERE cwd=? ORDER BY last_ts DESC LIMIT 1",
            [cw.as_str()],
            |r| r.get::<_, Option<String>>(0),
        ) {
            if !proj.is_empty() && !candidates.contains(&proj) {
                candidates.push(proj);
            }
        }
    }
    for cw in &cwds {
        let enc = db::encode_cwd(cw);
        if !candidates.contains(&enc) {
            candidates.push(enc);
        }
    }

    let display = raws.first().cloned().unwrap_or_default();
    let proj_dir = paths::projects_dir();

    for enc in &candidates {
        let md = proj_dir.join(enc).join("memory").join("MEMORY.md");
        if md.exists() {
            return CurrentContext {
                cwd: display,
                key: enc.clone(),
                memory_md: md,
                candidates,
            };
        }
    }
    let chosen = candidates
        .first()
        .cloned()
        .unwrap_or_else(|| db::encode_cwd(&display));
    let md = proj_dir.join(&chosen).join("memory").join("MEMORY.md");
    CurrentContext {
        cwd: display,
        key: chosen,
        memory_md: md,
        candidates,
    }
}

// ---- gathered stats ---------------------------------------------------------

struct ProjectRow {
    project: String,
    cwd: Option<String>,
    turns: i64,
    sessions: i64,
}

struct FactStats {
    active: i64,
    archived: i64,
    pinned: i64,
    bytype: Vec<(String, i64)>,
}

struct Stats {
    sessions: i64,
    turns: i64,
    min_ts: Option<String>,
    max_ts: Option<String>,
    roles: HashMap<String, i64>,
    nproj: i64,
    projects: Vec<ProjectRow>,
    daily: Vec<(String, i64)>,
    facts: FactStats,
}

fn gather(conn: &Connection) -> rusqlite::Result<Stats> {
    let sessions: i64 = conn.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))?;

    // is_meta=0: real user/assistant turns only, never slash-command wrappers.
    let (turns, min_ts, max_ts): (i64, Option<String>, Option<String>) = conn.query_row(
        "SELECT count(*), min(ts), max(ts) FROM turns WHERE is_meta=0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    let mut roles: HashMap<String, i64> = HashMap::new();
    {
        let mut stmt =
            conn.prepare("SELECT role, count(*) n FROM turns WHERE is_meta=0 GROUP BY role")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            roles.insert(r.get(0)?, r.get(1)?);
        }
    }

    let nproj: i64 = conn.query_row(
        "SELECT count(DISTINCT project) FROM turns WHERE is_meta=0",
        [],
        |r| r.get(0),
    )?;

    // Label each project by its most-used cwd. Tie-break to shortest path so the repo root wins.
    let projects = {
        let mut stmt = conn.prepare(
            "SELECT t.project, \
             (SELECT cwd FROM turns x \
              WHERE x.project=t.project AND x.is_meta=0 AND x.cwd IS NOT NULL \
              GROUP BY cwd ORDER BY count(*) DESC, length(cwd) ASC LIMIT 1) cwd, \
             count(*) turns, count(DISTINCT session_id) sessions \
             FROM turns t WHERE t.is_meta=0 GROUP BY t.project ORDER BY turns DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out: Vec<ProjectRow> = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(ProjectRow {
                project: r.get(0)?,
                cwd: r.get(1)?,
                turns: r.get(2)?,
                sessions: r.get(3)?,
            });
        }
        out
    };

    // substr(ts,1,10) avoids tripping SQLite's date parser on Z/offset suffixes.
    let daily = {
        let mut stmt = conn.prepare(
            "SELECT substr(ts,1,10) d, count(*) n FROM turns \
             WHERE is_meta=0 AND ts IS NOT NULL AND length(ts)>=10 \
             GROUP BY substr(ts,1,10) ORDER BY d",
        )?;
        let mut rows = stmt.query([])?;
        let mut out: Vec<(String, i64)> = Vec::new();
        while let Some(r) = rows.next()? {
            out.push((r.get(0)?, r.get(1)?));
        }
        out
    };

    let facts = gather_facts(conn);

    Ok(Stats {
        sessions,
        turns,
        min_ts,
        max_ts,
        roles,
        nproj,
        projects,
        daily,
        facts,
    })
}

fn gather_facts(conn: &Connection) -> FactStats {
    let mut fs = FactStats {
        active: 0,
        archived: 0,
        pinned: 0,
        bytype: Vec::new(),
    };

    // Silently skip if the facts table doesn't exist (DB predates Phase 2).
    if let Ok(mut stmt) = conn.prepare(
        "SELECT CASE WHEN status='archived' THEN 'archived' ELSE 'active' END st, \
         count(*) n, sum(CASE WHEN pinned=1 THEN 1 ELSE 0 END) p FROM facts GROUP BY st",
    ) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(r)) = rows.next() {
                let st: String = r.get(0).unwrap_or_default();
                let n: i64 = r.get(1).unwrap_or(0);
                let p: i64 = r.get(2).unwrap_or(0);
                if st == "archived" {
                    fs.archived += n;
                } else {
                    fs.active += n;
                    fs.pinned += p;
                }
            }
        }
    }
    if let Ok(mut stmt) = conn.prepare(
        "SELECT type, count(*) n FROM facts WHERE status!='archived' GROUP BY type ORDER BY n DESC",
    ) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(r)) = rows.next() {
                let t: String = r
                    .get::<_, Option<String>>(0)
                    .unwrap_or(None)
                    .unwrap_or_else(|| "?".to_string());
                let n: i64 = r.get(1).unwrap_or(0);
                fs.bytype.push((t, n));
            }
        }
    }
    fs
}

// ---- terminal width ---------------------------------------------------------

fn term_width() -> usize {
    if let Ok(v) = std::env::var("COLUMNS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    #[cfg(unix)]
    if let Some(w) = ioctl_cols() {
        return w;
    }
    80
}

#[cfg(unix)]
fn ioctl_cols() -> Option<usize> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        _ws_xpixel: u16,
        _ws_ypixel: u16,
    }
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: IoctlReqT = 0x4008_7468;
    #[cfg(not(target_os = "macos"))]
    const TIOCGWINSZ: IoctlReqT = 0x5413;
    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        _ws_xpixel: 0,
        _ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes a fixed Winsize struct into the pointer we provide.
    let ret = unsafe {
        call_ioctl(
            1,
            TIOCGWINSZ,
            &mut ws as *mut Winsize as *mut std::ffi::c_void,
        )
    };
    if ret == 0 && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        None
    }
}

#[cfg(unix)]
type IoctlReqT = u64;

#[cfg(unix)]
extern "C" {
    fn ioctl(fd: i32, request: IoctlReqT, ...) -> i32;
}

#[cfg(unix)]
unsafe fn call_ioctl(fd: i32, req: IoctlReqT, arg: *mut std::ffi::c_void) -> i32 {
    ioctl(fd, req, arg)
}

// ---- stdout TTY check -------------------------------------------------------

fn stdout_is_tty() -> bool {
    #[cfg(unix)]
    {
        extern "C" {
            fn isatty(fd: i32) -> i32;
        }
        // SAFETY: isatty(1) is a pure query with no side effects.
        unsafe { isatty(1) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

// ---- render -----------------------------------------------------------------

fn sline(label: &str, value: &str, lw: usize) -> String {
    format!("  {} {}", c1(&format!("{:lw$}", label), "gray"), value)
}

fn render_table(
    projects: &[ProjectRow],
    total_turns: i64,
    term_w: usize,
    current_keys: &HashSet<String>,
    current_slug: Option<&str>,
) {
    let top = &projects[..projects.len().min(10)];
    let labels: Vec<String> = top
        .iter()
        .map(|r| {
            if current_keys.contains(&r.project) {
                current_slug
                    .map(str::to_string)
                    .unwrap_or_else(|| shorten_project(&r.project, r.cwd.as_deref()))
            } else {
                shorten_project(&r.project, r.cwd.as_deref())
            }
        })
        .collect();

    let (sess_w, turn_w, share_w) = (6usize, 7usize, 7usize);
    // Keep the whole row inside the terminal: gutter(2) + pw + 3 column gaps/widths.
    let fits = term_w.saturating_sub(2 + 2 + sess_w + 2 + turn_w + 2 + share_w);
    let max_label = labels.iter().map(|l| l.len()).max().unwrap_or(0);
    let pw = max_label
        .max("by project".len())
        .clamp(12, 34)
        .min(fits.max(12));

    println!(
        "  {}  {}  {}  {}",
        c1(&format!("{:pw$}", "by project"), "bold"),
        c1(&format!("{:>sess_w$}", "sess"), "gray"),
        c1(&format!("{:>turn_w$}", "turns"), "gray"),
        c1(&format!("{:>share_w$}", "share"), "gray"),
    );

    for (r, label) in top.iter().zip(labels.iter()) {
        let share = if total_turns > 0 {
            r.turns as f64 / total_turns as f64
        } else {
            0.0
        };
        let label = if label.chars().count() > pw {
            let mut s: String = label.chars().take(pw - 1).collect();
            s.push('…');
            s
        } else {
            label.clone()
        };
        let is_cur = current_keys.contains(&r.project);
        let (pcol_a, pcol_b) = if share >= 0.25 {
            ("bold", Some("white"))
        } else if share >= 0.10 {
            ("white", None)
        } else {
            ("dim", None)
        };
        let gutter = if is_cur {
            c2("▸", "bold", "cyan") + " "
        } else {
            "  ".to_string()
        };
        let name = if is_cur {
            c2(&format!("{:pw$}", label), "bold", "cyan")
        } else {
            format!("{:pw$}", label)
        };
        let share_str = format!("{:.1}%", share * 100.0);
        let share_col = match pcol_b {
            Some(b) => c2(&format!("{:>share_w$}", share_str), pcol_a, b),
            None => c1(&format!("{:>share_w$}", share_str), pcol_a),
        };
        let sess_col = human(r.sessions);
        let turn_col = human(r.turns);
        println!(
            "{}{}  {:>sess_w$}  {:>turn_w$}  {}",
            gutter, name, sess_col, turn_col, share_col,
        );
    }

    if projects.len() > 10 {
        let more = projects.len() - 10;
        let extra: i64 = projects[10..].iter().map(|r| r.turns).sum();
        let pct = if total_turns > 0 {
            extra as f64 / total_turns as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "  {}",
            c1(
                &format!(
                    "+ {} more project{} · {:.1}%",
                    more,
                    if more != 1 { "s" } else { "" },
                    pct
                ),
                "dim"
            )
        );
    }
}

fn render(conn: &Connection, stats: &Stats, ctx: &CurrentContext, detail: bool) {
    let term_w = term_width();
    let avail = (term_w as isize - 2).max(10) as usize;

    // ---- header: title + last write (right-aligned) ----
    println!();
    let title = c2("subrosa", "bold", "cyan") + &c1(" · memory archive", "gray");
    let last = match &stats.max_ts {
        Some(ts) => format!("last write {}", ago(ts)),
        None => String::new(),
    };
    let pad = avail as isize - vlen(&title) as isize - last.len() as isize;
    let header_has_last = !last.is_empty() && pad >= 2;
    if header_has_last {
        println!(
            "  {}{}{}",
            title,
            " ".repeat(pad as usize),
            c1(&last, "gray")
        );
    } else {
        println!("  {}", title);
    }

    // ---- working in (current repo) ----
    let cwd_disp = &ctx.cwd;
    let mut cur_mark: HashSet<String> = HashSet::new();
    let slug = if !cwd_disp.is_empty() {
        git_slug(cwd_disp)
    } else {
        None
    };

    if !cwd_disp.is_empty() {
        let by_key: HashMap<&str, &ProjectRow> = stats
            .projects
            .iter()
            .map(|r| (r.project.as_str(), r))
            .collect();
        let match_row = ctx
            .candidates
            .iter()
            .find_map(|k| by_key.get(k.as_str()).copied());
        if let Some(row) = match_row {
            cur_mark.insert(row.project.clone());
            let label = slug.as_deref().map(str::to_string).unwrap_or_else(|| {
                shorten_project(&row.project, row.cwd.as_deref().or(Some(cwd_disp)))
            });
            let meta = c1(
                &format!(
                    " · {} turns · {} sessions",
                    human(row.turns),
                    human(row.sessions)
                ),
                "gray",
            );
            println!(
                "{}",
                sline("working in", &(c2(&label, "bold", "cyan") + &meta), 10)
            );
        } else {
            let label = slug
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| shorten_project(&ctx.key, Some(cwd_disp)));
            let meta = c1(" · not archived yet", "gray");
            println!(
                "{}",
                sline("working in", &(c2(&label, "bold", "cyan") + &meta), 10)
            );
        }
    }

    if stats.turns == 0 {
        println!();
        println!(
            "  {}",
            c1(
                "No sessions archived yet — run `subrosa ingest --sweep`.",
                "gray"
            )
        );
        println!();
        return;
    }

    // ---- activity sparkline (the hero visual) ----
    let (counts, dates) = daily_series(&stats.daily);
    if !counts.is_empty() {
        let spark_w = counts.len().min(avail);
        let buckets = bucketize(&counts, spark_w);
        let u = *stats.roles.get("user").unwrap_or(&0);
        let a = *stats.roles.get("assistant").unwrap_or(&0);
        let ndays = counts.len();
        println!();
        println!(
            "  {}{}",
            c1("session history", "bold"),
            c1(
                &format!(
                    " · {} sessions · {} projects",
                    human(stats.sessions),
                    stats.nproj
                ),
                "gray"
            )
        );
        let axis = date_axis(&dates, spark_w);
        if !axis.is_empty() {
            println!("  {}", c1(&axis, "gray"));
        }
        println!("  {}", sparkline(&buckets));
        println!(
            "  {}",
            c1(
                &format!(
                    "{} turns · {} user · {} assistant · {} day{}",
                    human(stats.turns),
                    human(u),
                    human(a),
                    ndays,
                    if ndays != 1 { "s" } else { "" }
                ),
                "gray"
            )
        );
    }

    // ---- store stats ----
    println!();
    if let Some(ref ts) = stats.max_ts {
        if !header_has_last {
            println!(
                "{}",
                sline(
                    "updated",
                    &format!("{} {}", fmt_ts(ts), c1(&format!("({})", ago(ts)), "gray")),
                    8
                )
            );
        }
    }

    let f = &stats.facts;
    println!(
        "{}",
        sline(
            "facts",
            &format!(
                "{}  {}",
                c1(&format!("{} active", human(f.active)), "white"),
                c1(
                    &format!("{} pinned · {} archived", f.pinned, f.archived),
                    "gray"
                )
            ),
            8
        )
    );

    let md = &ctx.memory_md;
    if md.exists() {
        let msize = std::fs::metadata(md).map(|m| m.len()).unwrap_or(0);
        // The dashboard only reads, so a bad .budget draws the meter against
        // the default rather than killing the whole view.
        let budget = match generate::resolve_budget(md.parent().unwrap_or(Path::new("."))) {
            Ok((b, warn)) => {
                if let Some(w) = warn {
                    eprintln!("{w}");
                }
                b
            }
            Err(e) => {
                eprintln!("{e}");
                generate::DEFAULT_BUDGET
            }
        }
        // Bytes past the load cap never reach context, so the meter shows the
        // lower of the two — a budget set above it isn't real headroom.
        .min(generate::CC_LOAD_CAP) as u64;
        let frac = msize as f64 / budget as f64;
        let col = if frac < 0.70 {
            "green"
        } else if frac < 0.90 {
            "yellow"
        } else {
            "bred"
        };
        let lines = std::fs::read_to_string(md)
            .map(|t| t.lines().count())
            .unwrap_or(0);
        let over = if lines > generate::CC_LOAD_LINES {
            c1(
                &format!("  {lines} lines >{}", generate::CC_LOAD_LINES),
                "bred",
            )
        } else {
            String::new()
        };
        let mw = ((term_w as isize - 40).max(10) as usize).min(30);
        println!(
            "{}",
            sline(
                "index",
                &format!(
                    "{} {} {}  {}{}",
                    human_bytes(msize, false),
                    meter(frac, mw),
                    human_bytes(budget, false),
                    c1(&format!("{:.0}%", frac * 100.0), col),
                    over
                ),
                8
            )
        );
    } else {
        println!(
            "{}",
            sline(
                "index",
                &format!(
                    "{}{}",
                    c1("—", "dim"),
                    c1("  no MEMORY.md for this project", "gray")
                ),
                8
            )
        );
    }

    let db_path = paths::db_path();
    let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let btxt = match last_backup_age_secs() {
        Some(age) => format!("backup {}", ago_secs(age as i64)),
        None => c1("backup never", "yellow"),
    };
    println!(
        "{}",
        sline(
            "db",
            &format!(
                "{} {} {}",
                human_bytes(db_size, true),
                c1("·", "gray"),
                btxt
            ),
            8
        )
    );

    println!("{}", sline("semantic", &semantic_line(conn), 8));

    match pending_count() {
        Some(pend) if pend > 0 => println!(
            "{}",
            sline(
                "ckpt",
                &format!(
                    "{}{}",
                    c1(&format!("{} pending", pend), "yellow"),
                    c1("  run /checkpoint", "gray")
                ),
                8
            )
        ),
        Some(_) => {}
        // A queue we can't read is louder than one that's empty: the backlog
        // is invisible exactly when something is wrong with the file holding it.
        None => println!(
            "{}",
            sline(
                "ckpt",
                &c1(
                    "unreadable — check ~/.claude/subrosa/pending-checkpoint.log",
                    "bred"
                ),
                8
            )
        ),
    }

    // ---- detail section ----
    if detail {
        println!();
        println!("  {}", c1("details", "bold"));
        if let (Some(ref mn), Some(ref mx)) = (&stats.min_ts, &stats.max_ts) {
            let days = match (parse_ts(mn), parse_ts(mx)) {
                (Some(t0), Some(t1)) => ((t1 - t0) / 86_400).max(0),
                _ => 0,
            };
            println!(
                "{}",
                sline(
                    "span",
                    &format!(
                        "{} → {} {}",
                        fmt_ts(mn),
                        fmt_ts(mx),
                        c1(&format!("({} days)", days), "gray")
                    ),
                    11
                )
            );
        }
        let mut role_vec: Vec<(&String, &i64)> = stats.roles.iter().collect();
        role_vec.sort_by_key(|(_, &v)| std::cmp::Reverse(v));
        let roles_str = if role_vec.is_empty() {
            "—".to_string()
        } else {
            role_vec
                .iter()
                .map(|(k, v)| format!("{} {}", k, human(**v)))
                .collect::<Vec<_>>()
                .join(" · ")
        };
        println!("{}", sline("by role", &c1(&roles_str, "gray"), 11));

        if !f.bytype.is_empty() {
            let bt = f
                .bytype
                .iter()
                .map(|(k, v)| format!("{} {}", k, v))
                .collect::<Vec<_>>()
                .join(" · ");
            println!("{}", sline("by type", &c1(&bt, "gray"), 11));
        }
        if !cwd_disp.is_empty() {
            println!(
                "{}",
                sline("working dir", &c1(&tilde(cwd_disp), "gray"), 11)
            );
        }
        println!(
            "{}",
            sline("db path", &c1(&db_path.to_string_lossy(), "gray"), 11)
        );
        let md_str = if md.as_os_str().is_empty() {
            "—".to_string()
        } else {
            md.to_string_lossy().into_owned()
        };
        println!("{}", sline("MEMORY.md", &c1(&md_str, "gray"), 11));
    }

    // ---- by project ----
    println!();
    render_table(
        &stats.projects,
        stats.turns,
        term_w,
        &cur_mark,
        slug.as_deref(),
    );
    println!();
}

// ---- DB connection (read-only) ----------------------------------------------

fn connect_readonly() -> Result<Connection, rusqlite::Error> {
    let p = paths::db_path();
    Connection::open_with_flags(
        &p,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
}

// ---- entry point ------------------------------------------------------------

pub fn run(args: &Args) -> ExitCode {
    if stdout_is_tty() && !args.no_color && std::env::var("NO_COLOR").is_err() {
        color_on();
    }

    let conn = match connect_readonly() {
        Ok(c) => c,
        Err(_) => {
            // Friendly message when the DB doesn't exist yet.
            println!(
                "  {} Run `subrosa init`, then `subrosa ingest --sweep` to archive past sessions.",
                c1("No memory store yet.", "yellow")
            );
            return ExitCode::FAILURE;
        }
    };

    let stats = match gather(&conn) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[subrosa] stats error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let ctx = current_context(&conn);
    render(&conn, &stats, &ctx, args.detail);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::commas;

    /// The index line's whole job is being readable at a glance, and the
    /// grouping is where an off-by-one hides.
    #[test]
    fn counts_group_every_three_digits() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(6_027), "6,027");
        assert_eq!(commas(127_688), "127,688");
        assert_eq!(commas(1_234_567), "1,234,567");
        assert_eq!(commas(-1_234), "-1,234");
    }
}
