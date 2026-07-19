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
        if (1..=candidates.len()).contains(&idx) {
            return Some(candidates[idx - 1].1.clone());
        }
        return None;
    }
    let p = ans.strip_prefix("~/").map(|rest| paths::home().join(rest));
    Some(p.unwrap_or_else(|| PathBuf::from(ans)))
}

//---------------------------------------------------------------------------
// `init --claude-md`: append subrosa's standing CLAUDE.md instructions.

/// One standing instruction `init --claude-md` installs into CLAUDE.md, keyed by
/// its heading. Each section is upserted independently (append-only), so re-running
/// adds whichever section is missing without rewriting existing bytes. Every `body`
/// is byte-identical to a fenced block in README.md ("Make Claude use the archive
/// itself") — readme_pins_snippets keeps the two from drifting.
struct Section {
    /// Heading; its presence anywhere in the file means the section is installed.
    marker: &'static str,
    body: &'static str,
}

const SECTIONS: &[Section] = &[
    Section {
        marker: "## Memory recall (subrosa)",
        body: r#"## Memory recall (subrosa)

Every past Claude Code session is archived locally and searchable with
`subrosa search "<keywords>"` — scope with `--project <name>`, narrow by date or
tag with `--after`/`--before`/`--tag`, more results with `-n 20`, and retry with
`--fuzzy` if an exact search finds nothing (partial names, small typos).
(If `subrosa` isn't on PATH, it's at `~/.claude/subrosa/bin/subrosa`.)
At the start of any task — investigating, debugging, designing, reviewing, or when
a ticket, environment, resource, person, or past decision comes up — search the
archive first and build on what past sessions already worked out instead of
starting cold. Announce the search ("Searching past sessions for [topic]...") and
cite hits with their date. Skip only for trivial one-liners. `MEMORY.md` is
generated — never hand-edit it; update facts with `subrosa fact` + `subrosa generate`,
or run `/subrosa:checkpoint`.
"#,
    },
    Section {
        marker: "## Memory auto-checkpoint (subrosa)",
        body: r#"## Memory auto-checkpoint (subrosa)

When a `[subrosa] ACTION REQUIRED` note says sessions are queued for checkpoint
(or `subrosa pending` is non-empty), run the `/subrosa:checkpoint-backlog` skill
in the background — never before or blocking the task you're working on. It saves
the durable facts from each queued session into that project's memory, then clears
the queue as it finishes. Skip it silently when nothing is queued.
"#,
    },
];

enum ClaudeMd {
    /// Markers of the sections newly appended this run.
    Written(Vec<&'static str>),
    /// Every section was already present.
    AlreadyPresent,
}

pub fn append_claude_md() -> ExitCode {
    let path = paths::claude_md();
    match upsert_claude_md(&path) {
        Ok(ClaudeMd::Written(markers)) => {
            for m in &markers {
                println!("[subrosa] appended \"{m}\" to {}", path.display());
            }
            println!(
                "[subrosa] subrosa's CLAUDE.md sections cost ~250 tokens of always-loaded context in total; delete a section to undo"
            );
            ExitCode::SUCCESS
        }
        Ok(ClaudeMd::AlreadyPresent) => {
            println!(
                "[subrosa] {} already has subrosa's CLAUDE.md sections — nothing to do",
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
/// dir) is created. Appends each section whose marker is absent, separated by a
/// blank line. NotFound reads count as empty; other read errors abort.
fn upsert_claude_md(path: &std::path::Path) -> std::io::Result<ClaudeMd> {
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut additions = String::new();
    let mut tail = existing.clone();
    let mut appended = Vec::new();
    for s in SECTIONS {
        if existing.contains(s.marker) {
            continue;
        }
        let sep = if tail.is_empty() || tail.ends_with("\n\n") {
            ""
        } else if tail.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        additions.push_str(sep);
        additions.push_str(s.body);
        tail.push_str(sep);
        tail.push_str(s.body);
        appended.push(s.marker);
    }
    if appended.is_empty() {
        return Ok(ClaudeMd::AlreadyPresent);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(additions.as_bytes())?;
    Ok(ClaudeMd::Written(appended))
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

    /// All sections in order, joined the way upsert writes them into an empty file
    /// (each body ends in "\n", so one blank line falls between them).
    fn all_sections_joined() -> String {
        let mut out = String::new();
        for (i, s) in SECTIONS.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(s.body);
        }
        out
    }

    #[test]
    fn creates_file_and_parent_dir() {
        let p = tmp("create");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert!(matches!(upsert_claude_md(&p), Ok(ClaudeMd::Written(_))));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), all_sections_joined());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn appends_after_blank_line_separator() {
        let p = tmp("append");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# my rules").unwrap(); // no trailing newline
        assert!(matches!(upsert_claude_md(&p), Ok(ClaudeMd::Written(_))));
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, format!("# my rules\n\n{}", all_sections_joined()));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn second_run_is_a_no_op() {
        let p = tmp("noop");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        assert!(matches!(upsert_claude_md(&p), Ok(ClaudeMd::Written(_))));
        let before = std::fs::read_to_string(&p).unwrap();
        assert!(matches!(upsert_claude_md(&p), Ok(ClaudeMd::AlreadyPresent)));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// A file that already has one section (e.g. an old install) gets only the
    /// missing section appended — the existing one is never duplicated.
    #[test]
    fn appends_only_missing_section() {
        let p = tmp("partial");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, SECTIONS[0].body).unwrap();
        let appended = match upsert_claude_md(&p).unwrap() {
            ClaudeMd::Written(m) => m,
            ClaudeMd::AlreadyPresent => panic!("expected Written, got AlreadyPresent"),
        };
        let want: Vec<_> = SECTIONS[1..].iter().map(|s| s.marker).collect();
        assert_eq!(appended, want);
        let got = std::fs::read_to_string(&p).unwrap();
        assert_eq!(got, all_sections_joined());
        assert_eq!(got.matches(SECTIONS[0].marker).count(), 1);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn readme_pins_snippets() {
        let readme = include_str!("../README.md");
        for s in SECTIONS {
            assert!(
                readme.contains(s.body),
                "README.md missing section: {}",
                s.marker
            );
        }
    }
}
