//! One-time interactive setup: create the schema and ask where backup
//! snapshots should mirror to (iCloud / Dropbox / Google Drive / OneDrive /
//! a custom folder / none). The live DB always stays local — synced folders
//! corrupt live SQLite WAL files — so the mirror only ever receives a single
//! static snapshot file.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use crate::{backup, db, paths};

/// Cloud-synced folders worth suggesting, in the order they're found.
fn detect_candidates() -> Vec<(String, PathBuf)> {
    let home = paths::home();
    let mut out = Vec::new();
    let icloud = home
        .join("Library")
        .join("Mobile Documents")
        .join("com~apple~CloudDocs");
    if icloud.is_dir() {
        out.push(("iCloud Drive".to_string(), icloud));
    }
    // macOS mounts Dropbox / Google Drive / OneDrive under ~/Library/CloudStorage.
    if let Ok(entries) = std::fs::read_dir(home.join("Library").join("CloudStorage")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let label = e
                    .file_name()
                    .to_string_lossy()
                    .split('-')
                    .next()
                    .unwrap_or("cloud folder")
                    .to_string();
                out.push((label, p));
            }
        }
    }
    for (label, rel) in [("Dropbox", "Dropbox"), ("Google Drive", "Google Drive")] {
        let p = home.join(rel);
        if p.is_dir() && !out.iter().any(|(_, q)| q == &p) {
            out.push((label.to_string(), p));
        }
    }
    out
}

pub fn run(mirror_flag: Option<PathBuf>, no_mirror: bool) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("[subrosa] data dir: {}", paths::mem_dir().display());
    println!(
        "[subrosa] the live DB stays here, outside synced folders — cloud sync corrupts live SQLite."
    );

    let chosen: Option<PathBuf> = if no_mirror {
        None
    } else if let Some(m) = mirror_flag {
        Some(m)
    } else if std::io::stdin().is_terminal() {
        ask_interactively()
    } else {
        // Non-interactive (hooks, CI): change nothing that wasn't passed explicitly.
        println!("[subrosa] non-interactive: keeping current mirror setting (use --mirror PATH or --no-mirror).");
        match paths::mirror() {
            Some(m) => {
                println!("[subrosa] mirror: {}", m.display());
                return ExitCode::SUCCESS;
            }
            None => {
                println!("[subrosa] mirror: none");
                return ExitCode::SUCCESS;
            }
        }
    };

    match chosen {
        Some(dir) => {
            let dir = if dir.ends_with("subrosa") {
                dir
            } else {
                dir.join("subrosa")
            };
            if let Err(e) = paths::config_set("mirror", &dir.to_string_lossy()) {
                eprintln!("[subrosa] could not save config: {e}");
                return ExitCode::FAILURE;
            }
            println!(
                "[subrosa] backup snapshots will mirror to: {}",
                dir.display()
            );
            match backup::snapshot(&conn, true, backup::DEFAULT_KEEP, true) {
                Ok(Some(label)) => println!("[subrosa] first backup: {label}"),
                Ok(None) => {}
                Err(e) => eprintln!("[subrosa] first backup failed: {e}"),
            }
        }
        None => {
            let _ = paths::config_set("mirror", "none");
            println!(
                "[subrosa] no mirror — snapshots stay in {}",
                paths::backups_dir().display()
            );
        }
    }
    println!("[subrosa] setup done. Try: subrosa ingest --sweep && subrosa search <terms>");
    ExitCode::SUCCESS
}

/// Number the detected cloud folders and read one choice from stdin.
fn ask_interactively() -> Option<PathBuf> {
    let candidates = detect_candidates();
    println!("[subrosa] where should backup snapshots mirror to (off-machine durability)?");
    for (i, (label, path)) in candidates.iter().enumerate() {
        println!(
            "  {}) {:<14} {}",
            i + 1,
            label,
            path.join("subrosa").display()
        );
    }
    println!("  n) no mirror — local snapshots only");
    print!(
        "choice [1-{}/n, or type a folder path]: ",
        candidates.len().max(1)
    );
    let _ = std::io::stdout().flush();

    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return None;
    }
    let ans = line.trim();
    if ans.is_empty() || ans.eq_ignore_ascii_case("n") || ans.eq_ignore_ascii_case("none") {
        return None;
    }
    if let Ok(idx) = ans.parse::<usize>() {
        if idx >= 1 && idx <= candidates.len() {
            return Some(candidates[idx - 1].1.clone());
        }
        return None;
    }
    let p = ans.strip_prefix("~/").map(|rest| paths::home().join(rest));
    Some(p.unwrap_or_else(|| PathBuf::from(ans)))
}

