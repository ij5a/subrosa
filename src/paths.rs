//! Data locations. Env-overridable so tests can point at a throwaway dir
//! without touching the live DB.

use std::io::Write;
use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Expand a leading `~` the way a shell would (clap doesn't).
pub fn expanduser(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        return home();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home().join(rest);
    }
    p.to_path_buf()
}

fn env_path(key: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(default)
}

pub fn mem_dir() -> PathBuf {
    env_path("SUBROSA_DIR", || home().join(".claude").join("subrosa"))
}

pub fn db_path() -> PathBuf {
    env_path("SUBROSA_DB", || mem_dir().join("memory.db"))
}

pub fn projects_dir() -> PathBuf {
    env_path("SUBROSA_PROJECTS_DIR", || {
        home().join(".claude").join("projects")
    })
}

/// Claude Code's user-global memory file — the `init --claude-md` target.
pub fn claude_md() -> PathBuf {
    env_path("SUBROSA_CLAUDE_MD", || {
        home().join(".claude").join("CLAUDE.md")
    })
}

pub fn pending_log() -> PathBuf {
    env_path("SUBROSA_PENDING_LOG", || {
        mem_dir().join("pending-checkpoint.log")
    })
}

pub fn hook_log() -> PathBuf {
    mem_dir().join("hook.log")
}

/// Recall dedup state: which source sessions were already injected into a
/// given live session, so repeated prompts on one topic stay silent.
pub fn recall_seen_log() -> PathBuf {
    mem_dir().join("recall-seen.log")
}

pub fn backups_dir() -> PathBuf {
    mem_dir().join("backups")
}

/// Plain KEY=VALUE config in the data dir. Tiny on purpose — no TOML dep.
pub fn config_path() -> PathBuf {
    mem_dir().join("config")
}

/// The whole config file. Only a missing file means "nothing configured" —
/// a permissions problem or invalid UTF-8 is an error, never an empty config.
/// Treating those as empty is how a stored passphrase silently disappears.
fn config_read() -> std::io::Result<String> {
    match std::fs::read_to_string(config_path()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        other => other,
    }
}

pub fn config_get(key: &str) -> std::io::Result<Option<String>> {
    Ok(config_read()?
        .lines()
        .find_map(|l| l.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty()))
}

pub fn config_set(key: &str, value: &str) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Never rewrite what we couldn't read: that would drop every other key,
    // the passphrase included.
    let existing = config_read().map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "cannot read {}: {e} — refusing to rewrite config",
                path.display()
            ),
        )
    })?;
    let prefix = format!("{key}=");
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.starts_with(&prefix) && !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    lines.push(format!("{key}={value}"));

    // The config can hold the mirror passphrase, so it's 0600 from the first
    // byte and renamed into place: no world-readable window, and a crash
    // can't leave the file truncated.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let write = create_new_600(&tmp)
        .and_then(|mut f| f.write_all((lines.join("\n") + "\n").as_bytes()))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// Create a file that must not exist yet, owner-only from the start.
/// `create_new` closes the check-then-write race an `exists()` test leaves open.
#[cfg(unix)]
pub(crate) fn create_new_600(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
pub(crate) fn create_new_600(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
pub(crate) fn chmod600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub(crate) fn chmod600(_path: &std::path::Path) {}

/// The two places the mirror folder can be named, in precedence order. The env
/// value is trimmed like the config one: `"none "` has to mean none, or the
/// sentinel is one stray space away from creating a folder literally called
/// "none " and filling it with a readable copy.
fn mirror_sources() -> [Option<String>; 2] {
    [
        std::env::var("SUBROSA_MIRROR")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        config_get("mirror").ok().flatten(),
    ]
}

/// Every distinct folder the sources name, in precedence order, skipping
/// "none". A hand-edited `mirror=~/Dropbox/...` would otherwise create a
/// folder literally named "~" and never match the real one.
fn named_folders(sources: [Option<String>; 2]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for v in sources.into_iter().flatten() {
        if v.eq_ignore_ascii_case("none") {
            continue;
        }
        let p = expanduser(std::path::Path::new(&v));
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Every folder a readable copy could end up in — for the guards whose job is
/// to stop that happening. Both sources count when they name different
/// folders: only one of them wins the write, but the other is just as
/// cloud-synced, and nothing cleans or guards it if we only look at the
/// winner. "none" is an opt-out from writing, never evidence that the other
/// source is safe. Use `mirror()` to decide where to write, and this to decide
/// what to protect.
pub fn mirror_risk_paths() -> Vec<PathBuf> {
    named_folders(mirror_sources())
}

/// Where backup snapshots mirror to (a synced folder is fine for single static
/// files — only the LIVE DB must stay out of cloud sync). Env beats config for
/// a real path, and "none" from EITHER source turns mirroring off.
///
/// Config and env are symmetric here on purpose. `mirror=none` is only ever
/// written deliberately — `setup --no-mirror`, or the rollback when setting up
/// encryption fails — so ambient shell state must not undo it; and someone who
/// exports `SUBROSA_MIRROR=none` for a session means it just as much.
///
/// An unreadable config yields no mirror from the config side. That is not a
/// guarantee on its own: with SUBROSA_MIRROR set, the write path still
/// resolves, and what stops a plaintext copy going out is `mirror_passphrase()`
/// erroring on the same unreadable file.
pub fn mirror() -> Option<PathBuf> {
    let sources = mirror_sources();
    if sources
        .iter()
        .flatten()
        .any(|v| v.eq_ignore_ascii_case("none"))
    {
        return None;
    }
    named_folders(sources).into_iter().next()
}

/// Passphrase for the mirror copy. `Ok(Some)` = encrypt, `Ok(None)` = nothing
/// configured, so the mirror stays plaintext as before. `Err` = configured but
/// unusable, which must never fall back to plaintext — the caller skips the
/// mirror instead. Env beats config; both are trimmed, so one value means the
/// same thing wherever it was set.
pub fn mirror_passphrase() -> Result<Option<String>, String> {
    if let Some(raw) = std::env::var_os("SUBROSA_MIRROR_PASSPHRASE") {
        match raw.to_str() {
            Some(s) if !s.trim().is_empty() => return Ok(Some(s.trim().to_string())),
            Some(_) => {}
            None => {
                return Err("SUBROSA_MIRROR_PASSPHRASE is set but isn't valid text — \
                            refusing to mirror in plaintext"
                    .into())
            }
        }
    }
    config_get("mirror_passphrase").map_err(|e| {
        format!(
            "cannot read {}: {e} — refusing to mirror",
            config_path().display()
        )
    })
}

/// How loud the SessionStart checkpoint-backlog nudge is: "loud" (default — an
/// imperative ACTION REQUIRED block so the backlog can't be missed), "quiet"
/// (the calm one-liner), or "off" (no checkpoint nudge at all). Env beats
/// config; an unset or unknown value falls back to loud.
pub fn checkpoint_nudge_mode() -> String {
    std::env::var("SUBROSA_CHECKPOINT_NUDGE")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| config_get("checkpoint_nudge").ok().flatten())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "loud".to_string())
}
