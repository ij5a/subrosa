mod backup;
mod db;
mod facts;
mod generate;
mod hook;
mod import_existing;
mod ingest;
mod paths;
mod recall;
mod redact;
mod related;
mod search;
mod session;
mod sessions;
mod setup;
mod stats;
mod tags;
mod text;
mod timeutil;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Persistent, private memory for Claude Code.
///
/// Archives every session transcript into a local SQLite database (FTS5)
/// and makes it searchable. Everything stays on your machine — sub rosa.
#[derive(Parser)]
#[command(name = "subrosa", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-time setup: create the schema and pick where backups mirror to
    Setup {
        /// Mirror snapshots into this folder (skips the interactive question)
        #[arg(long)]
        mirror: Option<PathBuf>,
        /// Keep snapshots local-only, no mirror
        #[arg(long)]
        no_mirror: bool,
    },
    /// Create (or verify) the local memory schema and print a status line
    Init {
        /// Append the "Memory recall" block to ~/.claude/CLAUDE.md (idempotent),
        /// so Claude searches the archive on its own at task start
        #[arg(long)]
        claude_md: bool,
    },
    /// Snapshot the DB (consistent copy; safe while in use)
    Backup {
        /// Snapshot even if one was taken in the last 24h
        #[arg(long)]
        force: bool,
        /// How many local snapshots to keep
        #[arg(long, default_value_t = backup::DEFAULT_KEEP)]
        keep: usize,
        /// Skip the configured mirror copy
        #[arg(long)]
        no_mirror: bool,
    },
    /// Archive transcript JSONL files into the local memory DB
    Ingest {
        /// Transcript .jsonl paths
        paths: Vec<PathBuf>,
        /// Ingest every transcript that changed since its last archive
        #[arg(long)]
        sweep: bool,
        /// Suppress per-file output
        #[arg(long)]
        quiet: bool,
    },
    /// Ingest every transcript that changed since its last archive
    Sweep {
        /// Suppress output
        #[arg(long)]
        quiet: bool,
    },
    /// Search the archived transcripts (FTS5, bm25-ranked)
    Search {
        /// Search terms (each phrase-quoted unless --raw)
        terms: Vec<String>,
        /// Max results
        #[arg(short = 'n', long, default_value_t = 15)]
        limit: i64,
        /// Treat the query as raw FTS5 syntax
        #[arg(long)]
        raw: bool,
        /// Restrict to a project (substring match)
        #[arg(long)]
        project: Option<String>,
        /// Restrict to a session id (prefix match)
        #[arg(long)]
        session: Option<String>,
        /// Substring/typo matching via a trigram index (built on first use)
        #[arg(long)]
        fuzzy: bool,
        /// Only turns on or after this UTC date (YYYY-MM-DD, inclusive)
        #[arg(long)]
        after: Option<String>,
        /// Only turns on or before this UTC date (YYYY-MM-DD, inclusive)
        #[arg(long)]
        before: Option<String>,
        /// Only sessions carrying this tag (e.g. tool:bash); repeatable, ANDed
        #[arg(long)]
        tag: Vec<String>,
        /// Drop hits that contain this term (repeatable; ignored with --raw)
        #[arg(long)]
        exclude: Vec<String>,
        /// Also print N turns on each side of every hit (same session), for context
        #[arg(short = 'C', long, default_value_t = 0)]
        context: i64,
    },
    /// Find terms and sessions that co-occur with an identifier across the archive
    Related {
        /// The anchor identifier (phrase-quoted; hyphens and dots are safe)
        identifier: String,
        /// Max related terms to show
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
        /// Restrict to a project (substring match)
        #[arg(long)]
        project: Option<String>,
        /// Max sessions to list
        #[arg(long, default_value_t = 10)]
        sessions: i64,
    },
    /// List archived sessions (newest first), filterable by project/date/tag
    Sessions {
        /// Restrict to a project (substring match)
        #[arg(long)]
        project: Option<String>,
        /// Only sessions ending on or after this UTC date (YYYY-MM-DD, inclusive)
        #[arg(long)]
        after: Option<String>,
        /// Only sessions starting on or before this UTC date (YYYY-MM-DD, inclusive)
        #[arg(long)]
        before: Option<String>,
        /// Only sessions carrying this tag (e.g. tool:bash); repeatable, ANDed
        #[arg(long)]
        tag: Vec<String>,
        /// Max sessions to list
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
    },
    /// Show the memory archive dashboard (activity, store, by-project share)
    Stats(stats::Args),
    /// Inspect or mutate curated facts (search/link/list/upsert/archive/pin/unpin)
    Fact {
        #[arg(value_enum)]
        action: facts::FactAction,
        /// For `link`: a fact slug or leaf filename; for `search`: the query terms
        anchor: Option<String>,
        /// Leaf filename, e.g. reference_foo.md (bare filename, no path)
        #[arg(long)]
        leaf: Option<String>,
        /// Fact type: user | feedback | project | reference
        #[arg(long = "type")]
        type_: Option<String>,
        /// Index title (default: stored value, then name slug)
        #[arg(long)]
        title: Option<String>,
        /// One-line index hook (default: stored value, then description)
        #[arg(long)]
        hook: Option<String>,
        /// Force always-loaded regardless of budget
        #[arg(long)]
        pin: bool,
        /// Stamp origin_session on new facts (checkpoint provenance)
        #[arg(long)]
        origin_session: Option<String>,
        /// Encoded project name (default: parent dir of --memdir)
        #[arg(long)]
        project: Option<String>,
        /// A project's memory/ dir (default: the current project, from cwd)
        #[arg(long)]
        memdir: Option<PathBuf>,
        /// Filter for `list`
        #[arg(long, value_enum, default_value = "active")]
        status: facts::StatusFilter,
    },
    /// Generate a project's MEMORY.md from the facts table (byte-budgeted)
    Generate {
        /// Encoded project name (default: parent dir of --memdir)
        #[arg(long)]
        project: Option<String>,
        /// A project's memory/ dir (default: the current project, from cwd)
        #[arg(long)]
        memdir: Option<PathBuf>,
        /// Max bytes
        #[arg(long, default_value_t = generate::DEFAULT_BUDGET)]
        budget: i64,
        /// Output path (default: <memdir>/MEMORY.md)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Let facts not currently in the index compete for the budget too
        #[arg(long)]
        include_orphans: bool,
        /// Print to stdout, don't write the file
        #[arg(long)]
        dry_run: bool,
    },
    /// One-time import of a project's MEMORY.md + leaf files into the facts table
    Import {
        /// The project's memory/ dir (default: the current project, from cwd)
        memdir: Option<PathBuf>,
        /// Skip the safety backup of the memdir
        #[arg(long)]
        no_backup: bool,
        /// Override the encoded project name (default: parent dir name)
        #[arg(long)]
        project: Option<String>,
    },
    /// Print one archived session's flattened turns
    Session {
        /// Session id (transcript filename stem)
        id: String,
        /// Also print the session's auto-derived tags (one extra `# tags:` line)
        #[arg(long)]
        tags: bool,
    },
    /// List sessions queued for checkpoint
    Pending,
    /// Remove a session from the queue + record the checkpoint high-water mark
    CheckpointDrop {
        /// Session id
        id: String,
    },
    /// Conditionally queue a session (prunes empty/sub-agent-only sessions)
    CheckpointEnqueue {
        /// Session id
        id: String,
    },
    /// Mark the currently-running session checkpointed
    CheckpointMark,
    /// Empty the whole checkpoint queue (prefer checkpoint-drop per session)
    CheckpointClear,
    /// Claude Code hook entrypoints (read the hook JSON on stdin; never fail the session)
    #[command(subcommand)]
    Hook(HookEvent),
}

