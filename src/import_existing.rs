//! One-time import of a project's MEMORY.md + leaf files into the facts table.
//!
//! Backs up the memdir first, then parses the index pointers and leaf frontmatter
//! into rows. Idempotent: re-running updates rows in place and preserves status/pins.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::{db, facts, paths};

// `- [title](leaf.md) — hook` (em dash or ASCII hyphen separator)
const POINTER_RE_STR: &str = r"^- \[(.+?)\]\(([^)]+)\)\s*[—\-]\s*(.*)$";

/// Copy every file in `src` into `dest`, creating `dest` and any subdirectories.
/// The live memdir is one flat level of .md files, but we walk recursively anyway.
fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_symlink() {
            continue; // never follow links out of the tree being backed up
        }
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Derive the compact UTC timestamp used in the backup dir name from
/// `now_iso()` — consistent with every other timestamp in the crate.
fn compact_ts_from_iso(iso: &str) -> String {
    // Keep only ASCII digits, take first 14 (YYYYmmddHHMMSS).
    let digits: String = iso
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    if digits.len() == 14 {
        format!("{}-{}", &digits[..8], &digits[8..])
    } else {
        digits
    }
}

/// Back up the memdir tree to `paths::mem_dir()/memory-backups/<parent>-<timestamp>`.
fn backup(memdir: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let ts = compact_ts_from_iso(&db::now_iso());
    let parent_name = memdir
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let dest = paths::mem_dir()
        .join("memory-backups")
        .join(format!("{parent_name}-{ts}"));
    copy_tree(memdir, &dest)?;
    Ok(dest)
}

/// Parse MEMORY.md pointers into a map of `leaf_filename -> (index_seq, title, hook)`.
/// Sequence counts only lines that match the pointer pattern.
fn parse_index(memory_md: &Path) -> HashMap<String, (usize, String, String)> {
    let mut out = HashMap::new();
    let text = match fs::read_to_string(memory_md) {
        Ok(t) => t,
        Err(_) => return out,
    };
    let re = regex::Regex::new(POINTER_RE_STR).expect("static regex");
    let mut seq: usize = 0;
    for line in text.lines() {
        if let Some(caps) = re.captures(line) {
            let leaf = caps[2].trim().to_string();
            let title = caps[1].trim().to_string();
            let hook = caps[3].trim().to_string();
            out.insert(leaf, (seq, title, hook));
            seq += 1;
        }
    }
    out
}

