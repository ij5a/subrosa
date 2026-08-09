//! The embedding model, run in-process by candle: BAAI/bge-small-en-v1.5, a
//! BERT encoder pinned to one revision and fetched once into `<SUBROSA_DIR>/models`.
//! Only `subrosa embed` and `search --semantic` build one — recall, ingest and
//! every hook stay far away from it.
//!
//! ponytail: no HTTP crate. The one-time download shells out to the system
//! curl, which is the only thing in the tree that opens a socket. An HTTP
//! client would be a much larger supply chain for a fetch that happens once.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::paths;
use crate::timeutil;
use crate::wordpiece::Vocab;

/// What the model is called on screen and on disk.
pub const MODEL_NAME: &str = "bge-small-en-v1.5";

/// The pinned revision on huggingface.co/BAAI/bge-small-en-v1.5 (MIT licensed).
const REVISION: &str = "5c38ec7c405ec4b44b94cc5a9bb96e735b38267a";

/// Stored beside every vector. The revision is part of it so that vectors from
/// a different model — or a different revision of this one, or a v0.22 archive
/// that happened to use the bare name — can never be mistaken for these and
/// mixed in. Re-pinning the revision re-keys the store and forces a clean
/// backfill.
pub const MODEL_KEY: &str = "bge-small-en-v1.5@5c38ec7c";

/// Every file the model needs: name, byte size, and the sha256 a fresh download
/// is checked against before it's kept. The weights come off the network, so
/// nothing is stored that hasn't matched its hash once.
const FILES: [(&str, u64, &str); 3] = [
    (
        "model.safetensors",
        133_466_304,
        "3c9f31665447c8911517620762200d2245a2518d6e7208acc78cd9db317e21ad",
    ),
    (
        "config.json",
        743,
        "094f8e891b932f2000c92cfc663bac4c62069f5d8af5b5278c4306aef3084750",
    ),
    (
        "vocab.txt",
        231_508,
        "07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3",
    ),
];

/// bge is trained asymmetrically: a query carries this instruction and stored
/// passages go in bare. Dropping it costs real ranking quality.
pub const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// What the model was trained on, specials included. Longer input is cut.
const MAX_TOKENS: usize = 512;

/// Where the model's files live — keyed by revision, so re-pinning looks at an
/// empty folder and downloads fresh. Sharing one folder across revisions would
/// let a weights-only repin whose file sizes happen to match load the OLD
/// weights and store their vectors under the new key.
pub fn model_dir() -> PathBuf {
    paths::models_dir().join(MODEL_KEY)
}

pub struct Embedder {
    model: BertModel,
    vocab: Vocab,
}

impl Embedder {
    /// Fetch (once) and load the weights. Seconds of work and a few hundred MB
    /// of memory — build one and reuse it for the whole run. `embed` takes
    /// `&self`, so one loaded model serves every worker thread.
    pub fn load() -> Result<Embedder, String> {
        let dir = ensure_model()?;
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .map_err(|e| format!("cannot read {}: {e}", dir.join(name).display()))
        };
        let config: Config = serde_json::from_str(&read("config.json")?)
            .map_err(|e| format!("{MODEL_NAME} config.json is unreadable: {e}"))?;
        let vocab = Vocab::parse(&read("vocab.txt")?)?;
        // SAFETY: the weights are memory-mapped read-only. Another process
        // truncating the file underneath would fault — nothing else writes
        // into the model folder, and the contents were just checksummed.
        let weights = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DType::F32,
                &Device::Cpu,
            )
        }
        .map_err(|e| format!("cannot map {MODEL_NAME} weights: {e}"))?;
        let model = BertModel::load(weights, &config)
            .map_err(|e| format!("cannot load {MODEL_NAME}: {e}"))?;
        Ok(Embedder { model, vocab })
    }

    /// One L2-normalized vector per input, in order. The batch is padded to its
    /// longest sequence and the padding is masked out of attention.
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let rows: Vec<Vec<u32>> = texts
            .iter()
            .map(|t| self.vocab.encode(t, MAX_TOKENS))
            .collect();
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        if width == 0 {
            return Ok(Vec::new());
        }
        let mut ids: Vec<u32> = Vec::with_capacity(rows.len() * width);
        let mut mask: Vec<u32> = Vec::with_capacity(rows.len() * width);
        for row in &rows {
            let pad = width - row.len();
            ids.extend_from_slice(row);
            ids.extend(std::iter::repeat_n(self.vocab.pad, pad));
            mask.extend(std::iter::repeat_n(1u32, row.len()));
            mask.extend(std::iter::repeat_n(0u32, pad));
        }
        let shape = (rows.len(), width);
        let forward = || -> candle_core::Result<Vec<Vec<f32>>> {
            let ids = Tensor::from_vec(ids, shape, &Device::Cpu)?;
            let mask = Tensor::from_vec(mask, shape, &Device::Cpu)?;
            // Single-segment input, so the token types are all zero.
            let hidden = self.model.forward(&ids, &ids.zeros_like()?, Some(&mask))?;
            cls_normalized(&hidden)?.to_vec2::<f32>()
        };
        let out = forward().map_err(|e| format!("{MODEL_NAME} failed to embed: {e}"))?;
        // A non-finite value would be stored, outrank every real hit, and be
        // indistinguishable from a good vector afterwards.
        if out.iter().flatten().any(|x| !x.is_finite()) {
            return Err(format!("{MODEL_NAME} returned an unusable vector"));
        }
        Ok(out)
    }
}

/// bge pools the `[CLS]` token — the first position of the last hidden state —
/// and then normalizes. Mean-pooling instead loads fine, returns the right
/// shape, and silently ranks worse.
fn cls_normalized(hidden: &Tensor) -> candle_core::Result<Tensor> {
    let cls = hidden.narrow(1, 0, 1)?.squeeze(1)?;
    cls.broadcast_div(&cls.sqr()?.sum_keepdim(1)?.sqrt()?)
}

