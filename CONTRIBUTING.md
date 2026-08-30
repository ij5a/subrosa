# Contributing to subrosa

subrosa is a Rust CLI and Claude Code plugin for private, persistent memory. It archives sessions locally, makes them searchable, and uses no tokens to save memory.

This guide covers setup, required checks, and project rules.

## Getting set up

```sh
git clone https://github.com/ij5a/subrosa.git
cd subrosa
mise install                          # installs the pinned Rust toolchain
git config core.hooksPath .githooks   # one-time: turns on the pre-commit checks
cargo build --locked
```

Use a throwaway data directory. `SUBROSA_DIR` and `SUBROSA_PROJECTS_DIR` point subrosa to test folders instead of `~/.claude/subrosa`:

```sh
SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init
SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo test --locked
```

## The verification gate

Every code change must pass 6 gates before commit, push, or release. The pre-commit hook and CI run some checks for you. Run the rest by hand.

1. **Tests:** Run `cargo test --locked`. This runs unit tests and golden tests in `tests/`. A failure blocks the commit.
2. **Performance:** Run `scripts/bench.sh` with [`hyperfine`](https://github.com/sharkdp/hyperfine). It measures recall, search, ingest, and startup. README latency numbers are promises. A slowdown is a bug. Run it before a push or release. Run it after changes to `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema.
3. **Token usage:** `scripts/bench.sh` also measures recall injection. It fails above the 220-token guard behind the about-180-token promise.
4. **Smoke:** Run `scripts/smoke.sh` with the built binary. It uses a throwaway directory and checks redaction, encrypted-mirror and restore paths, a fail-closed budget override, and hook exit 0.
5. **Security:** Run `cargo audit` and `/security-review` over the branch diff. CI also runs the audit. Run them before every push and release.
6. **Docs:** Update every affected Markdown file, including `README.md`, `docs/*.md`, `CLAUDE.md`, and skill docs. Make sure documented numbers, flags, and limits match the code. A code change without its doc update is not finished.

The pre-commit hook (`.githooks/pre-commit`) runs `scripts/sweep.sh`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and the tests. `scripts/sweep.sh` checks for secrets and database files. CI runs the same checks plus `cargo audit`.

## Golden tests are compatibility-critical

Golden tests in `tests/` pin the stored-text, session-dump, `MEMORY.md`, and recall formats byte-for-byte. Older archives must keep working, so these formats cannot change freely.

If a golden test fails, a format changed. Update the golden file only when you intend that change. Explain why in the commit message. Never edit a golden file only to pass a test.

## The 11-crate rule

subrosa depends on 11 direct crates: `clap`, `regex`, `rusqlite`, `serde`, `serde_json`, `chacha20poly1305`, `argon2`, `candle-core`, `candle-nn`, `candle-transformers`, and `sha2`. A small supply chain and one static binary are part of the project. A new dependency needs a strong reason. Open an issue before adding one.

## Commits and pull requests

Use the conventional-commit format for commit messages. Use lowercase, one line, and at most 120 characters. Add no AI footer or co-author line.

```
feat(recall): add match-centered snippets
fix(ingest): skip zero-byte transcripts
docs: refresh the performance table
chore: bump toolchain and refresh mise.lock
```

Use these three pull request sections in this order:

```markdown
## Why
[One short paragraph: the problem and the context.]

## What changed?
1. First change
2. Second change

## How to test?
1. First step
2. Second step
```

Use sentence case for headings, prose, and list items. Keep it short and direct.

## Pre-merge checklist

- [ ] `cargo test --locked` passes, including the golden tests
- [ ] Run `scripts/bench.sh` if the change touches `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema
- [ ] Run `scripts/smoke.sh` with the built binary
- [ ] Run `cargo audit` and review the diff for security
- [ ] Update affected docs
- [ ] Make every golden-file change deliberate