#[derive(Subcommand, Clone, Copy)]
pub enum HookEvent {
    /// SessionStart: catch-up ingest + checkpoint/byte-cap nudge
    SessionStart,
    /// SessionEnd: archive the ended session and queue it for checkpoint
    SessionEnd,
    /// UserPromptSubmit: inject relevant past-session hits into context
    UserPromptSubmit,
    /// PreCompact: archive the conversation so far + reset recall dedup
    PreCompact,
    /// Stop: incrementally ingest the in-progress transcript (near-real-time)
    Stop,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Hooks keep Rust's SIGPIPE ignore (they must always exit 0); CLI commands
    // get the Unix default back so `subrosa search | head` ends silently
    // instead of panicking on the closed pipe.
    if !matches!(cli.cmd, Some(Cmd::Hook(_))) {
        restore_sigpipe();
    }
    let Some(cmd) = cli.cmd else {
        // Bare `subrosa` opens the dashboard, same as `subrosa stats`.
        return stats::run(&stats::Args {
            detail: false,
            no_color: false,
        });
    };
    match cmd {
        Cmd::Hook(event) => hook::run(event), // logs problems, always exits 0
        Cmd::Setup { mirror, no_mirror } => setup::run(mirror, no_mirror),
        Cmd::Backup {
            force,
            keep,
            no_mirror,
        } => run_backup(force, keep, no_mirror),
        Cmd::Init { claude_md } => run_init(claude_md),
        Cmd::Ingest {
            paths,
            sweep,
            quiet,
        } => run_ingest(paths, sweep, quiet),
        Cmd::Sweep { quiet } => run_ingest(Vec::new(), true, quiet),
        Cmd::Search {
            terms,
            limit,
            raw,
            project,
            session,
            fuzzy,
            after,
            before,
            tag,
            exclude,
            context,
        } => search::run(
            &terms,
            limit,
            raw,
            project.as_deref(),
            session.as_deref(),
            fuzzy,
            after.as_deref(),
            before.as_deref(),
            &tag,
            &exclude,
            context,
        ),
        Cmd::Related {
            identifier,
            limit,
            project,
            sessions,
        } => related::run(&identifier, limit, project.as_deref(), sessions),
        Cmd::Sessions {
            project,
            after,
            before,
            tag,
            limit,
        } => sessions::run(
            project.as_deref(),
            after.as_deref(),
            before.as_deref(),
            &tag,
            limit,
        ),
        Cmd::Stats(args) => stats::run(&args),
        Cmd::Fact {
            action,
            anchor,
            leaf,
            type_,
            title,
            hook,
            pin,
            origin_session,
            project,
            memdir,
            status,
        } => facts::run(
            action,
            leaf,
            type_,
            title,
            hook,
            pin,
            origin_session,
            project,
            memdir,
            status,
            anchor,
        ),
        Cmd::Generate {
            project,
            memdir,
            budget,
            out,
            include_orphans,
            dry_run,
        } => generate::run(project, memdir, budget, out, include_orphans, dry_run),
        Cmd::Import {
            memdir,
            no_backup,
            project,
        } => import_existing::run(memdir, no_backup, project),
        Cmd::Session { id, tags } => session::dump(&id, tags),
        Cmd::Pending => run_pending(),
        Cmd::CheckpointDrop { id } => session::drop_sid(&id),
        Cmd::CheckpointEnqueue { id } => session::enqueue(&id),
        Cmd::CheckpointMark => session::mark_current(),
        Cmd::CheckpointClear => run_checkpoint_clear(),
    }
}

