# Contributing to subrosa

Thanks for thinking about contributing. subrosa is a Rust CLI and Claude Code plugin that gives Claude Code persistent, private memory — every session is archived locally and made searchable, and saving never spends tokens.

This guide covers how to set up, the checks your change has to pass, and the few rules that keep the project small and stable.

## Getting set up

```sh
git clone https://github.com/ij5a/subrosa.git
cd subrosa
mise install                          # installs the pinned Rust toolchain
git config core.hooksPath .githooks   # one-time: turns on the pre-commit checks
cargo build --locked
```

Always work against a throwaway data directory so you never touch your own archive. The `SUBROSA_DIR` and `SUBROSA_PROJECTS_DIR` environment variables point subrosa at test folders instead of `~/.claude/subrosa`:

```sh
SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo run -- init
SUBROSA_DIR=/tmp/x SUBROSA_PROJECTS_DIR=/tmp/x/projects cargo test --locked
```

## The verification gate

Every code change has to pass four checks before it is committed, pushed, or released. The pre-commit hook and CI run some of them for you; the rest you run by hand.

1. **Tests** — `cargo test --locked`. Runs the unit tests and the golden tests in `tests/`. A red test blocks the commit.
2. **Performance** — `scripts/bench.sh` (needs [`hyperfine`](https://github.com/sharkdp/hyperfine)). Measures the recall hot path, search, ingest, and startup. The latency numbers in the README are a promise, so a slowdown is a real bug. Run it before a push or release, and on any change to `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema.
3. **Security** — `cargo audit` (also a CI job) plus a read-through of your own diff. This repo is public, so do this before every push and release.
4. **Docs in sync** — update every markdown file your change affects (`README.md`, `docs/*.md`, `CLAUDE.md`, the skill docs). Numbers, flags, and limits in the docs must match the code. A code change that lands without its doc update is not finished.

The pre-commit hook (`.githooks/pre-commit`) runs `scripts/sweep.sh` (it looks for stray secrets and database files), then `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and the tests. CI runs the same set plus `cargo audit`.

## Golden tests are compatibility-critical

The stored-text, session-dump, `MEMORY.md`, and recall output formats are pinned byte-for-byte by golden tests in `tests/`. Archives made by older versions have to keep working, so these formats are not free to change.

If a golden test fails, it means a format changed. Treat that as a decision, not an accident: only update the golden file when you mean to change the format, and say why in the commit message. Never edit a golden file just to make a failing test pass.

## The 5-crate rule

subrosa depends on five crates: `clap`, `regex`, `rusqlite`, `serde`, `serde_json`. A single static binary with a small supply chain is part of what the project offers, so a new dependency needs a strong reason. Please open an issue to discuss before adding one.

## Commits and pull requests

Commit messages use the conventional-commit format — lowercase, one line, up to 120 characters. No AI footer or co-author line.

```
feat(recall): add match-centered snippets
fix(ingest): skip zero-byte transcripts
docs: refresh the performance table
chore: bump toolchain and refresh mise.lock
```

Pull request descriptions use three sections, in this order:

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

Use sentence case everywhere — headings, prose, and list items. Keep it short and direct.

## Pre-merge checklist

- [ ] `cargo test --locked` passes (including the golden tests)
- [ ] `scripts/bench.sh` run, if the change touches `recall.rs`, `search.rs`, `ingest.rs`, or the FTS schema
- [ ] `cargo audit` run and the diff reviewed for security
- [ ] Affected docs updated
- [ ] Any golden-file change is deliberate
