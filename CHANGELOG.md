# Changelog

All notable changes to subrosa are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.25.1] - 2026-08-15

### Fixed

- `subrosa fact --help` now documents `--status` for both `list` and `fact search`. It says active facts show by default. Archived-fact searches no longer return an unexplained empty result.

## [0.25.0] - 2026-08-09

### Changed

- Default semantic search sets itself up. You do not need to type `subrosa embed`. Session start and end start a separate low-priority process and return immediately, with no measurable hook-time change. The process downloads the model once and indexes the archive newest-first. Recent sessions become searchable within a minute. It exits after indexing, handles each new session, and shows progress; a partial index reports its state instead of asking you to run a command.
- `semantic=off` in `~/.claude/subrosa/config`, or `SUBROSA_SEMANTIC=off`, disables background runs, model downloads, and every network call. If config cannot be read, subrosa treats it as off. `subrosa embed` remains available for manual indexing.
- A failed run is recorded. It retries after 1 hour, then with delays that double up to 1 day. The report shows the real reason. It does not retry every session. Keyword search remains unaffected.
- Only one indexing run can happen at a time. A database-keyed lock covers hook and manual runs, so two ending sessions index once. Background runs use half the cores. Manual `subrosa embed` uses all cores.
- Hardening now runs `curl`, `stty`, and `git` from absolute paths. Steering files have size caps and must be plain files. Pipes and symlinks cannot stall hooks or redirect writes. Replaced files use temp files and rename, so an interrupted write cannot leave a half-file. If the terminal cannot hide input, subrosa refuses to ask for the mirror passphrase.
- README, FAQ, and the comparison doc now state that the model downloads on first run by default. They say there is no daemon, only a finite indexing pass that exits. They also state what the network downloads and what stays local.

## [0.24.0] - 2026-08-09

### Changed

