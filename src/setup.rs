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