/// Cosine similarity of two same-length, L2-normalized vectors — a plain dot
/// product. Accumulated in f64: an f32 accumulator overflows to infinity on a
/// corrupt vector with huge components, and infinity outranks every real score.
/// Callers check the dimensions and that the result is in range; nothing relies
/// on this to catch either.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum()
}

/// L2-normalize in place so ranking is a dot product. A zero vector stays zero.
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Is the model already on disk, whole? Decides whether `subrosa embed` can do
/// anything at all without the network — which is what the advice a search
/// gives out has to be built on.
pub fn model_ready() -> bool {
    missing_files(&model_dir()).is_empty()
}

/// The files that aren't there at their pinned size yet.
fn missing_files(dir: &Path) -> Vec<&'static (&'static str, u64, &'static str)> {
    FILES
        .iter()
        .filter(|(name, size, _)| !sized(&dir.join(name), *size))
        .collect()
}

/// All three files present at their pinned size, downloading whatever isn't.
/// Returns the folder they live in.
fn ensure_model() -> Result<PathBuf, String> {
    let dir = model_dir();
    if missing_files(&dir).is_empty() {
        return Ok(dir);
    }
    // The one switch, one meaning: off means no subrosa process opens a socket,
    // whoever asked. A model already on disk still loads above this line, so an
    // index built before it was switched off keeps working.
    if auto_off() {
        return Err(format!(
            "semantic indexing is switched off, so the {MODEL_NAME} model is not downloaded — \
             remove `semantic=off` from {} (or set `semantic=on`) to turn it back on",
            paths::config_path().display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // Only the slow path locks. Waiting behind a peer's download is the point,
    // so the leash is long.
    let _lock = download_lock()?;
    // A peer may have finished the whole thing while we waited.
    let missing = missing_files(&dir);
    if missing.is_empty() {
        return Ok(dir);
    }
    // Progress goes to stderr: stdout carries search results.
    eprintln!("[subrosa] downloading {MODEL_NAME} (~133 MB, one time)");
    for (name, size, sha) in missing {
        fetch(&dir, name, *size, sha).map_err(|e| format!("{e}\n{}", manual_hint(&dir)))?;
    }
    Ok(dir)
}

/// The cross-process lock over the whole download: an open write transaction on
/// a SQLite file next to the model folder. Holding the connection IS the lock,
/// and dropping it releases — including on a crash, since the OS closes the
/// file. No new dependency, no stale lock file to clean up.
fn download_lock() -> Result<Connection, String> {
    let path = paths::models_dir().join(".download.lock");
    let fail = |e: rusqlite::Error| format!("cannot lock {}: {e}", path.display());
    let conn = Connection::open(&path).map_err(fail)?;
    conn.busy_timeout(Duration::from_secs(30 * 60))
        .map_err(fail)?;
    conn.execute_batch("BEGIN IMMEDIATE").map_err(fail)?;
    Ok(conn)
}

//--- the automatic indexer ---------------------------------------------------

/// Where the run lock for a given database lives: beside the database itself,
/// under its canonical path. Keying it to `SUBROSA_DIR` instead would let two
/// data dirs pointed at one `SUBROSA_DB` take different locks and backfill the
/// same archive at once. The path is canonicalized (the parent, when the file
/// isn't there yet) so two spellings of one database still meet on one lock.
fn lock_path(db: &Path) -> PathBuf {
    let real = db.canonicalize().unwrap_or_else(|_| {
        match (db.parent(), db.file_name()) {
            (Some(dir), Some(name)) => dir.canonicalize().map(|d| d.join(name)),
            _ => Err(std::io::Error::other("no parent")),
        }
        .unwrap_or_else(|_| db.to_path_buf())
    });
    let mut name = real.file_name().unwrap_or_default().to_os_string();
    name.push(".embed-lock");
    real.with_file_name(name)
}

/// The lock one indexing run holds, so two never embed the same turns at once.
/// `Ok(None)` means another run has it. Same open-write-transaction trick as
/// `download_lock`: holding the connection IS the lock, and a killed run
/// releases it because the OS closes the file.
pub fn run_lock(wait: Duration) -> Result<Option<Connection>, String> {
    let db = paths::db_path();
    if let Some(dir) = db.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let path = lock_path(&db);
    let fail = |e: rusqlite::Error| format!("cannot lock {}: {e}", path.display());
    let conn = Connection::open(&path).map_err(fail)?;
    conn.busy_timeout(wait).map_err(fail)?;
    match conn.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => Ok(Some(conn)),
        Err(e) if held_by_someone_else(&e) => Ok(None),
        Err(e) => Err(fail(e)),
    }
}

fn held_by_someone_else(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Automatic indexing switched off — a config we can't read counts as off, so
/// nothing downloads on a maybe. Use this wherever the answer is only yes/no;
/// `paths::semantic_mode()` keeps the reason for the places that report it.
pub fn auto_off() -> bool {
    !matches!(paths::semantic_mode().as_deref(), Ok(m) if m != "off")
}

/// What the last automatic run left behind. `running` is written before the
/// expensive work starts, so a run that is killed or runs out of memory still
/// leaves something for the next hook to back off from; `failed` carries the
/// error the run actually hit.
pub struct Retry {
    pub failures: u32,
    /// The epoch second the next automatic attempt may start.
    pub at: i64,
    pub running: bool,
    pub last_error: String,
}

/// How long a failed run waits before the next automatic attempt: an hour,
/// doubling to a day. Typing `subrosa embed` ignores this and tries now.
fn backoff_secs(failures: u32) -> i64 {
    3600i64
        .saturating_mul(1i64 << failures.saturating_sub(1).min(20))
        .min(86_400)
}

/// The state file, read the way a hook has to read anything: only a regular
/// file, never through a symlink, and never more than a few KB. A FIFO or a
/// device node would block the hook forever and a huge file would eat its
/// memory — both are worse than having no state at all.
///
/// ponytail: the stat and the open are two steps, so this trusts that nothing
/// swaps the file in between. Anything that could is already inside the 0700
/// data dir, where it can write the state file directly anyway.
fn read_state() -> Option<String> {
    match paths::read_control_file(&paths::embed_state_path(), 4096) {
        Ok(text) => text,
        // Unusable reads as healthy on purpose: the worst it costs is one
        // wasted attempt, and that run replaces the file.
        Err(e) => {
            crate::hook::log(&format!("embed: ignoring the indexer state — {e}"));
            None
        }
    }
}

/// The recorded state, or `None` when the last run finished cleanly. A file we
/// can't read or parse reads as healthy: the worst that costs is one wasted
/// attempt, and the run that follows replaces the file.
pub fn retry_state() -> Option<Retry> {
    let text = read_state()?;
    let failures: u32 = paths::kv_get(&text, "failures")?.parse().ok()?;
    let last: i64 = paths::kv_get(&text, "last_attempt")?.parse().ok()?;
    Some(Retry {
        failures,
        at: last.saturating_add(backoff_secs(failures)),
        running: paths::kv_get(&text, "state").as_deref() == Some("running"),
        last_error: paths::kv_get(&text, "last_error").unwrap_or_default(),
    })
}

/// True while the last run is still inside its retry window — whether it failed
/// or was killed part way. Either way, another one now would just repeat it.
pub fn retry_pending() -> bool {
    retry_state().is_some_and(|r| timeutil::now_unix() <= r.at)
}

/// The last run that did NOT finish, for the lines that explain why the index
/// is standing still. A `running` marker whose window has passed is a run that
/// died without saying so, so it reports as an unfinished one rather than
/// leaving the dashboard claiming progress that isn't happening.
pub fn last_failure() -> Option<Retry> {
    let r = retry_state()?;
    match r.running && timeutil::now_unix() <= r.at {
        true => None,
        false => Some(r),
    }
}

/// Note that a run is starting. Cleared on success, replaced by the real error
/// on a clean failure — and left exactly as it is by a kill or an OOM, which is
/// the whole point. Returns the attempt count so the failure that follows
/// doesn't count the same run twice.
pub fn record_attempt() -> u32 {
    let failures = retry_state().map_or(0, |r| r.failures).saturating_add(1);
    write_state(failures, true, "stopped before it finished");
    failures
}

/// Remember why an automatic run failed, so the next ones back off instead of
/// re-running something that can't work.
pub fn record_failure(failures: u32, err: &str) {
    write_state(failures, false, &one_line(err));
}

/// Failing to write this is worth a line: on a full disk the backoff itself is
/// what can't be written, so nothing would ever back off and every session
/// would retry — the one thing the backoff exists to stop.
fn write_state(failures: u32, running: bool, err: &str) {
    let state = if running { "running" } else { "failed" };
    let path = paths::embed_state_path();
    if let Err(e) = paths::write_control_file(
        &path,
        &format!(
            "last_attempt={}\nfailures={failures}\nstate={state}\nlast_error={err}\n",
            timeutil::now_unix()
        ),
    ) {
        crate::hook::log(&format!(
            "embed: cannot write {} — the next session will try again instead of backing off: {e}",
            path.display()
        ));
    }
}

/// The cause, in one line. A model error can run to several — the first is what
/// went wrong and the rest is the manual-download instructions, which belong in
/// front of whoever typed the command, not in a log line or on the dashboard.
/// The state file is KEY=VALUE and the hook log is one entry per line, so both
/// need the short version anyway.
pub fn one_line(err: &str) -> String {
    let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let short: String = first.trim().chars().take(120).collect();
    match first.trim().chars().count() > 120 {
        true => format!("{short}…"),
        false => short,
    }
}

/// A run that finished is the healthy state, and healthy is no file at all.
/// Every completed backfill clears it, typed or automatic — a hand-run
/// `subrosa embed` that fixes the problem must not leave the hooks backing off.
pub fn clear_failures() {
    let path = paths::embed_state_path();
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        // Nothing to clear is the normal case, every run after the first.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // Left behind, this reads as "waiting to retry" on a healthy install.
        Err(e) => crate::hook::log(&format!("embed: cannot clear {} — {e}", path.display())),
    }
}

/// The system `nice`, by absolute path. This runs on every session start and
/// end, and a bare name there is a PATH lookup — someone else's binary waiting
/// to be found. Not being at this path just means the run isn't niced.
const NICE: &str = "/usr/bin/nice";

/// Start the background indexing run and return — nothing waits on it. Skipped
/// when semantic is off, or while the last run is inside its retry window; both
/// are file reads, so a hook can call this without touching the database. What
/// is left to index is the worker's own decision.
pub fn spawn_if_due() {
    match paths::semantic_mode() {
        Ok(mode) if mode == "off" => return,
        // Fail closed, and say why once: a config we can't read might say off.
        Err(e) => return crate::hook::log(&format!("embed: not indexing — {e}")),
        Ok(_) => {}
    }
    if retry_pending() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return crate::hook::log(&format!("embed: cannot find my own binary — {e}")),
    };
    let (exe, embed, auto) = (exe.as_os_str(), OsStr::new("embed"), OsStr::new("--auto"));
    let plain = || detach(&[exe, embed, auto]);
    // Niced when nice is where it always is; at normal priority when it isn't.
    // Known limit: a successful spawn of nice tells us nothing about nice's own
    // exec of our binary, so a stale exe path under a niced spawn is lost until
    // the next session. Catching it needs a pipe and a handshake — not worth it
    // for a case that self-heals.
    let started = match Path::new(NICE).is_file() {
        true => detach(&[
            OsStr::new(NICE),
            OsStr::new("-n"),
            OsStr::new("10"),
            exe,
            embed,
            auto,
        ])
        .or_else(|_| plain()),
        false => plain(),
    };
    // Nothing else records this: the state file is written by the child, and
    // there is no child. Without a line here the dashboard would go on saying
    // the index is building.
    if let Err(e) = started {
        crate::hook::log(&format!(
            "embed: could not start the background indexer — {e}"
        ));
    }
}

/// Spawn `argv` into a process group of its own, with no stdio and no wait.
/// The new group is the whole point: a session ending signals the group its
/// hooks ran in, and a child outside that group survives it, reparents to init
/// and finishes the index. Claude Code caps a hook at 120s, so the work cannot
/// live inside the hook.
fn detach(argv: &[&OsStr]) -> std::io::Result<()> {
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().map(|_| ())
}

/// Where a file stands before curl runs.
#[derive(Debug, PartialEq)]
enum Staged {
    /// Another process finished this file while we were looking.
    Won,
    /// A finished leftover: worth a checksum instead of a re-download.
    Complete(PathBuf),
    /// Our own staging path, holding a short leftover to resume or nothing yet.
    Partial(PathBuf),
}

/// Only this process ever writes this file, so a checksum on it can't be
/// invalidated by someone else between the check and the rename.
fn staging_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.part.{}", std::process::id()))
}

