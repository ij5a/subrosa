mod backup;
mod db;
mod hook;
mod ingest;
mod paths;
mod redact;
mod search;
mod setup;

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
    cmd: Cmd,
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
    Init,
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
    },
    /// List sessions queued for checkpoint
    Pending,
    /// Claude Code hook entrypoints (read the hook JSON on stdin; never fail the session)
    #[command(subcommand)]
    Hook(HookEvent),
}

#[derive(Subcommand, Clone, Copy)]
pub enum HookEvent {
    /// SessionStart: catch-up ingest of any transcript that changed since its last archive
    SessionStart,
    /// SessionEnd: archive the ended session and queue it for checkpoint
    SessionEnd,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hook(event) => hook::run(event), // logs problems, always exits 0
        Cmd::Setup { mirror, no_mirror } => setup::run(mirror, no_mirror),
        Cmd::Backup {
            force,
            keep,
            no_mirror,
        } => run_backup(force, keep, no_mirror),
        Cmd::Init => run_init(),
        Cmd::Ingest {
            paths,
            sweep,
            quiet,
        } => run_ingest(paths, sweep, quiet),
        Cmd::Search {
            terms,
            limit,
            raw,
            project,
            session,
        } => search::run(&terms, limit, raw, project.as_deref(), session.as_deref()),
        Cmd::Pending => run_pending(),
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

fn run_init() -> ExitCode {
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
                    println!("[subrosa] sweep: {files} transcripts, {ingested} changed, +{inserted} turns");
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
