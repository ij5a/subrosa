# Changelog

All notable changes to subrosa are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.23.0] - 2026-08-08

### Added

- Battery-included semantic search: `subrosa embed` and `search --semantic` now run the bge-large-en-v1.5 model inside subrosa itself via candle, CPU-only, pure Rust. No Ollama, no server, nothing else to install. On the first `subrosa embed` the model (~1.3 GB, MIT-licensed, pinned to one revision) downloads once into `~/.claude/subrosa/models/` through your system `curl` — every file sha256-pinned and size-capped, resumable if interrupted, and serialized by a lock so two runs never fight. No curl? The error names the three files and where to put them, and hand-downloaded files are checksummed too.

### Changed

- BREAKING: the Ollama backend is gone, and with it `SUBROSA_OLLAMA_HOST`/`ollama_host` and `SUBROSA_EMBED_MODEL`/`embed_model`. Vectors made with v0.22.0 stay in the database under their old key and are ignored; run `subrosa embed` once after upgrading to re-index with the built-in model.
- The binary now opens no sockets at all — the one network touch left is the model download, a visible `curl` child process. Search queries are still redacted before they reach the model, and nothing ever leaves your machine.
- Direct dependencies 7 → 11 (candle-core, candle-nn, candle-transformers, sha2). candle is pinned to 0.9 — the last version with a pure-Rust tree — and a guard test fails the build if a bump drags C code back in. Binary ~4 → ~5.3 MB.

## [0.22.0] - 2026-08-08

### Added

- `subrosa fact doctor` — a read-only integrity check over a project's memory. It flags malformed or spliced frontmatter, missing or invalid fields, dangling `[[links]]`, duplicate name slugs, and orphaned leaves or facts. It errors on your own registered facts and only warns on foreign leaves, so it never cries wolf on Claude Code's native auto-memory. The `/subrosa:checkpoint` skill now runs it after each save, so a corrupted leaf — the kind that silently stops loading — gets caught where it happens. Exit 1 on any error, so it can gate a script.
- Opt-in semantic search. `subrosa embed` precomputes an embedding per turn via a local Ollama endpoint into a lazy `turn_embeddings` table (no schema change, zero cost until you use it), and `search --semantic` ranks the whole archive by meaning — surfacing turns that share no words with your query. Configure with `SUBROSA_OLLAMA_HOST` / `ollama_host` (default `localhost:11434`) and `SUBROSA_EMBED_MODEL` / `embed_model` (default `nomic-embed-text`); `subrosa embed --rebuild` re-embeds a model from scratch. The query is redacted before it is sent, and if the index is incomplete the search says so rather than quietly under-searching. Needs Ollama running; the one-time backfill takes minutes and roughly 150 MB on a large archive. The Ollama client is hand-rolled over `std::net` and the existing `serde_json`, so this adds zero dependencies — still 7 crates, still a single static binary with no networking crate.

### Changed

- Positioning: subrosa's core — capture, recall, storage, and all non-semantic search — stays zero-network. The one exception is the opt-in `search --semantic` and `subrosa embed`, which reach a local Ollama model you run over a plain-HTTP localhost call. Recall stays keyword-only and never touches the network. README, FAQ, and the comparison doc are reworded to match.

## [0.21.0] - 2026-08-08

### Added

- Per-project `MEMORY.md` budget: a `.budget` file in a project's memory folder (one number, bytes) overrides the 23,000-byte default, so a saturated project can stop dropping good facts. An explicit `--budget` still beats the file; a bad file warns and falls back; an unreadable one stops `generate` before it rewrites anything.
- The index now also honors Claude Code's 200-line `MEMORY.md` load cap: facts past line 200 are archive-dropped and reported exactly like byte-budget drops, and the session-start nudge and dashboard flag line overflow too.
- Encrypted mirror snapshots: set a passphrase (`subrosa setup`, or the `mirror_passphrase` config key / `SUBROSA_MIRROR_PASSPHRASE`) and the mirror copy of each backup snapshot is sealed with XChaCha20-Poly1305, keyed by Argon2id from your passphrase, parameters stored in the file header. The live database and local snapshots stay plaintext — this protects the copy that leaves the machine. Keep the passphrase in a password manager: without it the off-machine copy is unrecoverable.
- `subrosa restore <file.enc> [--out path]`: decrypts a sealed snapshot, verifies it is a SQLite database, refuses to write into cloud-synced folders by default, never overwrites an existing file, and never touches the live database.
- Redaction now masks passphrase-shaped secrets whole-line (`SUBROSA_MIRROR_PASSPHRASE=...`, `mirror_passphrase: ...`) and env-var-style names the old pattern missed (`MYSQL_PASSWORD=`, `API_TOKEN=`), so the mirror passphrase can never reach the archive it seals.

