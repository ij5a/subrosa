//! Encrypted mirror snapshots. The mirror copy is the one file that can leave
//! the machine (into iCloud, Dropbox, ...), so it can be sealed with a
//! passphrase: XChaCha20-Poly1305 over the whole snapshot, key from argon2id.
//!
//! File layout — a 60-byte plaintext header, then the ciphertext and its
//! 16-byte tag:
//!
//!   magic(8) | m_cost(4) | t_cost(4) | p_cost(4) | salt(16) | nonce(24)
//!
//! The counts are little-endian, and the whole header is the AAD, so editing
//! the KDF settings breaks the tag instead of quietly weakening the key.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{AeadInPlace, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::paths;

/// Last byte is the format version — bump it when the layout changes.
const MAGIC: &[u8; 8] = b"SUBROSA1";
const HEADER_LEN: usize = 60;
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

// Write-side KDF cost: 64 MiB and 3 passes, well under a second on a laptop.
const M_COST: u32 = 65536;
const T_COST: u32 = 3;
const P_COST: u32 = 1;

// The KDF settings come out of a file, so they are untrusted input: cap them
// before argon2 allocates the memory block or grinds through passes. Version 1
// only ever writes 65536/3/1, so nothing real is turned away.
const MAX_M_COST: u32 = 262_144;
const MAX_T_COST: u32 = 8;
const MAX_P_COST: u32 = 4;

fn derive_key(passphrase: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<[u8; 32], String> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| format!("bad KDF settings: {e}"))?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| format!("key derivation failed: {e}"))?;
    Ok(key)
}

/// Seal a snapshot. Fresh salt and nonce every call, so the same DB encrypted
/// twice never produces the same bytes.
///
/// ponytail: whole-buffer — the output is sized up front and the body is
/// encrypted in place inside it, so a 239 MB archive peaks around 478 MB (the
/// caller's plaintext plus this buffer). If that allocation fails the process
/// aborts, because the crate is built with panic=abort; the hook wrapper still
/// exits 0 and all that's lost is one snapshot. If archives ever reach GBs,
/// move to the AEAD STREAM construction and encrypt in chunks.
pub fn encrypt(passphrase: &str, plaintext: Vec<u8>) -> Result<Vec<u8>, String> {
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);

    let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + 16);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&M_COST.to_le_bytes());
    out.extend_from_slice(&T_COST.to_le_bytes());
    out.extend_from_slice(&P_COST.to_le_bytes());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&plaintext);
    drop(plaintext);

    let key = derive_key(passphrase, &salt, M_COST, T_COST, P_COST)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|e| e.to_string())?;
    let (header, body) = out.split_at_mut(HEADER_LEN);
    let tag = cipher
        .encrypt_in_place_detached(XNonce::from_slice(&nonce), &header[..], body)
        .map_err(|_| "encryption failed".to_string())?;
    out.extend_from_slice(&tag);
    Ok(out)
}

/// Open a sealed snapshot. Every parse step is length-checked first: this reads
/// a file that came back from a synced folder, and the process aborts on panic.
pub fn decrypt(passphrase: &str, blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < HEADER_LEN {
        return Err("file is too short to be a subrosa encrypted snapshot".into());
    }
    let header = &blob[..HEADER_LEN];
    if header[..7] != MAGIC[..7] {
        return Err("not a subrosa encrypted snapshot".into());
    }
    if header[7] != MAGIC[7] {
        return Err("made by a newer subrosa — upgrade".into());
    }
    let (m, t, p) = (le_u32(header, 8), le_u32(header, 12), le_u32(header, 16));
    if m > MAX_M_COST || t > MAX_T_COST || p > MAX_P_COST {
        return Err("unsupported KDF settings in the header".into());
    }
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&header[36..HEADER_LEN]);

    let key = derive_key(passphrase, &header[20..36], m, t, p)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|e| e.to_string())?;
    let mut body = blob[HEADER_LEN..].to_vec();

    // AEAD cannot tell a wrong key from a damaged file, so both get one error.
    // Don't try to split them — the attempt is where padding-oracle bugs start.
    cipher
        .decrypt_in_place(XNonce::from_slice(&nonce), header, &mut body)
        .map_err(|_| "wrong passphrase or corrupted file".to_string())?;
    Ok(body)
}