- A 127,688-turn archive fell from 47 hours to 9 minutes 21 seconds, or 227 turns/sec, on an M3 Max. It peaked at 1.25 GB of memory. A smaller model and 1 worker thread per core sharing 1 loaded model caused the gain. This replaced 1 core doing everything. Embedding each repeated turn only once also helped. About 1/5 of the archive repeats a turn.
- BREAKING: The embedding model changes from bge-large-en-v1.5 to [bge-small-en-v1.5](https://huggingface.co/BAAI/bge-small-en-v1.5) (MIT, pinned to one revision). The download drops from 1.3 GB to 133 MB. Vectors drop from 1024 to 384 dimensions, making semantic search about 2.7x faster to scan. Vectors use model keys, so old vectors cannot be ranked. `subrosa embed` deletes old-model vectors, reports the count, and re-indexes. The database barely changes size because the new vectors use about the same disk.
- Embedding now works newest first. Recent sessions become searchable within the first minute. It remains resumable. Press Ctrl-C and the next run continues. The progress line shows a rate and estimate, for example `embedded 4096/127688 · 192/s · ~11m left`, instead of only a count.
- macOS builds link Apple's Accelerate framework for matrix work. This is a link step, not a compile step. Direct dependencies stay at 11 on every platform. Linux and musl builds are unchanged.

## [0.23.0] - 2026-08-08

### Added

- Semantic search is built into subrosa. `subrosa embed` and `search --semantic` run bge-large-en-v1.5 through candle, CPU-only and pure Rust. No Ollama, server, or extra install is needed. The first `subrosa embed` downloads the model once to `~/.claude/subrosa/models/` through system `curl`. The model is about 1.3 GB, MIT-licensed, and pinned to one revision; every file is sha256-pinned and size-capped. Downloads resume after interruption and a lock serializes them. Without curl, the error names the 3 files and their location; hand-downloaded files are also checksummed.

### Changed

- The Ollama backend is gone. So are `SUBROSA_OLLAMA_HOST`/`ollama_host` and `SUBROSA_EMBED_MODEL`/`embed_model`. v0.22.0 vectors stay under their old key and are ignored. Run `subrosa embed` once after upgrading.
- The binary opens no sockets. The only network action is the model download through a visible `curl` child process. Search queries are redacted before the model sees them. Nothing leaves your machine.
- Direct dependencies grow from 7 to 11: candle-core, candle-nn, candle-transformers, and sha2. candle stays pinned to 0.9, the last pure-Rust version. A guard test rejects any bump that brings back C code. The binary grows from about 4 MB to about 5.3 MB.

## [0.22.0] - 2026-08-08

### Added

- `subrosa fact doctor` is a read-only memory integrity check. It flags malformed or spliced frontmatter, missing or invalid fields, dangling `[[links]]`, duplicate name slugs, and orphaned leaves or facts. It errors on your registered facts and warns on foreign leaves. This avoids false errors on Claude Code's native auto-memory. The `/subrosa:checkpoint` skill runs it after each save and catches corrupted leaves when they occur. It exits 1 on any error.
- Opt-in semantic search adds a lazy `turn_embeddings` table. `subrosa embed` creates one embedding per turn through a local Ollama endpoint and adds no schema change or cost until used. `search --semantic` ranks the archive by meaning, including turns with no shared query words. Configure it with `SUBROSA_OLLAMA_HOST` / `ollama_host` (default `localhost:11434`) and `SUBROSA_EMBED_MODEL` / `embed_model` (default `nomic-embed-text`). `subrosa embed --rebuild` re-embeds a model from scratch. Queries are redacted before sending; incomplete indexes report their state instead of under-searching.
- Ollama must run. A large-archive backfill takes minutes and about 150 MB. The client uses hand-rolled `std::net` and existing `serde_json`. This adds zero dependencies, leaving 7 crates, one static binary, and no networking crate.

### Changed

- Core capture, recall, storage, and non-semantic search remain zero-network. The opt-in `search --semantic` and `subrosa embed` calls use a local Ollama model over plain HTTP. Recall stays keyword-only and never uses the network. README, FAQ, and the comparison doc now match.

## [0.21.0] - 2026-08-08

### Added

- A per-project `MEMORY.md` budget can use a `.budget` file with one number in bytes. It overrides the 23,000-byte default. An explicit `--budget` wins. A bad file warns and falls back. An unreadable file stops `generate` before rewriting.
- The index honors Claude Code's 200-line `MEMORY.md` cap. Facts after line 200 are archive-dropped and reported like byte-budget drops. The session-start nudge and dashboard also flag line overflow.
- Encrypted mirror snapshots use XChaCha20-Poly1305 and Argon2id. Set a passphrase with `subrosa setup`, `mirror_passphrase`, or `SUBROSA_MIRROR_PASSPHRASE`. Each mirror backup is sealed, with Argon2id parameters in the file header. The live database and local snapshots stay plaintext; this protects the copy that leaves the machine. Keep the passphrase in a password manager. Without it, the off-machine copy cannot be recovered.
- `subrosa restore <file.enc> [--out path]` decrypts a sealed snapshot and verifies its SQLite format. It refuses cloud-synced output folders by default. It never overwrites an existing file or touches the live database.
- Redaction now masks whole-line passphrase secrets such as `SUBROSA_MIRROR_PASSPHRASE=...` and `mirror_passphrase: ...`. It also masks names such as `MYSQL_PASSWORD=` and `API_TOKEN=`. The mirror passphrase cannot reach the archive it seals.

### Changed

- `mirror=none` in config now disables a `SUBROSA_MIRROR` environment override. Delete the line or run `subrosa setup` to mirror again. This changes behavior when config has `mirror=none` and mirroring uses only the environment.
- When encryption is intended, the mirror never sends plaintext. This applies when a passphrase resolves or a sealed `.enc` is in the mirror folder. Every backup removes stale readable copies, including iCloud eviction placeholders. Downgrading requires deleting the `.enc` yourself. Any failure skips the mirror instead of falling back.
- Two pure-Rust dependencies, `chacha20poly1305` and `argon2`, raise direct crates from 5 to 7.

## [0.20.1] - 2026-07-19

### Fixed

- `subrosa checkpoint-mark` could choose another project's newer transcript, including a concurrent session or spawned agent. The saved session then re-queued and ran twice. It now chooses the newest transcript in the current directory's project. An optional session id or unique prefix, `subrosa checkpoint-mark <id>`, pins the target.

## [0.20.0] - 2026-07-19

### Added

- `subrosa search --fuzzy` now handles small typos. If substring search finds nothing, it prints a new `no substring match` header and finds matches within 1 edit. It handles a wrong, missing, extra, or swapped letter, such as `latecny` for `latency`. With several terms, it rescues one typo while keeping other terms exact. Existing searches stay unchanged because the fallback runs only after no matches. Recall remains exact keyword search.

### Changed

- README, FAQ, and `--help` now describe `--fuzzy` as matching partial names and small typos. The FAQ explains the 1-edit rule.

## [0.19.2] - 2026-07-07

### Changed

- The `/subrosa:checkpoint` report safe line now starts with 👍 instead of 🟢.

## [0.19.1] - 2026-07-07

### Changed

- The `/subrosa:checkpoint` report now uses a short glance instead of a long ledger. It prints a one-line headline, a `📋 Recap`, and `🟢 Safe to /clear or /compact.`. The headline is `✅ Saved X, Updated Y.`. Add archived and flagged counts on that line only when non-zero.
- The old Saved, Updated, and Skipped lists, `MEMORY.md` byte block, and named archive or review sections no longer print. The skill still runs the full save, update, staleness, and regenerate procedure. Only the printed report changed. Per-leaf detail remains available through `subrosa fact list` and `subrosa search`.

## [0.19.0] - 2026-06-21

### Added

- `subrosa init --claude-md` now installs `## Memory auto-checkpoint (subrosa)` beside the recall section. It tells Claude to run `/subrosa:checkpoint-backlog` in the background without blocking the task when sessions wait for checkpointing. The two sections update independently. Re-running `init --claude-md` adds only missing sections. It does not duplicate or rewrite existing sections. Existing installs get the new section on the next run.

## [0.18.0] - 2026-06-21

### Changed

- The per-prompt checkpoint-backlog directive now tells Claude to run `/subrosa:checkpoint-backlog` in the background without blocking the current turn, rather than only naming the skill. The queued-session reminder now starts that non-blocking task, so the backlog clears without making the user wait.

## [0.17.0] - 2026-06-21

### Added

- The checkpoint-backlog reminder now runs on every `UserPromptSubmit`, not only once at session start. While sessions wait, subrosa adds `[subrosa] ACTION REQUIRED before this turn: N session(s) queued…` before recall. It stays visible when a busy first task hides the session-start nudge. The reminder repeats until the queue drains. It honors `checkpoint_nudge`: `off` silences it and `quiet` makes it one line. Every line starts with `[subrosa]`, so it never feeds back into the archive.

## [0.16.0] - 2026-06-21

### Changed

- The `Stop` hook now resumes live ingest from a saved byte offset instead of rereading the transcript. Per-turn cost stays flat at about 7 ms, even in long sessions. The archive stays byte-for-byte identical to a full reread. This changes speed only. Schema v4 adds `scan_offset` and `scan_seq` to `sessions` as an additive migration. Existing archives reread once on the next ingest to set the cursor.

## [0.15.0] - 2026-06-21

### Changed

- The session-start checkpoint-backlog nudge is loud by default. It prints an `[subrosa] ACTION REQUIRED` block with a short directive and up to 5 newest queued session ids. It replaces the old one-line note. Every line starts with `[subrosa]`, so ingest drops it and it never feeds back into recall.

### Added

- The `checkpoint_nudge` config key and `SUBROSA_CHECKPOINT_NUDGE` environment variable choose the style. The environment variable wins. Options are `loud` (default), `quiet` (previous one-line style), and `off`. An unset or unknown value uses loud.

## [0.14.1] - 2026-06-19

### Fixed

- Recall now matches identifiers with different separators. FTS5 treats `-`, `_`, and `.` alike, but the old relevance filter did not. A stored `cache_prod` now matches `cache-prod` without adding weak results. The filter folds all 3 separators and only keeps rows returned by search. Recall remains quiet on weak matches.
- The match-term gate no longer over-filters long pasted prompts. It scales required terms with prompt length and caps the result. A pasted error log plus a question cannot demand too many matches. The anchor requirement is unchanged.

## [0.14.0] - 2026-06-19

### Added

- `subrosa search -C/--context N` prints N turns on each side of each hit in the same session. The default `N=0` keeps output byte-for-byte unchanged. `subrosa search --exclude <term>` removes hits containing a term. The flag is repeatable. It uses the positive-term phrase quoting, so identifiers stay hyphen-safe. It is ignored with `--raw`.
- `subrosa search --any` matches any term instead of all terms. It uses OR instead of AND. With `--exclude`, it behaves as `(a OR b) NOT c`.
- `subrosa fact search <terms>` runs a bm25-ranked full-text search over title, hook, and description. It uses the current project and `--status`, active by default. It finds a fact in projects with dozens of facts.

## [0.13.0] - 2026-06-17

### Added

- `subrosa search` now shows relative age after each timestamp, such as `[2026-05-29 13:42] (7mo old)`. It uses the same `(today)`, `(3d old)`, and `(2w old)` forms as recall. This shows freshness without date math. Both surfaces use one helper.

## [0.12.1] - 2026-06-17

### Changed

- The README banner is now an animated wordmark SVG. The rings in the `o` spin. The banner, mark, and social card assets were refreshed. There is no code or behavior change.

## [0.12.0] - 2026-06-17

### Added

- A `Stop` hook now runs `subrosa hook stop` after each assistant turn. It incrementally ingests only the active transcript, so the current session is searchable before it ends. This helps a second terminal or agent and fast session switches. It adds ~7 ms per turn and runs after the reply appears. Automatic prompt recall still skips the current session, so it never echoes your own turns.

## [0.11.0] - 2026-06-16

### Added

- Archived sessions now get read-only tags from stored, redacted turns. Tags cover tools such as `tool:bash`, file types such as `ext:rs`, and distinctive topics such as `topic:cache-prod`. They run locally, use no LLM or tokens, and are recomputed on each archive. They are never hand-edited. The `session_tags` table uses schema v3 with an additive migration and one-time backfill.
- `subrosa search` now supports `--after` and `--before` with inclusive UTC `YYYY-MM-DD` dates. Repeatable `--tag` filters use AND. Filters run before `bm25`, so ranking stays unchanged.
- `subrosa sessions` lists past sessions newest-first with tags. It supports `--project`, `--after`, `--before`, and `--tag` filters. This provides a by-session view and helps find work without a keyword.
- The opt-in `subrosa session <id> --tags` flag adds tags to the dump header. The default output stays byte-for-byte unchanged.

## [0.10.0] - 2026-06-16

### Added

- Auto-recall lines now show relative age after the session date: `(today)`, `(3d old)`, `(2w old)`, `(7mo old)`, or `(2y old)`. Claude can weigh fresh hits and check stale ones against current code. The per-prompt cap stays about 180 tokens. `scripts/bench.sh` measures 177.

### Changed

- The declared MSRV is now `rust-version = "1.85"` to match locked `clap 4.6`. The dependency lockfile was refreshed. The code now uses `is_none_or` and `repeat_n`. There are still 5 direct dependencies, with none added.

## [0.9.0] - 2026-06-15

### Added

- `subrosa fact link <anchor>` is a read-only command for `[[name]]` links into and out of a curated fact. It lists outbound and inbound links, resolves slugs to titles, and flags missing facts as `[dangling]`, self-links as `[self]`, and archived facts. It complements `related`. Hand-written links are exact, unlike co-occurrence guesses. The `/subrosa:checkpoint` skill writes and checks them.

## [0.8.1] - 2026-06-14

### Changed

- `subrosa session <id>` now accepts a unique id prefix, not only the full id. The 8-character id from `search` and `related` can open a session. An ambiguous prefix lists candidates. An exact id still resolves directly.

## [0.8.0] - 2026-06-14

### Added

- `subrosa related <identifier>` shows terms that co-occur with an identifier across the archive. It down-weights common archive words and ranks distinctive terms, then lists their sessions. It answers what the work touched, unlike text search. It runs in process and stays under 1 second on a 50k-turn archive.

## [0.7.0] - 2026-06-14

### Added

- GitHub community health files now include the Code of Conduct (Contributor Covenant 2.1), `CONTRIBUTING.md`, `SECURITY.md`, and issue and pull-request templates.

### Changed

- `/subrosa:checkpoint-backlog` now handles multi-project queues in parallel. It starts one sub-agent per project and keeps each project's sessions serial. A single-project queue stays sequential. The orchestrator clears a session only after its lane reports completion. A failed lane leaves its sessions queued.

## [0.6.0] - 2026-06-14

### Changed

- Auto-recall now injects an FTS5 `snippet()` centered on the match, instead of the turn's first 160 characters. The cap stays about 180 tokens per prompt.
- The recall post-match gate is stem and prefix aware. It keeps Porter matches such as `deploy` and `deployed`. It rejects false matches such as `spec` inside `respect`.
- Recall ranking now has a relative `bm25` floor that drops weak tails and a mild recency tie-break. A fresher session wins only a genuine near-tie. Required matched terms now scale with prompt length.

## [0.5.0] - 2026-06-14

### Added

- `subrosa search --fuzzy` adds substring and typo matching through a trigram index built on first use. Exact search and auto-recall still use the Porter index.

## [0.4.2] - 2026-06-13

### Added

- `subrosa init --claude-md` is an opt-in, idempotent command. It appends the memory-recall block to `CLAUDE.md`, so Claude searches the archive automatically.

## [0.4.1] - 2026-06-12

### Changed

- Performance work adds a fork-free hook wrapper, SQLite `mmap` and in-memory temp storage, zero-copy redaction, and a capped recall FTS union. The per-prompt hook runs well under 10 ms. Added `scripts/bench.sh`.

## [0.4.0] - 2026-06-12

### Added

- The PreCompact hook archives the conversation before compaction summarizes it. It also resets recall dedup so post-compact prompts can resurface relevant sessions.

### Changed

- FTS now uses the Porter stemmer in schema v2. Recall matches word forms such as `deploy`, `deployed`, and `deploying`. Identifiers still match exactly. Upgrades rebuild the index from source tables.

## [0.3.0] - 2026-06-12

### Changed

- Recall now requires an anchor-grade term and deduplicates per session. Index hook lines are capped. The session-start nudge is filtered from the archive. A `SIGPIPE` guard makes `subrosa search | head` exit cleanly.

## [0.2.1] - 2026-06-12

### Fixed

- Concurrent hooks no longer cause SQLite lock errors. Immediate transactions, read-only connections, and atomic retrying backups fix this.

## [0.2.0] - 2026-06-12

### Added

- Added prompt auto-recall, curated facts with byte-budgeted `MEMORY.md` generation, the `/subrosa:checkpoint` and `/subrosa:checkpoint-backlog` skills, the `subrosa` dashboard, `subrosa import`, and a pre-commit gate.

## [0.1.0] - 2026-06-12

### Added

- Initial release: Rust memory engine, local SQLite FTS5 transcript archive, Claude Code plugin wiring, plugin binary bootstrap, install script, release automation, and CI.

[0.25.1]: https://github.com/ij5a/subrosa/compare/v0.25.0...v0.25.1
[0.25.0]: https://github.com/ij5a/subrosa/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/ij5a/subrosa/compare/v0.23.0...v0.24.0
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