### Changed

- `mirror=none` in the config now also switches off a `SUBROSA_MIRROR` environment override — a security opt-out must not lose to a shell profile. Delete the line or re-run `subrosa setup` to mirror again. This is a behavior change if you kept `mirror=none` in the config while mirroring purely through the env var.
- Once encryption is intended — a passphrase resolves, or a sealed `.enc` sits in the mirror folder — the mirror never goes out in plaintext: stale readable copies (including iCloud eviction placeholders) are purged on every backup path, downgrading requires deleting the `.enc` yourself, and any failure skips the mirror instead of falling back.
- Two new dependencies, `chacha20poly1305` and `argon2` (both pure Rust); direct crates go 5 → 7.

## [0.20.1] - 2026-07-19

### Fixed

- `subrosa checkpoint-mark` could stamp the wrong session when another project's transcript was modified more recently — a concurrent session or a spawned agent — so the just-checkpointed session re-queued and got drained a second time. The mark now targets the newest transcript in the current directory's own project, and accepts an optional session id or unique prefix (`subrosa checkpoint-mark <id>`) to pin it exactly.

## [0.20.0] - 2026-07-19

### Added

- `subrosa search --fuzzy` now rescues small typos. When the substring pass finds nothing, it falls back to the nearest matches within one edit — a wrong, missing, extra, or swapped letter (`latecny` finds `latency`). Hits print under a new `no substring match — nearest matches (within one edit):` header. With several terms, one typo'd term is rescued while the others stay exact. Existing searches are unchanged: the fallback only runs where you previously got "no matches", and recall stays on exact keyword matching.

### Changed

- README, FAQ, and `--help` now describe `--fuzzy` as matching "partial names and small typos", with the one-edit mechanism spelled out in the FAQ.

## [0.19.2] - 2026-07-07

### Changed

- The `/subrosa:checkpoint` report's safe-to-wipe line now leads with 👍 instead of 🟢.

## [0.19.1] - 2026-07-07

### Changed

- The `/subrosa:checkpoint` skill now closes with a short glance instead of a long ledger. The report is a one-line headline (`✅ Saved X, Updated Y.` — archived and flagged counts ride the same line only when non-zero), a `📋 Recap` of what the session actually did, and the `🟢 Safe to /clear or /compact.` line. The old per-category Saved/Updated/Skipped lists, the `MEMORY.md` byte block, and the named archive/review sections no longer print. The skill still runs the full save/update/staleness/regenerate procedure — only what it prints changed; the per-leaf detail stays recoverable with `subrosa fact list` and `subrosa search`.

## [0.19.0] - 2026-06-21

### Added

- `subrosa init --claude-md` now installs a second standing instruction, `## Memory auto-checkpoint (subrosa)`, next to the existing recall section. It tells Claude to run the `/subrosa:checkpoint-backlog` skill in the background — without blocking the task you're working on — whenever sessions are queued for checkpoint, so the backlog gets cleared on its own. The two sections are upserted independently: re-running `init --claude-md` adds whichever section is missing without duplicating or rewriting what's already there, so an existing install picks up the new section on the next run.

## [0.18.0] - 2026-06-21

### Changed

- The per-prompt checkpoint-backlog directive now tells Claude to run `/subrosa:checkpoint-backlog` in the background without blocking the current turn, instead of just naming the skill. The reminder that rides each prompt while sessions are queued now cues the action — kick the backlog off as a non-blocking background task and carry on — so a queued backlog gets handled without making you wait.

