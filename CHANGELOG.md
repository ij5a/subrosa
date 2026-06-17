# Changelog

All notable changes to subrosa are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