fn run_backup(force: bool, keep: usize, no_mirror: bool) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    match backup::snapshot(&conn, force, keep, !no_mirror) {
        Ok(Some(label)) => {
            println!("[subrosa] backup: {label}");
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("[subrosa] throttled — last snapshot is <24h old (use --force)");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[subrosa] backup failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_init(claude_md: bool) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type IN ('table','view') \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()
        })
        .unwrap_or_default();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    println!("[subrosa] db: {}", paths::db_path().display());
    println!("[subrosa] objects: {}", tables.join(", "));
    println!(
        "[subrosa] sessions={} turns={}",
        count("SELECT count(*) FROM sessions"),
        count("SELECT count(*) FROM turns")
    );
    if claude_md {
        return setup::append_claude_md();
    }
    ExitCode::SUCCESS
}

fn run_ingest(paths: Vec<PathBuf>, sweep: bool, quiet: bool) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    if sweep {
        match ingest::sweep(&conn, &paths::projects_dir()) {
            Ok((files, ingested, inserted)) => {
                if !quiet {
                    println!(
                        "[subrosa] sweep: {files} transcripts, {ingested} changed, +{inserted} turns"
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("[subrosa] sweep failed: {e}");
                ExitCode::FAILURE
            }
        }
    } else if paths.is_empty() {
        eprintln!("[subrosa] give one or more transcript paths, or --sweep");
        ExitCode::from(2)
    } else {
        let mut total = 0;
        for p in &paths {
            match ingest::ingest_file(&conn, p) {
                Ok((inserted, scanned)) => {
                    total += inserted;
                    if !quiet {
                        println!(
                            "[subrosa] {}: +{inserted} turns ({scanned} records scanned)",
                            p.display()
                        );
                    }
                }
                Err(e) => eprintln!("[subrosa] {}: {e}", p.display()),
            }
        }
        if !quiet && paths.len() > 1 {
            println!("[subrosa] total +{total} turns");
        }
        ExitCode::SUCCESS
    }
}

/// Print the queue, deduped by session id (a session can fire SessionEnd more than once).
fn run_pending() -> ExitCode {
    let Ok(text) = std::fs::read_to_string(paths::pending_log()) else {
        return ExitCode::SUCCESS; // no queue file = empty queue
    };
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let sid = line.rsplit('\t').next().unwrap_or(line);
        if seen.insert(sid.to_string()) {
            println!("{line}");
        }
    }
    ExitCode::SUCCESS
}

#[cfg(unix)]
fn restore_sigpipe() {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    // SIGPIPE=13, SIG_DFL=0 on the unix targets we ship (macOS, Linux).
    unsafe {
        signal(13, 0);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn run_checkpoint_clear() -> ExitCode {
    let pending = paths::pending_log();
    let Ok(text) = std::fs::read_to_string(&pending) else {
        println!("[subrosa] queue empty");
        return ExitCode::SUCCESS;
    };
    let n = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.rsplit('\t').next().unwrap_or(l))
        .collect::<std::collections::HashSet<_>>()
        .len();
    if let Err(e) = std::fs::write(&pending, "") {
        eprintln!("[subrosa] cannot clear queue: {e}");
        return ExitCode::FAILURE;
    }
    println!("[subrosa] cleared {n} queued session(s)");
    ExitCode::SUCCESS
}