## [0.17.0] - 2026-06-21

### Added

- The checkpoint-backlog reminder now also rides every prompt (`UserPromptSubmit`), not just the once-per-session-start nudge. While sessions are waiting to be checkpointed, subrosa injects a short `[subrosa] ACTION REQUIRED before this turn: N session(s) queued…` line ahead of recall, so the backlog stays in view even when a busy first task scrolls the session-start nudge out of sight. It repeats until the queue drains, honors `checkpoint_nudge` (`off` silences it, `quiet` is a one-liner), and stays `[subrosa]`-prefixed so it never feeds back into the archive.

## [0.16.0] - 2026-06-21

### Changed

- The `Stop` hook's per-turn live ingest now resumes from a saved byte offset instead of re-reading the whole transcript each turn. The cost stays flat no matter how long the session runs — a long multi-hour session ingests each turn as fast as a fresh one (~7 ms), where before the per-turn cost grew with the transcript length. The archive it produces is byte-for-byte identical to a full re-read; this is a speed change only. Schema v4 adds two `sessions` columns (`scan_offset`, `scan_seq`) for the cursor — an additive migration; existing archives re-read once on the next ingest to set it.

## [0.15.0] - 2026-06-21

### Changed

- The session-start checkpoint-backlog nudge is loud by default. When sessions are waiting to be checkpointed, subrosa prints an `[subrosa] ACTION REQUIRED — N session(s) queued…` block — a short directive plus the up-to-5 newest queued session ids — instead of the old one-line note, so the backlog is harder to skip. Every line stays `[subrosa]`-prefixed, so it's still dropped on ingest and never feeds back into recall.

### Added

- `checkpoint_nudge` config key (and the `SUBROSA_CHECKPOINT_NUDGE` env var, which wins over the config file) to pick the nudge style: `loud` (default), `quiet` (the previous one-liner), or `off`. An unset or unknown value falls back to loud.

## [0.14.1] - 2026-06-19

### Fixed

- Recall now finds a past session even when an identifier was written with a different separator. The FTS5 index splits `-`, `_`, and `.` the same way, but the relevance post-filter didn't — so a stored `cache_prod` wouldn't match a prompt's `cache-prod` even though the search had already found the row. The post-filter now folds the three separators the way FTS does, and it only re-admits rows the search already returned, so recall stays exactly as quiet on weak matches as before.
- The match-term gate no longer over-filters long pasted prompts. It scales the number of required matching terms with prompt length, but that's now capped, so a pasted error log plus a question can't demand so many matches that the genuinely relevant session is dropped. The anchor requirement is unchanged.

## [0.14.0] - 2026-06-19

### Added

- `subrosa search -C/--context N` prints the N turns on each side of every hit (same session), so you can read a match in context without opening the whole session. The default `N=0` keeps the output byte-for-byte as before.
- `subrosa search --exclude <term>` drops hits that contain the term (repeatable). It's built on the same phrase-quoting as the positive terms, so identifiers stay hyphen-safe, and it's ignored with `--raw`.
- `subrosa search --any` matches any term instead of all — OR instead of the default AND. It composes with `--exclude` as `(a OR b) NOT c`.
- `subrosa fact search <terms>` runs a bm25-ranked full-text search over the curated facts (title, hook, description), scoped to the current project, with a `--status` filter (active by default). It finds one fact once a project has dozens.

## [0.13.0] - 2026-06-17

### Added

- `subrosa search` now shows each hit's relative age after the timestamp — `[2026-05-29 13:42] (7mo old)` — using the same `(today)` / `(3d old)` / `(2w old)` form recall already prints. You can tell how fresh a result is at a glance without doing date math. Both surfaces share one helper, so they always read the same.

## [0.12.1] - 2026-06-17

### Changed

- The README banner is now an animated wordmark SVG — the rings in the "o" of the mark spin. Refreshed the brand assets (banner, mark, social card) to match. No code or behavior change.

## [0.12.0] - 2026-06-17

### Added