/// A staging file for `name`: the plain `<name>.part` a hand-download leaves, or
/// the `<name>.part.<pid>` a run of ours writes. Anything else that starts the
/// same way (`vocab.txt.part.bak`) belongs to someone else and is left alone.
fn is_staging(file: &str, name: &str) -> bool {
    match file.strip_prefix(&format!("{name}.part")) {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix('.')
            .is_some_and(|pid| !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit())),
        None => false,
    }
}

/// Sort out what earlier runs left behind: the biggest usable leftover is kept
/// (a complete one to check, a short one adopted under our own name so curl can
/// resume it) and the rest are deleted. Our own `.part.<pid>` counts too — a
/// crash plus a recycled pid can leave a full-length one, which curl would try
/// to resume and get a 416 for.
///
/// Runs under `download_lock`, so nothing else is writing these files while we
/// sort them out. ponytail: the lock only binds versions that take it, so a
/// mixed old/new-binary fleet can still race — everything renamed into place is
/// checksummed either way.
fn stage(dir: &Path, name: &str, size: u64) -> Staged {
    if sized(&dir.join(name), size) {
        return Staged::Won;
    }
    let ours = staging_path(dir, name);
    let mut leftovers: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| is_staging(&e.file_name().to_string_lossy(), name))
        .map(|e| (e.metadata().map(|m| m.len()).unwrap_or(0), e.path()))
        .collect();
    // Biggest first: a complete leftover beats a short one, and anything longer
    // than the pinned size is not this file at all.
    leftovers.sort_by_key(|(len, _)| std::cmp::Reverse(*len));
    let mut keep = None;
    for (len, path) in leftovers {
        if keep.is_none() && len <= size {
            keep = Some((len, path));
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    match keep {
        Some((len, path)) if len == size => Staged::Complete(path),
        // Adopt it so only we write to it from here — unless it's already ours.
        Some((_, path)) => {
            if path != ours && std::fs::rename(&path, &ours).is_err() {
                let _ = std::fs::remove_file(&path);
            }
            Staged::Partial(ours)
        }
        None => Staged::Partial(ours),
    }
}

/// One file through the system curl, checksummed before it's renamed into place
/// — a download that doesn't match its pin never reaches the folder we load
/// from.
fn fetch(dir: &Path, name: &str, size: u64, sha: &str) -> Result<(), String> {
    let final_path = dir.join(name);
    let part = match stage(dir, name, size) {
        Staged::Won => return Ok(()),
        Staged::Complete(path) => {
            if verified(&path, sha) {
                return promote(&path, &final_path, name);
            }
            let _ = std::fs::remove_file(&path);
            staging_path(dir, name)
        }
        Staged::Partial(path) => path,
    };
    let url = url(name);
    let at = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    // Opened before curl is spawned: a sink we can't even open must not leave a
    // download running with nowhere to put it.
    let mut sink = staging_sink(&part).map_err(|e| write_failed(&part, e))?;
    // curl writes to a pipe and WE write the file. The lock keeps other subrosas
    // out, but a killed parent can't stop its own curl child, and that child
    // would go on writing into a file a later run may adopt and promote. On the
    // pipe an orphan dies of SIGPIPE at its next write instead. Structural, so
    // there's no kill test — a flaky one wouldn't prove more than this comment.
    let mut child = Command::new(curl_bin()?)
        .args(curl_args(at, &url))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    let status = match drain(&mut child, &mut sink, size.saturating_sub(at)) {
        Ok(s) => s,
        // An overrun means the bytes on disk aren't this file, so they go; a
        // write error leaves them, since freeing space and re-running resumes.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            let _ = std::fs::remove_file(&part);
            return Err(format!("{url}: {e}"));
        }
        Err(e) => return Err(write_failed(&part, e)),
    };
    if !status.success() {
        return Err(format!(
            "curl could not download {url} ({status}) — run the command again \
             to pick up from where it stopped"
        ));
    }
    if !verified(&part, sha) {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "{name} does not match its pinned checksum — refusing to load it"
        ));
    }
    promote(&part, &final_path, name)
}