/// Out-of-range reads can't happen after the header length check; u32::MAX
/// keeps the function total anyway and trips the cost caps if it ever does.
fn le_u32(h: &[u8], at: usize) -> u32 {
    match h.get(at..at + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => u32::MAX,
    }
}

/// `subrosa restore <file.enc>`: decrypt a mirror snapshot into a plain .db.
pub fn restore(file: PathBuf, out: Option<PathBuf>) -> ExitCode {
    let file = paths::expanduser(&file);
    let blob = match std::fs::read(&file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[subrosa] cannot read {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    // Resolve the input before anything compares against it. A bare relative
    // name has an empty parent, and canonicalizing "" fails — which is exactly
    // how the guards below would go quietly dead on a recovery machine.
    let input_dir = match std::fs::canonicalize(&file) {
        Ok(p) => p.parent().map(PathBuf::from).unwrap_or(p),
        Err(e) => {
            eprintln!("[subrosa] cannot resolve {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    let pass = match passphrase() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[subrosa] {e}");
            return ExitCode::FAILURE;
        }
    };
    let plain = match decrypt(&pass, &blob) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[subrosa] {e}");
            return ExitCode::FAILURE;
        }
    };
    if !plain.starts_with(SQLITE_MAGIC) {
        eprintln!("[subrosa] decrypted, but the result isn't a SQLite database — wrong file?");
        return ExitCode::FAILURE;
    }

    let explicit = out.is_some();
    let out = match out {
        Some(p) => paths::expanduser(&p),
        None => {
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("subrosa-latest.db.enc");
            PathBuf::from(name.strip_suffix(".enc").unwrap_or(name))
        }
    };
    match synced_folder_risk(&input_dir, &out) {
        Ok(()) => {}
        // An explicit --out is the user's call, so it only earns a warning.
        Err(why) if explicit => eprintln!("[subrosa] warning: {why}"),
        Err(why) => {
            eprintln!("[subrosa] {why} — pass --out with a path outside it");
            return ExitCode::FAILURE;
        }
    }

    // create_new closes the race an exists() check leaves open: nothing can
    // slip a file in between the test and the write. A hard kill can still
    // leave a partial file, which is fine for a command you run by hand.
    let write = paths::create_new_600(&out).and_then(|mut f| f.write_all(&plain));
    if let Err(e) = write {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            eprintln!(
                "[subrosa] {} already exists — move it aside or pass --out",
                out.display()
            );
        } else {
            let _ = std::fs::remove_file(&out);
            eprintln!("[subrosa] cannot write {}: {e}", out.display());
        }
        return ExitCode::FAILURE;
    }

    println!(
        "[subrosa] restored {} ({} bytes)",
        out.display(),
        plain.len()
    );
    println!(
        "[subrosa] look inside it: sqlite3 'file:{}?immutable=1'",
        out.display()
    );
    println!(
        "[subrosa] to use it as the live archive, close every Claude Code session and move it \
         over {} yourself — subrosa never replaces the live DB for you.",
        paths::db_path().display()
    );
    ExitCode::SUCCESS
}