- Near-real-time archiving of the in-progress session. A `Stop` hook now runs `subrosa hook stop` after each assistant turn, incrementally ingesting just the active transcript (not a full sweep), so the current session is searchable with `subrosa search` before it ends — handy when a second terminal or agent needs to see this session, or when you switch sessions fast and want the one you just left already archived. It adds ~7 ms per turn and runs after the reply is on screen. Automatic prompt recall still skips the current session on purpose, so it never echoes your own turns back.

## [0.11.0] - 2026-06-16

### Added

- Auto-derived session tags. When a session is archived, subrosa reads its stored (already-redacted) turns and derives three kinds of read-only tag — `tool:bash` (tools the session used), `ext:rs` (file types it touched), and `topic:cache-prod` (the distinctive terms it was about). They're computed locally with no LLM, cost zero tokens, and are recomputed (never hand-edited) on each archive. A new `session_tags` table holds them — schema v3, an additive migration with a one-time backfill for existing archives.
- `subrosa search` filters: `--after` / `--before` (UTC `YYYY-MM-DD`, inclusive) narrow by date, and `--tag` (repeatable, ANDed) narrows by tag. The filters run before `bm25`, so ranking is unchanged.
- `subrosa sessions`: a new verb that lists past sessions newest-first with their tags, filterable by `--project`, `--after`/`--before`, and `--tag`. It's the by-session view of the archive — find work by what it was about without remembering a keyword.
- `subrosa session <id> --tags`: an opt-in flag that adds the session's tags to the dump header. The default output is byte-for-byte unchanged.

## [0.10.0] - 2026-06-16

### Added

- Auto-recall lines now show a relative age after the session date — `(today)`, `(3d old)`, `(2w old)`, `(7mo old)`, `(2y old)` — so Claude leans on fresh hits and double-checks stale ones against current code. The per-prompt cap is unchanged (~180 tokens; `scripts/bench.sh` measures 177).

### Changed

- Raised the declared MSRV to `rust-version = "1.85"` to match the locked `clap 4.6` (which already requires it), refreshed the dependency lockfile, and adopted the `is_none_or` / `repeat_n` idioms that MSRV unlocks. Still 5 direct dependencies — none added.

## [0.9.0] - 2026-06-15

### Added

- `subrosa fact link <anchor>`: a read-only verb that shows the `[[name]]` links into and out of a curated fact. It lists what the fact links to (outbound) and which facts link back (inbound), resolves each slug to its title, and flags links that point to a fact that doesn't exist (`[dangling]`), to the fact itself (`[self]`), or to an archived fact. It's the curated counterpart to `related`: the links are written by hand, so the connections are exact rather than guessed from co-occurrence. The `/subrosa:checkpoint` skill now writes these links when saving a fact and verifies them with `subrosa fact link`.

## [0.8.1] - 2026-06-14

### Changed

- `subrosa session <id>` now accepts a unique id prefix, not just the full id — so the 8-character session id that `search` and `related` print can be pasted straight back to open the session. An ambiguous prefix lists the candidates; an exact id still resolves outright.

## [0.8.0] - 2026-06-14

### Added

- `subrosa related <identifier>`: a read-only verb that surfaces what co-occurs with an identifier across the archive. It ranks the terms that keep showing up alongside the anchor — down-weighting words that are common archive-wide, so identifiers and distinctive terms rise — and then lists the past sessions those terms came from. It answers "what did this work touch", which `search` (text match) can't. Co-occurrence is computed in process over the matched sessions, so it stays sub-second even on a 50k-turn archive.

## [0.7.0] - 2026-06-14

### Added

- GitHub community health files: Code of conduct (Contributor Covenant 2.1), `CONTRIBUTING.md`, `SECURITY.md`, and issue + pull-request templates.

### Changed

- `/subrosa:checkpoint-backlog` now checkpoints a multi-project queue in parallel — one sub-agent per project, all run at once, with each project's sessions handled serially so a project's `MEMORY.md` never races itself. A single-project queue stays sequential, exactly as before. The orchestrator clears a session from the queue only after its lane reports it done, so a failed lane simply leaves its sessions queued for the next run.

## [0.6.0] - 2026-06-14

### Changed