/// The curl that fetches the model, by absolute path. This is the one process
/// in the tree that opens a socket, so it is never looked up through PATH —
/// that PATH belongs to whichever session's hook started the run. `SUBROSA_CURL`
/// names it for the systems where it lives somewhere else (Nix, a slim
/// container); that is a deliberate setting, not an ambient one.
fn curl_bin() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os("SUBROSA_CURL").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(named);
        // Absolute only, and the CANONICAL path is what runs. A relative
        // `./curl` would pass a file check here and then go back through a PATH
        // lookup at spawn time — the binary checked would not be the binary
        // executed.
        if !p.is_absolute() {
            return Err(format!(
                "cannot run curl: SUBROSA_CURL={} has to be an absolute path",
                p.display()
            ));
        }
        let real = p.canonicalize().map_err(|e| {
            format!(
                "cannot run curl: SUBROSA_CURL={} cannot be resolved: {e}",
                p.display()
            )
        })?;
        return match real.is_file() {
            true => Ok(real),
            false => Err(format!(
                "cannot run curl: SUBROSA_CURL={} is not a file",
                p.display()
            )),
        };
    }
    paths::system_tool(&["/usr/bin/curl", "/bin/curl"])
        .ok_or_else(|| "cannot run curl: there is none at /usr/bin/curl or /bin/curl".to_string())
}

