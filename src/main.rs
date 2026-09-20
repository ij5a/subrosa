mod backup;
mod crypt;
mod db;
mod embed;
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
mod wordpiece;

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
        /// Fail when any file has skipped lines or an unread trailing line
        #[arg(long)]
        require_complete: bool,
    },
    /// Decrypt an encrypted mirror snapshot back into a plain .db file
    Restore {
        /// Path to a subrosa-latest.db.enc
        file: PathBuf,
        /// Where to write the decrypted DB (default: ./<filename minus .enc>)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Ingest every transcript that changed since its last archive
    Sweep {
        /// Suppress output
        #[arg(long)]
        quiet: bool,
    },
    /// Search the archive (FTS5 by default; semantic direct or after an exact miss)
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
        /// Substring + small-typo matching via a trigram index (built on first use)
        #[arg(long)]
        fuzzy: bool,
        /// Match any term instead of all (OR instead of the default AND)
        #[arg(long)]
        any: bool,
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
        /// Rank by meaning instead of keywords (the index behind it builds itself)
        #[arg(long)]
        semantic: bool,
    },
    /// Index turns for semantic search now — normally automatic (downloads the model once, ~133 MB)
    Embed {
        /// Drop this model's stored vectors first, then embed every turn again
        #[arg(long)]
        rebuild: bool,
        /// The background run subrosa starts for itself: silent, low priority,
        /// gives up if another run is going
        #[arg(long, hide = true)]
        auto: bool,
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
    /// Inspect or mutate curated facts (search/link/doctor/list/upsert/archive/pin/unpin)
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
        /// Filter for `list` and `search`; shows only active facts by default
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
        /// Max bytes (default: <memdir>/.budget, else 23000)
        #[arg(long)]
        budget: Option<i64>,
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
        /// Print the archived sequence boundary for checkpointing.
        #[arg(long)]
        boundary: bool,
    },
    /// List sessions queued for checkpoint
    Pending,
    /// Remove a session from the queue + record the checkpoint high-water mark
    CheckpointDrop {
        /// Session id
        id: String,
        /// Mark only through this archived sequence
        #[arg(long)]
        max_seq: Option<i64>,
    },
    /// Conditionally queue a session (prunes empty/sub-agent-only sessions)
    CheckpointEnqueue {
        /// Session id
        id: String,
    },
    /// Mark the currently-running session checkpointed
    CheckpointMark {
        /// Session id or unique prefix (default: the cwd project's live session)
        id: Option<String>,
        /// Stamp this exact transcript sequence without ingesting newer turns
        #[arg(long)]
        max_seq: Option<i64>,
    },
    /// Empty the whole checkpoint queue (prefer checkpoint-drop per session)
    CheckpointClear {
        /// Confirm that every queued session may be removed
        #[arg(long)]
        confirm: bool,
    },
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
    /// Internal detached SessionEnd writer
    #[command(hide = true)]
    SessionEndWorker,
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
        Cmd::Hook(HookEvent::SessionEndWorker) => {
            let input = std::env::var("SUBROSA_SESSION_END_PAYLOAD")
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            let _ = hook::session_end_worker(&input);
            ExitCode::SUCCESS
        }
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
            require_complete,
        } => run_ingest(paths, sweep, quiet, require_complete),
        Cmd::Restore { file, out } => crypt::restore(file, out),
        Cmd::Sweep { quiet } => run_ingest(Vec::new(), true, quiet, false),
        Cmd::Search {
            terms,
            limit,
            raw,
            project,
            session,
            fuzzy,
            any,
            after,
            before,
            tag,
            exclude,
            context,
            semantic,
        } => search::run(
            &terms,
            limit,
            raw,
            project.as_deref(),
            session.as_deref(),
            fuzzy,
            any,
            after.as_deref(),
            before.as_deref(),
            &tag,
            &exclude,
            context,
            semantic,
        ),
        Cmd::Embed { rebuild, auto } => search::embed_backfill(rebuild, auto),
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
        Cmd::Session { id, tags, boundary } => session::dump(&id, tags, boundary),
        Cmd::Pending => run_pending(),
        Cmd::CheckpointDrop { id, max_seq } => session::drop_sid(&id, max_seq),
        Cmd::CheckpointEnqueue { id } => session::enqueue(&id),
        Cmd::CheckpointMark { id, max_seq } => session::mark_current(id.as_deref(), max_seq),
        Cmd::CheckpointClear { confirm } => run_checkpoint_clear(confirm),
    }
}