- Auto-recall now injects a **match-centered snippet** (FTS5 `snippet()`) so the line shows *why* a past session matched, instead of the turn's first 160 characters. The ~180-token-per-prompt cap is unchanged.
- The recall post-match gate is **stem/prefix-aware** instead of substring-based: it keeps Porter word-form matching (`deploy`↔`deployed`) while rejecting false positives like `spec` inside `respect`.
- Recall ranking gained a relative `bm25` floor (drops weak tails) and a mild **recency tie-break** (a fresher session wins only a genuine near-tie), and the required matched-term count now scales with prompt length.

## [0.5.0] - 2026-06-14

### Added

- `subrosa search --fuzzy`: substring and typo matching via a trigram index, built on first use. Exact search and auto-recall stay on the Porter index.

## [0.4.2] - 2026-06-13

### Added

- `subrosa init --claude-md`: opt-in, idempotent command that appends the memory-recall block to your `CLAUDE.md` so Claude searches the archive on its own.

## [0.4.1] - 2026-06-12

### Changed

- Performance: fork-free hook wrapper, SQLite `mmap` + in-memory temp store, zero-copy redaction, and a capped recall FTS union bring the per-prompt hook fire to well under 10 ms. Added `scripts/bench.sh`.

## [0.4.0] - 2026-06-12

### Added

- PreCompact hook: archives the conversation before compaction summarizes it away, and resets recall dedup so post-compact prompts can re-surface relevant sessions.

### Changed

- FTS now uses the Porter stemmer (schema v2) so recall matches word forms (`deploy` / `deployed` / `deploying`); identifiers still match exactly. The index rebuilds from the source tables on upgrade.

## [0.3.0] - 2026-06-12

### Changed

- Token-economy hardening: recall requires an anchor-grade term and dedups per session, index hook lines are capped, and the session-start nudge is filtered out of the archive. Added a `SIGPIPE` guard so `subrosa search | head` exits cleanly.

## [0.2.1] - 2026-06-12

### Fixed

- Eliminated SQLite lock errors from concurrent hooks: immediate transactions, read-only connects, and atomic retrying backups.

## [0.2.0] - 2026-06-12

### Added

- Auto-recall on prompts, curated facts with byte-budgeted `MEMORY.md` generation, the `/subrosa:checkpoint` and `/subrosa:checkpoint-backlog` skills, the `subrosa` dashboard, `subrosa import`, and a pre-commit gate.

## [0.1.0] - 2026-06-12

### Added

- Initial release: the Rust memory engine (archives Claude Code transcripts into a local SQLite FTS5 database), Claude Code plugin wiring, the plugin binary bootstrap, the install script, release automation, and CI.

[0.23.0]: https://github.com/ij5a/subrosa/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/ij5a/subrosa/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/ij5a/subrosa/compare/v0.20.1...v0.21.0
[0.20.1]: https://github.com/ij5a/subrosa/compare/v0.20.0...v0.20.1
[0.20.0]: https://github.com/ij5a/subrosa/compare/v0.19.2...v0.20.0
[0.19.2]: https://github.com/ij5a/subrosa/compare/v0.19.1...v0.19.2
[0.19.1]: https://github.com/ij5a/subrosa/compare/v0.19.0...v0.19.1
[0.19.0]: https://github.com/ij5a/subrosa/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/ij5a/subrosa/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/ij5a/subrosa/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/ij5a/subrosa/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/ij5a/subrosa/compare/v0.14.1...v0.15.0
[0.14.1]: https://github.com/ij5a/subrosa/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/ij5a/subrosa/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/ij5a/subrosa/compare/v0.12.1...v0.13.0
[0.12.1]: https://github.com/ij5a/subrosa/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/ij5a/subrosa/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/ij5a/subrosa/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/ij5a/subrosa/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/ij5a/subrosa/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/ij5a/subrosa/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/ij5a/subrosa/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/ij5a/subrosa/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/ij5a/subrosa/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ij5a/subrosa/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/ij5a/subrosa/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/ij5a/subrosa/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/ij5a/subrosa/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ij5a/subrosa/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/ij5a/subrosa/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/ij5a/subrosa/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ij5a/subrosa/releases/tag/v0.1.0
