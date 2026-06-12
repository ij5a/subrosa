//! Consistent DB snapshots via SQLite's online backup API. Local timestamped
//! snapshots stay in the data dir; the latest is optionally mirrored as one
//! static file to a synced folder (iCloud/Dropbox/...) for off-machine
//! durability. A static single file is sync-safe — only the live DB (with its
//! -wal/-shm sidecars) must never live in a synced folder.

use std::error::Error;
use std::fs;
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::Connection;

use crate::{db, paths};

const THROTTLE_SECONDS: u64 = 24 * 3600;
pub const DEFAULT_KEEP: usize = 7;

fn snapshots() -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = fs::read_dir(paths::backups_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("snapshot-") && n.ends_with(".db"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn newest_age_secs() -> Option<u64> {
    let newest = snapshots().pop()?;
    let mtime = fs::metadata(&newest).ok()?.modified().ok()?;
    mtime.elapsed().ok().map(|d| d.as_secs())
}

/// Take a snapshot. Returns None when throttled (a recent snapshot exists and
/// force is off). Mirrors the latest to the configured synced folder.
pub fn snapshot(
    conn: &Connection,
    force: bool,
    keep: usize,
    use_mirror: bool,
) -> Result<Option<String>, Box<dyn Error>> {
    if !paths::db_path().exists() {
        return Err("no DB to back up".into());
    }
    let dir = paths::backups_dir();
    fs::create_dir_all(&dir)?;
    if !force {
        if let Some(age) = newest_age_secs() {
            if age < THROTTLE_SECONDS {
                return Ok(None); // throttled — stay silent for the hook
            }
        }
    }
    // Filename timestamp from the ISO clock: 2026-06-12T06:20:02+00:00 → 20260612-062002
    let ts: String = db::now_iso()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    let dest = dir.join(format!("snapshot-{}-{}.db", &ts[..8], &ts[8..]));
    // Page-by-page copy into a per-process temp file, then an atomic rename:
    // hooks racing an expired throttle can't clobber a finished snapshot, and a
    // failed copy never leaves a partial one. Busy/Locked steps retry (bounded).
    let tmp = dir.join(format!(".snapshot-{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let copy = (|| -> Result<(), Box<dyn Error>> {
        let mut dst = Connection::open(&tmp)?;
        let bk = Backup::new(conn, &mut dst)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match bk.step(100)? {
                StepResult::Done => return Ok(()),
                StepResult::Busy | StepResult::Locked => {
                    if Instant::now() >= deadline {
                        return Err("timed out waiting for the database lock".into());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => {} // More: keep stepping (also covers future variants)
            }
        }
    })();
    if let Err(e) = copy {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    chmod600(&tmp);
    fs::rename(&tmp, &dest)?;

    let snaps = snapshots();
    if snaps.len() > keep {
        for old in &snaps[..snaps.len() - keep] {
            let _ = fs::remove_file(old);
        }
    }
    // Sweep temp files left by hooks that died mid-copy (spare active ones).
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let stale = name.starts_with(".snapshot-")
                && name.ends_with(".tmp")
                && e.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_secs() > 3600)
                    .unwrap_or(false);
            if stale {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    let mut label = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if use_mirror {
        if let Some(mirror_dir) = paths::mirror() {
            // Same temp+rename discipline as the local snapshot: a synced
            // folder must never see a torn subrosa-latest.db mid-copy.
            let mtmp = mirror_dir.join(format!(".subrosa-latest-{}.tmp", std::process::id()));
            match fs::create_dir_all(&mirror_dir)
                .and_then(|_| fs::copy(&dest, &mtmp))
                .and_then(|_| fs::rename(&mtmp, mirror_dir.join("subrosa-latest.db")))
            {
                Ok(_) => label.push_str(" + mirror"),
                Err(e) => {
                    let _ = fs::remove_file(&mtmp);
                    eprintln!("[subrosa] mirror skipped: {e}");
                }
            }
        }
    }
    Ok(Some(label))
}

/// Hook path: throttled, quiet, mirror on. Never raises past the caller's log.
pub fn throttled(conn: &Connection) -> Result<Option<String>, Box<dyn Error>> {
    snapshot(conn, false, DEFAULT_KEEP, true)
}

#[cfg(unix)]
fn chmod600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn chmod600(_path: &std::path::Path) {}
