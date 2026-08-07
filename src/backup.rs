//! Consistent DB snapshots via SQLite's online backup API. Local timestamped
//! snapshots stay in the data dir; the latest is optionally mirrored as one
//! static file to a synced folder (iCloud/Dropbox/...) for off-machine
//! durability. A static single file is sync-safe — only the live DB (with its
//! -wal/-shm sidecars) must never live in a synced folder.

use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::Connection;

use crate::{crypt, db, paths};

const THROTTLE_SECONDS: u64 = 24 * 3600;
const STALE_TMP_SECONDS: u64 = 3600;
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
    // Before anything that can fail. A readable copy that reappeared next to a
    // sealed one must not sit there through a run that errors out or gets
    // throttled — those are exactly the runs nobody looks at. Both entry
    // points already purged before opening the DB; this is the quiet backstop
    // for any other caller, and it's idempotent. Up to three config reads, two
    // exists() checks and one read_dir per named folder.
    if use_mirror {
        purge_mirror(false);
    }
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
    paths::chmod600(&tmp);
    fs::rename(&tmp, &dest)?;

    let snaps = snapshots();
    if snaps.len() > keep {
        for old in &snaps[..snaps.len() - keep] {
            let _ = fs::remove_file(old);
        }
    }
    sweep_stale_tmp(&dir, ".snapshot-");

    let mut label = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if use_mirror {
        if let Some(mirror_dir) = paths::mirror() {
            let mtmp = mirror_dir.join(format!(".subrosa-latest-{}.tmp", std::process::id()));
            match mirror_copy(&dest, &mirror_dir, &mtmp) {
                // The new file is already in place, so a cleanup failure now
                // is a different problem from never writing it. Saying
                // "skipped" here would be a lie.
                Ok((suffix, sealed)) => match sealed.then(|| purge_plaintext(&mirror_dir)) {
                    Some(Err(e)) => {
                        label.push_str(" + mirror (encrypted; cleanup failed)");
                        log_mirror(&format!("mirror updated, plaintext cleanup failed: {e}"));
                    }
                    _ => label.push_str(suffix),
                },
                Err(e) => {
                    let _ = fs::remove_file(&mtmp);
                    log_mirror(&format!("mirror skipped: {e}"));
                }
            }
            sweep_stale_tmp(&mirror_dir, ".subrosa-latest-");
        }
    }
    Ok(Some(label))
}

/// Put the snapshot in the mirror folder, sealed when a passphrase is set.
/// Returns the label suffix and whether it was sealed, so the caller knows to
/// sweep the plaintext behind it. Same temp+rename discipline as the local
/// snapshot: a synced folder must never see a torn file mid-copy. Anything
/// that goes wrong fails the whole mirror — writing plaintext instead is never
/// an option.
fn mirror_copy(
    dest: &Path,
    dir: &Path,
    tmp: &Path,
) -> Result<(&'static str, bool), Box<dyn Error>> {
    fs::create_dir_all(dir)?;
    // Same discipline as every other temp write here: never inherit whatever
    // is sitting on that name, symlink included.
    let _ = fs::remove_file(tmp);
    let key = paths::mirror_passphrase();
    // Purge before anything can bail out, and before sealing: a twin that came
    // back from cloud version history must not outlive a broken passphrase,
    // and sealing a big archive takes seconds during which the sync client
    // uploads whatever it can see.
    if encryption_intended(dir, &key) {
        purge_plaintext(dir)?;
    }
    let Some(pass) = key? else {
        // Never silently downgrade: a sealed mirror plus a passphrase that
        // went missing is a configuration problem, not permission to publish
        // the archive in the clear. Deleting the .enc is how you opt out.
        if let Some(enc) = sealed_present(dir) {
            return Err(format!(
                "encrypted mirror present but no passphrase configured — not downgrading; \
                 set SUBROSA_MIRROR_PASSPHRASE or the mirror_passphrase config key, \
                 or delete {} to disable encryption",
                enc.display()
            )
            .into());
        }
        fs::copy(dest, tmp)?;
        fs::rename(tmp, dir.join("subrosa-latest.db"))?;
        return Ok((" + mirror", false));
    };
    let sealed = crypt::encrypt(&pass, fs::read(dest)?)?;
    fs::write(tmp, &sealed)?;
    paths::chmod600(tmp);
    fs::rename(tmp, dir.join("subrosa-latest.db.enc"))?;
    Ok((" + mirror (encrypted)", true))
}

