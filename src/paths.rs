//! Data locations. Env-overridable so tests can point at a throwaway dir
//! without touching the live DB.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
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

pub fn pending_log() -> PathBuf {
    env_path("SUBROSA_PENDING_LOG", || {
        mem_dir().join("pending-checkpoint.log")
    })
}

pub fn hook_log() -> PathBuf {
    mem_dir().join("hook.log")
}

pub fn backups_dir() -> PathBuf {
    mem_dir().join("backups")
}

/// Plain KEY=VALUE config in the data dir. Tiny on purpose — no TOML dep.
pub fn config_path() -> PathBuf {
    mem_dir().join("config")
}

pub fn config_get(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    let prefix = format!("{key}=");
    text.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn config_set(key: &str, value: &str) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let prefix = format!("{key}=");
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.starts_with(&prefix) && !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    lines.push(format!("{key}={value}"));
    std::fs::write(&path, lines.join("\n") + "\n")
}

/// Where backup snapshots mirror to (a synced folder is fine for single static
/// files — only the LIVE DB must stay out of cloud sync). Env beats config;
/// the literal value "none" disables mirroring.
pub fn mirror() -> Option<PathBuf> {
    let v = std::env::var("SUBROSA_MIRROR")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| config_get("mirror"))?;
    if v.eq_ignore_ascii_case("none") {
        return None;
    }
    Some(PathBuf::from(v))
}