/// The argv for one download, resuming at byte `at`.
///
/// `-q` has to come FIRST, or curl reads the user's curlrc before our flags.
/// An `output=` in one wins over our later `--output -` — curl binds the output
/// per URL and only warns on stderr — which takes the body off the pipe
/// entirely, past the size cap and past the parent that is meant to be the only
/// writer. Everything we don't re-specify (a proxy, `--cacert`, cookies) rides
/// along the same way unless the rc file is disabled. `--output -` and
/// `--retry 0` are belt and braces on top. The rest: `-#` for a progress bar (a
/// download in silence reads as hung), the connect and minimum-speed watchdogs
/// so a stalling server can't sit on the download lock, and `-C` to pick up our
/// staging file where it stopped.
fn curl_args(at: u64, url: &str) -> Vec<String> {
    [
        "-q",
        "-fL",
        "--output",
        "-",
        "--retry",
        "0",
        "-#",
        "--connect-timeout",
        "30",
        "--speed-limit",
        "1024",
        "--speed-time",
        "60",
        "-A",
        "subrosa",
        "-C",
    ]
    .iter()
    .map(|a| a.to_string())
    .chain([at.to_string(), url.to_string()])
    .collect()
}

fn write_failed(part: &Path, e: std::io::Error) -> String {
    format!("cannot write {}: {e}", part.display())
}

/// The staging sink: created on a fresh download, continued on a resume.
fn staging_sink(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Copy at most `budget` bytes of the child's output into `sink`, then reap it.
///
/// The read end is closed BEFORE the wait, and first of all on an error (a full
/// disk, part way in). Curl blocks inside `write()` once the pipe fills and only
/// learns to stop when the reader goes away, so waiting on it while still
/// holding the pipe open never returns — with the download lock held.
///
/// One byte past the budget is read on purpose: the file's size is pinned, so
/// anything beyond it is a server we can't believe, and it would otherwise be
/// free to fill the disk until the checksum at stream end finally said no.
fn drain(
    child: &mut std::process::Child,
    sink: &mut impl std::io::Write,
    budget: u64,
) -> std::io::Result<ExitStatus> {
    use std::io::Read as _;
    let body = child.stdout.take().expect("curl stdout was piped");
    let mut capped = body.take(budget.saturating_add(1));
    let copied = std::io::copy(&mut capped, sink);
    drop(capped);
    let overrun = matches!(copied, Ok(n) if n > budget);
    if copied.is_err() || overrun {
        let _ = child.kill();
        let _ = child.wait();
        return Err(match copied {
            Err(e) => e,
            _ => std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the server sent more than the file's pinned size — refusing to read on",
            ),
        });
    }
    child.wait()
}

fn promote(from: &Path, to: &Path, name: &str) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("cannot save {name}: {e}"))
}

fn url(name: &str) -> String {
    format!("https://huggingface.co/BAAI/{MODEL_NAME}/resolve/{REVISION}/{name}")
}

/// The way out when curl is missing or the download can't be made to work. The
/// `.part` names matter: a file saved under one is checksummed and renamed into
/// place on the next run, while one dropped straight at its final name is only
/// ever size-checked.
fn manual_hint(dir: &Path) -> String {
    let urls: Vec<String> = FILES
        .iter()
        .map(|(name, _, _)| format!("  {name}.part  <-  {}", url(name)))
        .collect();
    format!(
        "[subrosa] download these into {}, saving each under the .part name shown, \
         then run the command again:\n{}",
        dir.display(),
        urls.join("\n")
    )
}

/// Is this file there at the size we pinned? Anything else — missing, short,
/// half-written — reads as "fetch it again".
/// ponytail: a stat, not a hash. Re-reading the weights before every search cost
/// far more than it bought, and the checksum was verified on download. What
/// this gives up: a file that rots in place at exactly the right size fails
/// loudly only if the corruption touches the safetensors header — damage to the
/// weights alone loads fine and quietly returns worse vectors. Anything dropped
/// straight at a final path is trusted on size alone, which is why the manual
/// instructions ask for `.part` names — those get hashed. Recovery either way is
/// to delete the model folder and re-run `subrosa embed`.
fn sized(path: &Path, want: u64) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == want)
}

/// Does this freshly downloaded file hash to what we pinned?
fn verified(path: &Path, want: &str) -> bool {
    sha256(path).is_ok_and(|got| got == want)
}

