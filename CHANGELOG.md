# Changelog

All notable changes to subrosa are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