/// Main entry point for `subrosa import`.
pub fn run(memdir: Option<PathBuf>, no_backup: bool, project: Option<String>) -> ExitCode {
    let conn = match db::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[subrosa] cannot open DB: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Resolve the memdir: arg > current_memdir(conn).
    let memdir: PathBuf = match memdir {
        Some(p) => paths::expanduser(&p),
        None => db::current_memdir(Some(&conn)),
    };

    if !memdir.is_dir() {
        eprintln!("[subrosa] not a directory: {}", memdir.display());
        return ExitCode::FAILURE;
    }

    // The project name is the parent dir name unless overridden.
    // Don't resolve() — the memory/ dir may be a symlink and resolving it would change
    // the project name to the backing store path instead of the canonical projects-dir name.
    let project: String = project.unwrap_or_else(|| {
        memdir
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });

    if !no_backup {
        match backup(&memdir) {
            Ok(dest) => println!("[subrosa] backed up -> {}", dest.display()),
            Err(e) => {
                eprintln!("[subrosa] backup failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let memory_md = memdir.join("MEMORY.md");
    let index = parse_index(&memory_md);
    println!("[subrosa] {} index pointers in MEMORY.md", index.len());

    let now = db::now_iso();

    // Collect sorted leaf .md files, excluding MEMORY.md.
    let mut leaves: Vec<PathBuf> = match fs::read_dir(&memdir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("md")
                    && p.file_name().and_then(|n| n.to_str()) != Some("MEMORY.md")
            })
            .collect(),
        Err(e) => {
            eprintln!("[subrosa] cannot read dir: {e}");
            return ExitCode::FAILURE;
        }
    };
    leaves.sort();

    let mut by_type: HashMap<String, usize> = HashMap::new();
    let mut orphans: usize = 0;

    for leaf in &leaves {
        let leaf_name = leaf
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let text = fs::read_to_string(leaf).unwrap_or_default();
        let fm = facts::parse_frontmatter(&text);

        let (index_seq, title, hook): (Option<i64>, String, String) =
            if let Some(&(seq, ref t, ref h)) = index.get(&leaf_name) {
                (Some(seq as i64), t.clone(), h.clone())
            } else {
                orphans += 1;
                // Title: frontmatter `name` or stem with underscores replaced by spaces.
                let t = fm
                    .get("name")
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_else(|| {
                        leaf.file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .replace('_', " ")
                    });
                // Hook: frontmatter `description` or empty string.
                let h = fm.get("description").cloned().unwrap_or_default();
                (None, t, h)
            };

        let ftype = {
            let raw = fm.get("type").map(|s| s.to_lowercase()).unwrap_or_default();
            if raw.is_empty() {
                facts::guess_type(&leaf_name).to_string()
            } else {
                raw
            }
        };

        *by_type.entry(ftype.clone()).or_insert(0) += 1;

        let pinned: i64 = {
            let v = fm
                .get("pinned")
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            if matches!(v.as_str(), "1" | "true" | "yes") {
                1
            } else {
                0
            }
        };

        let name = fm.get("name").cloned();
        let description = fm.get("description").cloned();
        let tags = fm.get("tags").cloned();
        let origin_session = fm.get("originSessionId").cloned();

        let result = conn.execute(
            r"
            INSERT INTO facts
              (project, name, type, title, hook, leaf_path, description, tags, index_seq,
               pinned, status, created_at, updated_at, origin_session)
            VALUES (?,?,?,?,?,?,?,?,?,?,'active',?,?,?)
            ON CONFLICT(project, leaf_path) DO UPDATE SET
              name        = excluded.name,
              type        = excluded.type,
              title       = excluded.title,
              hook        = excluded.hook,
              description = excluded.description,
              tags        = excluded.tags,
              index_seq   = excluded.index_seq,
              pinned      = MAX(facts.pinned, excluded.pinned),
              updated_at  = excluded.updated_at
            ",
            rusqlite::params![
                project,
                name,
                ftype,
                title,
                hook,
                leaf_name,
                description,
                tags,
                index_seq,
                pinned,
                now,
                now,
                origin_session,
            ],
        );

        if let Err(e) = result {
            eprintln!("[subrosa] failed to upsert {leaf_name}: {e}");
            return ExitCode::FAILURE;
        }
    }

    // Dangling pointers: index entries whose leaf file is absent.
    let leaf_names: std::collections::HashSet<String> = leaves
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
        .collect();
    let mut dangling: Vec<&String> = index.keys().filter(|l| !leaf_names.contains(*l)).collect();
    dangling.sort();

    println!(
        "[subrosa] imported {} facts into project '{project}'",
        leaves.len()
    );

    // by_type summary: "type=N, type=N" sorted by type name.
    let mut type_pairs: Vec<(&String, &usize)> = by_type.iter().collect();
    type_pairs.sort_by_key(|(k, _)| k.as_str());
    let summary = type_pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    println!("[subrosa] by type: {summary}");

    println!("[subrosa] orphan leaves (not in index): {orphans}");

    if !dangling.is_empty() {
        // Bracketed single-quoted list, e.g. ['a.md', 'b.md'] — the warning format
        // is part of the output contract pinned by golden tests.
        let list_repr = format!(
            "[{}]",
            dangling
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "[subrosa] WARNING: {} pointers have no leaf file: {list_repr}",
            dangling.len()
        );
    }

    ExitCode::SUCCESS
}