fn sha256(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut buf)? {
            0 => break,
            n => hasher.update(&buf[..n]),
        }
    }
    Ok(hasher.finalize().iter().fold(String::new(), |mut s, b| {
        s.push_str(&format!("{b:02x}"));
        s
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// candle 0.10+ depends on `tokenizers`, which builds the C oniguruma
    /// library. A version bump that drags it back in fails here, not in
    /// someone's musl build.
    #[test]
    fn the_lockfile_stays_free_of_c_tokenizers() {
        let lock = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"));
        for crate_name in ["tokenizers", "onig_sys"] {
            assert!(
                !lock.contains(&format!("name = \"{crate_name}\"")),
                "{crate_name} is back in Cargo.lock — candle must stay on 0.9"
            );
        }
    }

    /// The pinned `config.json`, byte for byte. candle's `Config` has no
    /// defaults for most fields, so a model whose config drifts fails to load
    /// at runtime — this catches that here instead.
    #[test]
    fn the_pinned_config_deserializes_into_candles_bert() {
        let config: Config = serde_json::from_str(
            r#"{
  "_name_or_path": "/root/.cache/torch/sentence_transformers/BAAI_bge-small-en/",
  "architectures": ["BertModel"],
  "attention_probs_dropout_prob": 0.1,
  "classifier_dropout": null,
  "hidden_act": "gelu",
  "hidden_dropout_prob": 0.1,
  "hidden_size": 384,
  "id2label": {"0": "LABEL_0"},
  "initializer_range": 0.02,
  "intermediate_size": 1536,
  "label2id": {"LABEL_0": 0},
  "layer_norm_eps": 1e-12,
  "max_position_embeddings": 512,
  "model_type": "bert",
  "num_attention_heads": 12,
  "num_hidden_layers": 12,
  "pad_token_id": 0,
  "position_embedding_type": "absolute",
  "torch_dtype": "float32",
  "transformers_version": "4.30.0",
  "type_vocab_size": 2,
  "use_cache": true,
  "vocab_size": 30522
}"#,
        )
        .expect("bge-small-en-v1.5 config.json must load as a candle bert Config");
        assert_eq!(config.hidden_size, 384);
        assert_eq!(config.num_hidden_layers, 12);
        assert_eq!(config.max_position_embeddings, MAX_TOKENS);
    }

    /// The storage key is hand-typed. A revision bump that forgets to re-type
    /// it would key new vectors as the old ones and mix two vector spaces —
    /// and, since the model folder is named by the key, load the old weights.
    #[test]
    fn the_storage_key_follows_the_pinned_revision() {
        assert_eq!(MODEL_KEY, format!("{MODEL_NAME}@{}", &REVISION[..8]));
        assert!(model_dir().ends_with(MODEL_KEY));
    }

    /// One database means one lock, however the process was pointed at it.
    /// `SUBROSA_DB` can aim two different data dirs at the same archive, and a
    /// lock keyed to the data dir would let both backfill it at once.
    #[test]
    fn one_database_is_one_lock_whatever_path_reaches_it() {
        let dir = std::env::temp_dir().join(format!("subrosa-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        let db = dir.join("memory.db");
        std::fs::write(&db, b"").unwrap();

        // The same file, spelled three ways — a plain path, a dot segment, and
        // a walk back out of a subdirectory.
        let dotted = dir.join(".").join("memory.db");
        let around = dir.join("nested").join("..").join("memory.db");
        assert_eq!(lock_path(&db), lock_path(&dotted));
        assert_eq!(lock_path(&db), lock_path(&around));
        assert!(lock_path(&db).ends_with("memory.db.embed-lock"));

        // A database that doesn't exist yet still resolves through its parent,
        // which is the state of a first run.
        let fresh = dir.join("other.db");
        assert_eq!(
            lock_path(&fresh),
            lock_path(&dir.join("nested").join("..").join("other.db"))
        );
        // And two databases never share one lock.
        assert_ne!(lock_path(&db), lock_path(&fresh));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The automatic indexer's retry schedule. A machine that stays offline
    /// must slow down instead of re-trying the same download every session,
    /// and the doubling must not overflow however long that goes on.
    #[test]
    fn the_retry_window_doubles_up_to_a_day() {
        assert_eq!(backoff_secs(1), 3_600);
        assert_eq!(backoff_secs(2), 7_200);
        assert_eq!(backoff_secs(3), 14_400);
        assert_eq!(backoff_secs(5), 57_600);
        assert_eq!(backoff_secs(6), 86_400, "capped at a day");
        assert_eq!(backoff_secs(u32::MAX), 86_400);
        // No failure recorded still waits the first window rather than 0s.
        assert_eq!(backoff_secs(0), 3_600);
    }

    #[test]
    fn cosine_scores_normalized_vectors() {
        let a = [1.0, 0.0, 0.0];
        assert_eq!(cosine(&a, &a), 1.0);
        assert_eq!(cosine(&a, &[0.0, 1.0, 0.0]), 0.0);
        assert_eq!(cosine(&a, &[-1.0, 0.0, 0.0]), -1.0);
        // Different dimensions score over the shared prefix, never panic.
        assert_eq!(cosine(&a, &[1.0, 0.0]), 1.0);
    }

    #[test]
    fn normalize_makes_a_unit_vector() {
        let mut v = [3.0, 4.0];
        normalize(&mut v);
        assert_eq!(v, [0.6, 0.8]);
        let mut zero = [0.0, 0.0];
        normalize(&mut zero);
        assert_eq!(zero, [0.0, 0.0]);
    }

    /// The [CLS] row is taken and normalized — not the mean over the sequence.
    #[test]
    fn pooling_takes_the_first_token_and_normalizes_it() {
        // One batch, two positions, two features: [CLS] = [3, 4], next = [9, 9].
        let hidden =
            Tensor::from_vec(vec![3.0f32, 4.0, 9.0, 9.0], (1, 2, 2), &Device::Cpu).unwrap();
        let out = cls_normalized(&hidden).unwrap().to_vec2::<f32>().unwrap();
        assert_eq!(out, vec![vec![0.6, 0.8]]);
    }

    #[test]
    fn sha256_matches_the_file_and_only_that_file() {
        let path = std::env::temp_dir().join(format!("subrosa-sha-{}", std::process::id()));
        std::fs::write(&path, b"subrosa").unwrap();
        // $ printf subrosa | shasum -a 256
        let want = "58a15ceba21a3a3c38152b63f99f9705183e30571b1080a4ee5dbe06903e2187";
        assert_eq!(sha256(&path).unwrap(), want);
        assert!(verified(&path, want));
        assert!(
            !verified(&path, &want.replace('5', "6")),
            "a wrong checksum must not pass"
        );
        std::fs::remove_file(&path).unwrap();
        assert!(!verified(&path, want), "a missing file must not pass");
    }

    /// The steady-state check every run pays: a stat against the pinned size.
    /// A file of the wrong size is treated as missing, so it gets fetched again.
    #[test]
    fn a_file_only_counts_when_it_is_the_pinned_size() {
        let dir = std::env::temp_dir().join(format!("subrosa-sized-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vocab.txt");
        assert!(!sized(&path, 7), "a missing file must not pass");
        std::fs::write(&path, b"subros").unwrap();
        assert!(!sized(&path, 7), "a short file must not pass");
        std::fs::write(&path, b"subrosa").unwrap();
        assert!(sized(&path, 7));
        // A folder of the right name is not the file.
        assert!(!sized(&dir, 7));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Only our own staging shapes are adopted; anything else keeping the
    /// prefix belongs to something we didn't write.
    #[test]
    fn only_part_and_part_pid_count_as_staging() {
        for good in ["vocab.txt.part", "vocab.txt.part.1", "vocab.txt.part.98765"] {
            assert!(is_staging(good, "vocab.txt"), "{good}");
        }
        for bad in [
            "vocab.txt",
            "vocab.txt.part.bak",
            "vocab.txt.part.",
            "vocab.txt.parts",
            "vocab.txt.part.12a",
            "config.json.part.1",
        ] {
            assert!(!is_staging(bad, "vocab.txt"), "{bad}");
        }
    }

    /// A throwaway model folder for the staging tests.
    fn staging_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("subrosa-stage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The download lands in the staging file by way of the parent, so a resume
    /// has to continue the file rather than start it over.
    #[test]
    fn downloaded_bytes_are_appended_not_overwritten() {
        let dir = staging_dir("append");
        let part = dir.join("vocab.txt.part");
        let copy = |bytes: &[u8]| {
            let mut sink = staging_sink(&part).unwrap();
            std::io::copy(&mut std::io::Cursor::new(bytes.to_vec()), &mut sink).unwrap()
        };
        assert_eq!(
            (copy(b"abc"), std::fs::read(&part).unwrap()),
            (3, b"abc".to_vec())
        );
        assert_eq!(
            (copy(b"def"), std::fs::read(&part).unwrap()),
            (3, b"abcdef".to_vec())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A sink that runs out of room part way through, the way a full disk does.
    struct Fails(usize);

    impl std::io::Write for Fails {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("no space left on device"));
            }
            let n = buf.len().min(self.0);
            self.0 -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The child fills the pipe and blocks in `write()`; if the parent waits on
    /// it while still holding the read end, neither ever moves — and this one
    /// holds the download lock. The timeout is the assertion: a regression
    /// fails the test instead of hanging the suite.
    #[test]
    fn a_sink_that_fails_does_not_wedge_the_child() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut child = endless();
            // The largest budget there is: the sink, not the cap, has to stop it.
            let err = drain(&mut child, &mut Fails(64 * 1024), u64::MAX).unwrap_err();
            let _ = tx.send(err.to_string());
        });
        let err = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("drain wedged on a full pipe");
        assert!(err.contains("no space left on device"), "{err}");
    }

    /// A producer that never stops, standing in for a server that keeps
    /// streaming — a real one is caught by curl's minimum-speed watchdog only
    /// if it also goes slow.
    fn endless() -> std::process::Child {
        Command::new("sh")
            .args(["-c", "cat /dev/zero"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Everything here defends the pipe. A curlrc in the user's home is read
    /// before our own flags unless `-q` leads, and an `--output` in one would
    /// take the body off the pipe entirely.
    #[test]
    fn curl_is_told_to_ignore_the_users_curlrc() {
        let args = curl_args(4096, "https://example.invalid/x");
        assert_eq!(args[0], "-q", "-q must lead the argv: {args:?}");
        for [flag, value] in [["--output", "-"], ["--retry", "0"], ["-C", "4096"]] {
            assert!(
                args.windows(2).any(|w| w[0] == flag && w[1] == value),
                "{flag} {value} missing from {args:?}"
            );
        }
        assert_eq!(args.last().unwrap(), "https://example.invalid/x");
    }

    /// The ordinary case: a producer that stops on its own, inside the budget.
    #[test]
    fn a_download_that_fits_the_budget_is_kept() {
        let dir = staging_dir("exact");
        let part = dir.join("vocab.txt.part");
        let budget = 4096u64;
        let (tx, rx) = std::sync::mpsc::channel();
        let sink_path = part.clone();
        std::thread::spawn(move || {
            let mut child = Command::new("sh")
                .args(["-c", &format!("head -c {budget} /dev/zero")])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut sink = staging_sink(&sink_path).unwrap();
            let _ = tx.send(drain(&mut child, &mut sink, budget).map(|s| s.success()));
        });
        let status = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("drain did not finish a bounded download");
        assert!(status.unwrap(), "the child exited non-zero");
        assert_eq!(std::fs::metadata(&part).unwrap().len(), budget);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The file's size is pinned, so a server that keeps sending past it is
    /// stopped there rather than at the checksum — otherwise it can fill the
    /// disk while holding the download lock.
    #[test]
    fn a_server_that_overruns_the_pinned_size_is_cut_off() {
        let dir = staging_dir("overrun");
        let part = dir.join("vocab.txt.part");
        let budget = 8 * 1024u64;
        let (tx, rx) = std::sync::mpsc::channel();
        let sink_path = part.clone();
        std::thread::spawn(move || {
            let mut child = endless();
            let mut sink = staging_sink(&sink_path).unwrap();
            let err = drain(&mut child, &mut sink, budget).unwrap_err();
            let _ = tx.send((err.kind(), err.to_string()));
        });
        let (kind, err) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("drain read on past the pinned size");
        assert_eq!(kind, std::io::ErrorKind::InvalidData, "{err}");
        assert!(err.contains("more than the file's pinned size"), "{err}");
        // At most one sentinel byte past the budget ever reaches the disk.
        let written = std::fs::metadata(&part).unwrap().len();
        assert!(
            written <= budget + 1,
            "wrote {written} for a {budget} budget"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A staging file another process would have written — never our own pid.
    fn foreign(dir: &Path, nth: u32, body: &[u8]) -> PathBuf {
        let path = dir.join(format!("vocab.txt.part.{}", std::process::id() + nth));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// The download is serialized by `download_lock`, so these decisions only
    /// ever meet files left behind by runs that already finished or died.
    /// ponytail: the decisions are tested, a real two-process race isn't — that
    /// test is flaky, and what keeps the race safe is the lock plus the rule
    /// that only a checksummed staging file is ever renamed into place.
    #[test]
    fn staging_promotes_a_complete_leftover_and_drops_the_rest() {
        let dir = staging_dir("leftovers");
        // Another process finished first: nothing left to do.
        std::fs::write(dir.join("vocab.txt"), b"1234").unwrap();
        assert_eq!(stage(&dir, "vocab.txt", 4), Staged::Won);
        std::fs::remove_file(dir.join("vocab.txt")).unwrap();

        // A complete leftover is handed back for a checksum, not re-downloaded.
        let theirs = foreign(&dir, 1, b"1234");
        // A shorter one, an oversized one and a file that only looks like
        // staging: the first two are swept, the last is none of our business.
        let short = foreign(&dir, 2, b"12");
        let long = foreign(&dir, 3, b"123456");
        let alien = dir.join("vocab.txt.part.bak");
        std::fs::write(&alien, b"12").unwrap();
        assert_eq!(stage(&dir, "vocab.txt", 4), Staged::Complete(theirs));
        assert!(
            !short.exists() && !long.exists(),
            "stale leftovers survived"
        );
        assert!(alien.exists(), "a file that isn't ours was deleted");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Our own staging file can be left full-length by a crash, and a recycled
    /// pid hands it back to us. Resuming it would ask the server for a range
    /// past the end and get a 416, so it goes down the checksum path instead.
    #[test]
    fn our_own_finished_staging_file_is_checked_not_resumed() {
        let dir = staging_dir("ourpid");
        let ours = staging_path(&dir, "vocab.txt");
        std::fs::write(&ours, b"1234").unwrap();
        assert_eq!(stage(&dir, "vocab.txt", 4), Staged::Complete(ours.clone()));
        // Short, it's ours to resume and is left exactly where it was.
        std::fs::write(&ours, b"12").unwrap();
        assert_eq!(stage(&dir, "vocab.txt", 4), Staged::Partial(ours.clone()));
        assert_eq!(std::fs::read(&ours).unwrap(), b"12");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn staging_adopts_the_longest_partial_under_our_own_name() {
        let dir = staging_dir("adopt");
        let ours = staging_path(&dir, "vocab.txt");
        let small = foreign(&dir, 1, b"1");
        let big = foreign(&dir, 2, b"123");
        assert_eq!(stage(&dir, "vocab.txt", 8), Staged::Partial(ours.clone()));
        // The longest one is what curl resumes, and it is now ours alone.
        assert_eq!(std::fs::read(&ours).unwrap(), b"123");
        assert!(!small.exists() && !big.exists());

        // Nothing to adopt is the same answer, minus the file.
        let empty = staging_dir("adopt-none");
        assert_eq!(
            stage(&empty, "vocab.txt", 8),
            Staged::Partial(staging_path(&empty, "vocab.txt"))
        );
        assert!(!staging_path(&empty, "vocab.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&empty).unwrap();
    }

    #[test]
    fn downloads_come_from_the_pinned_revision() {
        assert_eq!(
            url("model.safetensors"),
            "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/\
             5c38ec7c405ec4b44b94cc5a9bb96e735b38267a/model.safetensors"
        );
        let hint = manual_hint(Path::new("/tmp/models"));
        assert!(hint.contains("/tmp/models"), "{hint}");
        for (name, _, _) in FILES {
            assert!(hint.contains(&url(name)), "{name} missing from: {hint}");
            // A hand-download must land on a .part name, or it would be trusted
            // on size alone instead of being checksummed and renamed.
            let staged = format!("{name}.part");
            assert!(hint.contains(&staged), "{staged} missing from: {hint}");
            assert!(is_staging(&staged, name));
        }
    }

    /// The tripwire for pooling and prefixes: two ways of saying the same thing
    /// must score higher against each other than against something unrelated.
    /// Needs the real weights on disk — run by hand with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn related_sentences_outrank_an_unrelated_one() {
        let e = Embedder::load().unwrap();
        let v = e
            .embed(&[
                "deploy failed at 3am".to_string(),
                "the deployment broke overnight".to_string(),
                "grocery list apples".to_string(),
            ])
            .unwrap();
        assert_eq!(v[0].len(), 384, "bge-small is 384-dimensional");
        let (related, unrelated) = (cosine(&v[0], &v[1]), cosine(&v[0], &v[2]));
        assert!(
            related > unrelated,
            "related {related} should beat unrelated {unrelated} — check the pooling"
        );
    }
}