//---------------------------------------------------------------------------
// `init --claude-md`: append the standing "search the archive" instruction.

/// Byte-identical to the fenced block in README.md ("Make Claude search the
/// archive itself") — readme_pins_snippet keeps the two from drifting.
const CLAUDE_MD_SNIPPET: &str = r#"## Memory recall (subrosa)

Every past Claude Code session is archived locally and searchable with
`subrosa search "<keywords>"` — scope with `--project <name>`, more results with
`-n 20`. (If `subrosa` isn't on PATH, it's at `~/.claude/subrosa/bin/subrosa`.)
At the start of any task — investigating, debugging, designing, reviewing, or when
a ticket, environment, resource, person, or past decision comes up — search the
archive first and build on what past sessions already worked out instead of
starting cold. Announce the search ("Searching past sessions for [topic]...") and
cite hits with their date. Skip only for trivial one-liners. `MEMORY.md` is
generated — never hand-edit it; update facts with `subrosa fact` + `subrosa generate`,
or run `/subrosa:checkpoint`.
"#;

/// Present anywhere in the file (even pasted from the README) means the
/// instruction is already installed — appending a second copy is the worse bug.
const CLAUDE_MD_MARKER: &str = "## Memory recall (subrosa)";

enum ClaudeMd {
    Written,
    AlreadyPresent,
}

pub fn append_claude_md() -> ExitCode {
    let path = paths::claude_md();
    match upsert_claude_md(&path) {
        Ok(ClaudeMd::Written) => {
            println!(
                "[subrosa] appended \"{CLAUDE_MD_MARKER}\" to {} — Claude will now search the archive at task start",
                path.display()
            );
            println!(
                "[subrosa] it costs ~150 tokens of always-loaded context; delete the section to undo"
            );
            ExitCode::SUCCESS
        }
        Ok(ClaudeMd::AlreadyPresent) => {
            println!(
                "[subrosa] {} already has a \"{CLAUDE_MD_MARKER}\" section — nothing to do",
                path.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[subrosa] cannot update {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// Append-only: existing bytes are never rewritten; a missing file (and parent
/// dir) is created. NotFound reads count as empty; other read errors abort.
fn upsert_claude_md(path: &std::path::Path) -> std::io::Result<ClaudeMd> {
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if existing.contains(CLAUDE_MD_MARKER) {
        return Ok(ClaudeMd::AlreadyPresent);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let sep = if existing.is_empty() || existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    write!(f, "{sep}{CLAUDE_MD_SNIPPET}")?;
    Ok(ClaudeMd::Written)
}

#[cfg(test)]
mod claude_md_tests {
    use super::*;

    /// Unique throwaway path per test so the parallel test runner never races.
    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("subrosa-claudemd-{}-{name}", std::process::id()))
            .join("CLAUDE.md")
    }

    #[test]
    fn creates_file_and_parent_dir() {
        let p = tmp("create");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert!(matches!(upsert_claude_md(&p), Ok(ClaudeMd::Written)));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), CLAUDE_MD_SNIPPET);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn appends_after_blank_line_separator() {
        let p = tmp("append");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# my rules").unwrap(); // no trailing newline
        assert!(matches!(upsert_claude_md(&p), Ok(ClaudeMd::Written)));
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, format!("# my rules\n\n{CLAUDE_MD_SNIPPET}"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn second_run_is_a_no_op() {
        let p = tmp("noop");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert!(matches!(upsert_claude_md(&p), Ok(ClaudeMd::Written)));
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(matches!(upsert_claude_md(&p), Ok(ClaudeMd::AlreadyPresent)));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn readme_pins_snippet() {
        assert!(include_str!("../README.md").contains(CLAUDE_MD_SNIPPET));
    }
}
