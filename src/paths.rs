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

/// A system tool, by absolute path — the first candidate that is really a file.
/// Never a bare name: several of these run from a session hook, where PATH is
/// whatever the session happened to have, and a name resolved through it is
/// someone else's binary waiting to be found.
pub fn system_tool(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .find_map(|c| trusted_tool(std::path::Path::new(c)))
}

/// The directories a system tool may actually live in. A symlinked candidate
/// has to land inside one of these: BusyBox ships `/bin/stty` as a link to
/// `/bin/busybox` — normal, and the whole Alpine world — while a link pointing
/// out to somewhere writable is what this is guarding against.
const TOOL_DIRS: [&str; 3] = ["/bin", "/usr/bin", "/sbin"];

/// One candidate, resolved. `Some` only when it really is a regular file whose
/// resolved home is still a trusted directory. The RESOLVED path comes back,
/// so the thing that was checked is the thing that gets executed.
fn trusted_tool(candidate: &std::path::Path) -> Option<PathBuf> {
    let real = candidate.canonicalize().ok()?;
    if !real.is_file() {
        return None;
    }
    let parent = real.parent()?;
    TOOL_DIRS
        .iter()
        .any(|d| parent == std::path::Path::new(d))
        .then_some(real)
}

/// How much of one of our own control files is ever read. They are all a few
/// lines of KEY=VALUE or one number; anything near this is not our file.
pub const CONTROL_FILE_MAX: u64 = 1 << 20;

/// Read one of our own small control files (config, indexer state, the
/// checkpoint queue, a `.budget`). `Ok(None)` means there is genuinely nothing
/// there; `Err` means there IS something and it can't be used, which callers
/// must never round down to "absent".
///
/// The checks matter because most of these are read inside a hook: a FIFO or a
/// device node would block the session forever, a huge file would eat its
/// memory, and a DANGLING SYMLINK reads as plain ENOENT — indistinguishable
/// from an absent file, which is how a `semantic=off` in an evicted
/// cloud-synced config silently turns itself back on.
///
/// ponytail: the stat and the open are two steps. Anything that could swap the
/// file in between already has write access to a 0700 dir and could just write
/// the file.
pub fn read_control_file(path: &std::path::Path, max: u64) -> std::io::Result<Option<String>> {
    use std::io::Read as _;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        // Nothing here at all — the only shape that means "not configured".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if meta.is_symlink() {
        // Resolve by hand so a dangling link is an error, not an absence.
        let target = std::fs::metadata(path).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("{} is a symlink that leads nowhere: {e}", path.display()),
            )
        })?;
        if !target.is_file() {
            return Err(std::io::Error::other(format!(
                "{} is a symlink to something that isn't a plain file",
                path.display()
            )));
        }
    } else if !meta.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a plain file",
            path.display()
        )));
    }
    let mut text = String::new();
    let read = std::fs::File::open(path)?
        .take(max + 1)
        .read_to_string(&mut text)?;
    if read as u64 > max {
        return Err(std::io::Error::other(format!(
            "{} is bigger than {max} bytes — refusing to read it",
            path.display()
        )));
    }
    Ok(Some(text))
}