fn run_backup(force: bool, keep: usize, no_mirror: bool) -> ExitCode {
    // Ahead of db::connect(): a DB that won't open must not be the reason a
    // readable copy stays in the cloud.
    if !no_mirror {
        backup::purge_mirror_plaintext();
    }
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

fn run_ingest(paths: Vec<PathBuf>, sweep: bool, quiet: bool, require_complete: bool) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    if sweep {
        match ingest::sweep(&conn, &paths::projects_dir(), require_complete) {
            Ok((files, ingested, inserted, complete)) => {
                if !quiet {
                    println!(
                        "[subrosa] sweep: {files} transcripts, {ingested} changed, +{inserted} turns"
                    );
                }
                if require_complete && !complete {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
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
        let mut failed = false;
        for p in &paths {
            let result = if !p.is_file() {
                Err("path does not exist or is not a regular file".to_string())
            } else {
                ingest::ingest_file_report(&conn, p, require_complete).map_err(|e| e.to_string())
            };
            match result {
                Ok(report) => {
                    total += report.inserted;
                    if !quiet {
                        println!(
                            "[subrosa] {}: +{} turns ({} records scanned, {} lines skipped, partial tail: {})",
                            p.display(), report.inserted, report.scanned, report.skipped, report.partial_tail
                        );
                    }
                    if require_complete && (report.skipped > 0 || report.partial_tail) {
                        failed = true;
                    }
                }
                Err(e) => {
                    failed = true;
                    eprintln!("[subrosa] {}: {e}", p.display());
                }
            }
        }
        if !quiet && paths.len() > 1 {
            println!("[subrosa] total +{total} turns");
        }
        if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }
}

/// Print the queue, deduped by session id (a session can fire SessionEnd more than once).
fn run_pending() -> ExitCode {
    let conn = match db::connect_queue_readonly() {
        Ok(conn) => conn,
        Err(_) if !paths::db_path().try_exists().unwrap_or(true) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[subrosa] cannot read the checkpoint queue: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut stmt = match conn
        .prepare("SELECT enqueued_at, session_id FROM checkpoint_queue ORDER BY enqueue_seq DESC")
    {
        Ok(stmt) => stmt,
        Err(e) => {
            eprintln!("[subrosa] cannot read the checkpoint queue: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rows = match stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("[subrosa] cannot read the checkpoint queue: {e}");
            return ExitCode::FAILURE;
        }
    };
    for row in rows {
        match row {
            Ok((at, sid)) => println!("{at}\t{sid}"),
            Err(e) => {
                eprintln!("[subrosa] cannot read the checkpoint queue: {e}");
                return ExitCode::FAILURE;
            }
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

fn run_checkpoint_clear(confirm: bool) -> ExitCode {
    let conn = match db::connect() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    if !confirm {
        let n: i64 = match conn.query_row("SELECT count(*) FROM checkpoint_queue", [], |r| r.get(0))
        {
            Ok(n) => n,
            Err(e) => {
                eprintln!("[subrosa] cannot read the checkpoint queue: {e}");
                return ExitCode::FAILURE;
            }
        };
        eprintln!("[subrosa] refusing to clear {n} queued session(s): the queue lists sessions whose facts were never saved; use checkpoint-drop <id> for one session");
        return ExitCode::FAILURE;
    }
    let tx = match conn.unchecked_transaction() {
        Ok(tx) => tx,
        Err(e) => {
            eprintln!("[subrosa] cannot start queue clear: {e}");
            return ExitCode::FAILURE;
        }
    };
    let n: i64 = match tx.query_row("SELECT count(*) FROM checkpoint_queue", [], |r| r.get(0)) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[subrosa] cannot read the checkpoint queue: {e}");
            return ExitCode::FAILURE;
        }
    };
    if n > 0 {
        eprintln!("[subrosa] refusing to clear {n} queued session(s) that still need per-session verification; use checkpoint-drop <id>");
        return ExitCode::FAILURE;
    }
    // Clearing only drops pending work. It must not acknowledge turns that no one distilled.
    if let Err(e) = tx.execute("DELETE FROM checkpoint_queue", []) {
        eprintln!("[subrosa] cannot clear queue: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = tx.commit() {
        eprintln!("[subrosa] cannot commit queue clear: {e}");
        return ExitCode::FAILURE;
    }
    println!("[subrosa] cleared {n} queued session(s)");
    ExitCode::SUCCESS
}