/// The decrypted file must not land where the encrypted one lives: that folder
/// is normally the synced one, and writing a readable archive back into it
/// undoes the whole point. `input_dir` is already resolved. Err carries the
/// reason, and a folder that won't resolve counts as risky — the default path
/// has to fail closed rather than wave itself through.
fn synced_folder_risk(input_dir: &Path, out: &Path) -> Result<(), String> {
    let Some(dir) = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
    else {
        return Err("cannot work out which folder that would be written to".into());
    };
    // Resolve before comparing: /tmp and /private/tmp are the same folder, and
    // a plain string match would wave that through.
    let Ok(dir) = std::fs::canonicalize(&dir) else {
        return Err(format!("cannot resolve {}", dir.display()));
    };
    if dir == input_dir {
        return Err(format!(
            "{} would put a readable copy next to the encrypted one",
            dir.display()
        ));
    }
    if near(&dir, input_dir) {
        return Err(format!(
            "{} is inside or just above the folder holding the encrypted file, \
             which is normally cloud-synced",
            dir.display()
        ));
    }
    // Risk paths, not mirror(): opting out stops subrosa writing to a folder,
    // it doesn't make it less cloud-synced. Every named folder counts, not
    // just the one that wins the write — the loser is synced too.
    let roots = paths::mirror_risk_paths();
    if roots.is_empty() {
        // Say so rather than let the caller assume this check ran. Reachable
        // when the passphrase came from the environment.
        if let Err(e) = paths::config_get("mirror") {
            eprintln!(
                "[subrosa] warning: cannot read the config ({e}) — not checking \
                 whether that folder is the mirror"
            );
        }
        return Ok(());
    }
    for m in roots {
        // A mirror folder that won't resolve (deleted leaf, unmounted volume)
        // must not switch this check off — compare against the configured path
        // instead, with whatever of it does resolve.
        let m = match std::fs::canonicalize(&m) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "[subrosa] warning: cannot resolve the mirror folder {} ({e}) — \
                     checking against the configured path instead",
                    m.display()
                );
                resolve_what_exists(&m)
            }
        };
        if dir == m || near(&dir, &m) {
            return Err(format!(
                "{} is inside or just above the mirror folder, which is normally cloud-synced",
                dir.display()
            ));
        }
    }
    Ok(())
}

/// Is `dir` close enough to `root` to be in the same synced folder?
///
/// `subrosa setup` always mirrors into `<cloud root>/subrosa`, so a `root`
/// carrying that leaf tells us where the synced root is: the cloud root and
/// everything under it — siblings of the mirror included — is synced. Any
/// other `root` is a folder we can't place, so the rule stays narrow: inside
/// it, or exactly one level up. Sweeping further than that would refuse $HOME.
fn near(dir: &Path, root: &Path) -> bool {
    if root.file_name().is_some_and(|n| n == "subrosa") {
        // A mirror straight under $HOME would make the whole home directory
        // the synced root, and then "pass --out with a path outside it" is
        // advice nobody can follow. Fall back to the narrow rule there.
        if let Some(cloud) = root.parent().filter(|c| !is_home(c)) {
            return dir.starts_with(cloud);
        }
    }
    dir.starts_with(root) || root.parent() == Some(dir)
}

fn is_home(p: &Path) -> bool {
    std::fs::canonicalize(paths::home()).is_ok_and(|h| h == p)
}

/// Resolve the part of `p` that still exists and re-attach the rest. The
/// comparison above is against a canonical path, so a plain string form of a
/// half-missing mirror would never match it — /tmp and /private/tmp again.
fn resolve_what_exists(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => resolve_what_exists(parent).join(name),
        _ => p.to_path_buf(),
    }
}

/// Env/config passphrase, else ask when there's a terminal to ask on.
fn passphrase() -> Result<String, String> {
    if let Some(p) = paths::mirror_passphrase()? {
        return Ok(p);
    }
    if !std::io::stdin().is_terminal() {
        return Err(
            "no passphrase — set SUBROSA_MIRROR_PASSPHRASE, or run this from a terminal.".into(),
        );
    }

    match prompt_hidden("passphrase: ") {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err("no passphrase given".into()),
    }
}