/// With a passphrase set, nothing readable may sit in the synced folder: not
/// the old `subrosa-latest.db`, not a `.tmp` left by a plaintext copy that
/// died mid-write. Every failure propagates — a purge that quietly did nothing
/// is the exact case this exists to prevent.
fn purge_plaintext(dir: &Path) -> Result<(), Box<dyn Error>> {
    remove_if_present(&dir.join("subrosa-latest.db"))?;
    // An evicted plaintext mirror is only a placeholder on this machine, but
    // the readable object is still in the cloud — deleting the placeholder is
    // what removes it. Named exactly, so it can never hit the sealed
    // `.subrosa-latest.db.enc.icloud` that sealed_present() looks for.
    remove_if_present(&dir.join(".subrosa-latest.db.icloud"))?;
    let listing = match fs::read_dir(dir) {
        Ok(rd) => rd,
        // A mirror folder that isn't mounted right now has nothing to leak,
        // and complaining about it every session end helps nobody.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot list {}: {e}", dir.display()).into()),
    };
    for entry in listing {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let real = unplaceholder(&name);
        if !(real.starts_with(".subrosa-latest-") && real.ends_with(".tmp")) {
            continue;
        }
        let path = entry.path();
        // An evicted tmp can't be read to see what's in it, and a half-written
        // copy is never worth keeping either way — so it goes on sight.
        if real != name {
            remove_if_present(&path)?;
            continue;
        }
        // Both branches write the same tmp name, so the name alone can't say
        // what's inside. A SQLite header means a readable copy, and that goes
        // now; anything else may be a live encrypted write, so it waits for
        // the age gate.
        let stale = age_secs(&path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if is_plaintext(&path) || stale.is_some_and(|a| a > STALE_TMP_SECONDS) {
            remove_if_present(&path)?;
        }
    }
    Ok(())
}

/// iCloud swaps an evicted file `X` for a placeholder named `.X.icloud`. Strip
/// that wrapper so the name checks see the file it stands for.
fn unplaceholder(name: &str) -> &str {
    name.strip_suffix(".icloud")
        .and_then(|n| n.strip_prefix('.'))
        .unwrap_or(name)
}

/// Does the file open with SQLite's header? Anything unreadable or shorter
/// than the header is "don't know", which the caller treats as keep.
fn is_plaintext(p: &Path) -> bool {
    let mut head = [0u8; 16];
    fs::File::open(p)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head == b"SQLite format 3\0"
}

/// The sealed mirror if one is there. iCloud swaps an evicted file for a
/// dot-prefixed `.icloud` placeholder, and that still means "encrypted mirror
/// present" — otherwise an evicted file plus a lost passphrase downgrades.
fn sealed_present(dir: &Path) -> Option<std::path::PathBuf> {
    ["subrosa-latest.db.enc", ".subrosa-latest.db.enc.icloud"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists())
}

/// Is encryption meant to be on? A passphrase that resolves at all says yes —
/// an error means one is configured but unreadable, which is still a request
/// for encryption — and so does a sealed mirror already sitting there.
fn encryption_intended(dir: &Path, key: &Result<Option<String>, String>) -> bool {
    !matches!(key, Ok(None)) || sealed_present(dir).is_some()
}

/// Clear any readable copy out of every folder the mirror settings name.
/// Deliberately needs no database: it runs first at both entry points, so an
/// archive that won't even open can't be the reason a readable copy stays in
/// the cloud.
pub fn purge_mirror_plaintext() {
    purge_mirror(true);
}

/// `report` is off for the backstop call inside `snapshot()`: both entry
/// points already purged and logged, and one root cause writing three
/// identical lines into a log with no rotation helps nobody.
fn purge_mirror(report: bool) {
    let target = paths::mirror();
    let key = paths::mirror_passphrase();
    for dir in paths::mirror_risk_paths() {
        // Only one folder wins the write. Anywhere else — and anywhere at all
        // under an opt-out — we're a guest: touch it only when a sealed file
        // says these contents were meant to be encrypted, because then
        // clearing a readable twin takes data OUT of the cloud rather than
        // adding any. No .enc there means hands off.
        let writing_here = target.as_deref() == Some(dir.as_path());
        if !writing_here && sealed_present(&dir).is_none() {
            continue;
        }
        if !encryption_intended(&dir, &key) {
            continue;
        }
        if let Err(e) = purge_plaintext(&dir) {
            if report {
                // Only call it a skipped mirror when one was actually going to
                // be written there — otherwise nothing was ever planned.
                log_mirror(&if writing_here {
                    format!("mirror skipped: {e}")
                } else {
                    format!(
                        "could not clear the readable copy in {}: {e}",
                        dir.display()
                    )
                });
            }
        }
    }
}

fn remove_if_present(p: &Path) -> Result<(), Box<dyn Error>> {
    match fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {e}", p.display()).into())
        }
        _ => Ok(()),
    }
}

/// Seconds since the file was last written. None when it's gone, or the clock
/// won't say — both mean "leave it alone".
fn age_secs(p: &Path) -> std::io::Result<Option<u64>> {
    match fs::metadata(p) {
        Ok(md) => Ok(md
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// run.sh throws hook stderr away, so a mirror problem has to reach the log
/// too — that's the one path nobody is watching. eprintln stays for CLI runs.
fn log_mirror(msg: &str) {
    eprintln!("[subrosa] {msg}");
    crate::hook::log(msg);
}

/// Drop temp files left by a process that died mid-copy, sparing active ones.
/// Matches evicted placeholders too, on the placeholder's own age — otherwise
/// an evicted partial copy would sit in the folder forever.
fn sweep_stale_tmp(dir: &Path, prefix: &str) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let real = unplaceholder(&name);
        if real.starts_with(prefix)
            && real.ends_with(".tmp")
            && age_secs(&e.path())
                .ok()
                .flatten()
                .is_some_and(|a| a > STALE_TMP_SECONDS)
        {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Hook path: throttled, quiet, mirror on. Never raises past the caller's log.
pub fn throttled(conn: &Connection) -> Result<Option<String>, Box<dyn Error>> {
    snapshot(conn, false, DEFAULT_KEEP, true)
}