/// Is this somewhere we may append to? Absent is fine — the append creates it.
/// Anything present that isn't a plain file is not ours to write to: opening a
/// FIFO for append blocks until someone reads it, and the callers run inside a
/// hook.
///
/// ponytail: a stat, then an open, with a window in between. Closing it needs
/// `O_NOFOLLOW`/`O_NONBLOCK` on the descriptor itself, which costs either
/// `libc` as a direct dependency — the short dependency list is a stated
/// property of this thing — or hardcoded flag values that differ between macOS
/// and Linux. Reviewed and turned down on purpose: winning that race needs
/// write access inside a 0700 directory whose `memory.db` holds every
/// transcript you own, so it buys an attacker nothing they can't already read.
/// The case this check is really for is the benign one — a `hook.log`
/// symlinked to /dev/null to silence logging — and it handles that. Don't
/// reopen without a new argument.
pub fn appendable(path: &std::path::Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.is_file(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Replace one of our own control files, whole — the counterpart to
/// `read_control_file`, and the only way any of them should be rewritten.
///
/// The existing path is never opened: a sibling temp file is written 0600 and
/// renamed over the target. That kills symlink truncation by construction
/// rather than by checking for it — a symlinked path gets replaced by our own
/// file instead of quietly rewriting whatever it pointed at — and a FIFO is
/// never opened for writing, so nothing can block. The rename is atomic, so a
/// crash leaves the old file rather than half the new one.
pub fn write_control_file(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    if !appendable(path) {
        return Err(std::io::Error::other(format!(
            "{} is not a plain file — refusing to write it",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Process-unique, not thread-unique. That covers what actually happens —
    // separate hook processes, and the one coordinating thread that writes
    // embed.state — but two threads in ONE process writing the same control
    // file would collide on this name.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let write = create_new_600(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .and_then(|_| std::fs::rename(&tmp, path));
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// The whole config file. Only a genuinely missing file means "nothing
/// configured" — a permissions problem, a dangling symlink or invalid UTF-8 is
/// an error, never an empty config. Treating those as empty is how a stored
/// passphrase silently disappears, and how a `semantic=off` turns itself on.
fn config_read() -> std::io::Result<String> {
    Ok(read_control_file(&config_path(), CONTROL_FILE_MAX)?.unwrap_or_default())
}

/// One KEY=VALUE line out of a config-shaped file. Shared with the indexer's
/// `embed.state`, which is written in the same tiny format.
pub fn kv_get(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn config_get(key: &str) -> std::io::Result<Option<String>> {
    Ok(kv_get(&config_read()?, key))
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

    // The config can hold the mirror passphrase, so it goes out the same way
    // every control file does: 0600 from the first byte, renamed into place.
    write_control_file(&path, &(lines.join("\n") + "\n"))
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

/// Where the embedding model's files are downloaded, one folder per model.
/// `subrosa embed` and `search --semantic` load them; plain search checks readiness
/// before an automatic semantic retry after an exact miss.
pub fn models_dir() -> PathBuf {
    mem_dir().join("models")
}

/// Whether subrosa keeps the semantic index current on its own: "on" (default)
/// or "off". Off stops every automatic run and every download — no subrosa
/// process touches the network while it's set. Env beats config.
///
/// `Err` means the config file itself couldn't be read, and callers treat that
/// as off: a file that might say off must never default to on and start
/// downloading. Same fail-closed rule as an unreadable `.budget`.
pub fn semantic_mode() -> Result<String, String> {
    if let Some(v) = std::env::var("SUBROSA_SEMANTIC")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        return Ok(v.trim().to_ascii_lowercase());
    }
    match config_get("semantic") {
        Ok(v) => Ok(v
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "on".to_string())),
        Err(e) => Err(format!("cannot read {}: {e}", config_path().display())),
    }
}

/// The automatic indexer's retry state. Absence means healthy — a run that
/// finishes deletes it.
pub fn embed_state_path() -> PathBuf {
    mem_dir().join("embed.state")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BusyBox ships /bin/stty as a symlink to /bin/busybox, so rejecting every
    /// symlinked candidate turned the passphrase prompt off on Alpine — which
    /// is exactly what our two musl targets are for. A symlink is fine as long
    /// as it lands in a system directory; one pointing out of them is not.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_system_tool_is_trusted_only_while_it_stays_in_place() {
        // The real thing on this machine, whatever shape it takes.
        let stty = system_tool(&["/bin/stty", "/usr/bin/stty"]);
        assert!(
            stty.is_some(),
            "no usable stty found — the passphrase prompt would refuse to run"
        );
        let stty = stty.unwrap();
        assert!(stty.is_absolute() && stty.is_file(), "{}", stty.display());
        // What comes back is the RESOLVED path, so the binary that was checked
        // is the one that gets executed.
        assert_eq!(stty, stty.canonicalize().unwrap());

        // A link out of the system directories is refused, however real its
        // target is. /tmp stands in for anywhere a user can write.
        let dir = std::env::temp_dir().join(format!("subrosa-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let planted = dir.join("stty");
        std::fs::write(&planted, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            system_tool(&[planted.to_str().unwrap()]),
            None,
            "a tool outside the system directories must not be trusted"
        );
        let link = dir.join("link-to-stty");
        std::os::unix::fs::symlink(&planted, &link).unwrap();
        assert_eq!(system_tool(&[link.to_str().unwrap()]), None);

        // Nothing there at all is simply None, not a panic.
        assert_eq!(system_tool(&["/bin/definitely-not-a-tool"]), None);
        assert_eq!(system_tool(&[]), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The distinction the whole fail-closed rule rests on: "there is nothing
    /// here" versus "there is something and it can't be read". A dangling
    /// symlink reads as plain ENOENT through `read_to_string`, which is how an
    /// evicted cloud-synced `semantic=off` used to turn itself back on.
    #[test]
    fn an_absent_file_is_not_the_same_as_an_unusable_one() {
        let dir = std::env::temp_dir().join(format!("subrosa-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing there at all: the only shape that may default to "unset".
        let missing = dir.join("nope");
        assert!(read_control_file(&missing, 64).unwrap().is_none());

        // An ordinary file comes back whole.
        let real = dir.join("config");
        std::fs::write(&real, "semantic=off\n").unwrap();
        assert_eq!(
            read_control_file(&real, 64).unwrap().as_deref(),
            Some("semantic=off\n")
        );

        // A symlink to a file that isn't there is an ERROR, not an absence.
        #[cfg(unix)]
        {
            let dangling = dir.join("dangling");
            std::os::unix::fs::symlink(dir.join("gone"), &dangling).unwrap();
            let e = read_control_file(&dangling, 64).unwrap_err();
            assert!(e.to_string().contains("leads nowhere"), "{e}");

            // A symlink to a real file is fine — that's how ~/.claude is set up.
            let linked = dir.join("linked");
            std::os::unix::fs::symlink(&real, &linked).unwrap();
            assert_eq!(
                read_control_file(&linked, 64).unwrap().as_deref(),
                Some("semantic=off\n")
            );

            // A FIFO would block a hook forever: rejected on the stat, never opened.
            let fifo = dir.join("fifo");
            if std::process::Command::new("/usr/bin/mkfifo")
                .arg(&fifo)
                .status()
                .is_ok_and(|s| s.success())
            {
                let e = read_control_file(&fifo, 64).unwrap_err();
                assert!(e.to_string().contains("not a plain file"), "{e}");
            }
        }

        // Past the cap is an error too, rather than a hook eating the memory.
        let big = dir.join("big");
        std::fs::write(&big, "x".repeat(100)).unwrap();
        let e = read_control_file(&big, 64).unwrap_err();
        assert!(e.to_string().contains("bigger than 64 bytes"), "{e}");
        assert_eq!(read_control_file(&big, 100).unwrap().unwrap().len(), 100);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