/// Read one line with the terminal echo off. `stty` is on every macOS and
/// Linux box, and those are all we ship to; if it won't run we still read,
/// just visibly — a setup you can't finish is worse than a visible
/// passphrase. Echo is put back on every way out, read errors included.
pub(crate) fn prompt_hidden(label: &str) -> Option<String> {
    // Echo off BEFORE the prompt goes out: turning it off afterwards leaves a
    // window where input that arrives immediately is echoed anyway.
    let hidden = stty("-echo");
    print!("[subrosa] {label}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let read = std::io::stdin().read_line(&mut s);
    if hidden {
        stty("echo");
        println!();
    }
    read.ok()?;
    Some(s.trim().to_string())
}

fn stty(arg: &str) -> bool {
    std::process::Command::new("stty")
        .arg(arg)
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &[u8] = b"SQLite format 3\0not really a database, but it round-trips";

    #[test]
    fn round_trip_and_wrong_passphrase() {
        let blob = encrypt("correct horse", PLAIN.to_vec()).unwrap();
        assert_eq!(&blob[..8], MAGIC);
        assert_eq!(blob.len(), HEADER_LEN + PLAIN.len() + 16);
        assert_eq!(decrypt("correct horse", &blob).unwrap(), PLAIN);
        assert_eq!(
            decrypt("correct hors", &blob).unwrap_err(),
            "wrong passphrase or corrupted file"
        );
    }

    /// Both the ciphertext and the header are covered by the tag, so either one
    /// changing fails the same way — no oracle telling you which.
    #[test]
    fn tampering_is_caught() {
        let blob = encrypt("pass", PLAIN.to_vec()).unwrap();
        for at in [HEADER_LEN + 1, 9, 25, 40] {
            let mut bad = blob.clone();
            bad[at] ^= 0x01;
            assert_eq!(
                decrypt("pass", &bad).unwrap_err(),
                "wrong passphrase or corrupted file",
                "byte {at} should have failed the tag check"
            );
        }
    }

    /// Header checks run before the KDF, so these cost no argon2 time.
    #[test]
    fn header_shapes_are_rejected() {
        assert_eq!(
            decrypt("pass", &[0u8; HEADER_LEN - 1]).unwrap_err(),
            "file is too short to be a subrosa encrypted snapshot"
        );
        let mut plain_db = vec![0u8; HEADER_LEN];
        plain_db[..SQLITE_MAGIC.len()].copy_from_slice(SQLITE_MAGIC);
        assert_eq!(
            decrypt("pass", &plain_db).unwrap_err(),
            "not a subrosa encrypted snapshot"
        );
        let mut future = vec![0u8; HEADER_LEN];
        future[..8].copy_from_slice(b"SUBROSA9");
        assert_eq!(
            decrypt("pass", &future).unwrap_err(),
            "made by a newer subrosa — upgrade"
        );
        let mut greedy = vec![0u8; HEADER_LEN];
        greedy[..8].copy_from_slice(MAGIC);
        greedy[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decrypt("pass", &greedy).unwrap_err(),
            "unsupported KDF settings in the header"
        );
    }

    /// One step over any cost cap is refused before argon2 runs, so a hostile
    /// header can't make us allocate. At the cap it runs and fails the tag —
    /// slow but bounded, and never a crash.
    #[test]
    fn kdf_cost_caps_hold_at_the_boundary() {
        let header = |m: u32, t: u32, p: u32| {
            let mut h = vec![0u8; HEADER_LEN];
            h[..8].copy_from_slice(MAGIC);
            h[8..12].copy_from_slice(&m.to_le_bytes());
            h[12..16].copy_from_slice(&t.to_le_bytes());
            h[16..20].copy_from_slice(&p.to_le_bytes());
            h
        };
        for (m, t, p) in [
            (MAX_M_COST + 1, MAX_T_COST, MAX_P_COST),
            (MAX_M_COST, MAX_T_COST + 1, MAX_P_COST),
            (MAX_M_COST, MAX_T_COST, MAX_P_COST + 1),
        ] {
            assert_eq!(
                decrypt("pass", &header(m, t, p)).unwrap_err(),
                "unsupported KDF settings in the header",
                "({m},{t},{p}) should have been refused"
            );
        }
        // At the cap the KDF is allowed to run; an empty body can't
        // authenticate. Each cap is exercised at its cheapest partner values.
        for (m, t, p) in [(MAX_M_COST, 1, 1), (8 * MAX_P_COST, MAX_T_COST, MAX_P_COST)] {
            assert_eq!(
                decrypt("pass", &header(m, t, p)).unwrap_err(),
                "wrong passphrase or corrupted file",
                "({m},{t},{p}) sits at the cap and should be accepted"
            );
        }
    }
}
